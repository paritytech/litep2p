// Copyright 2023 litep2p developers
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use crate::{
    codec::ProtocolCodec,
    crypto::{ed25519::Keypair, PublicKey},
    error::{AddressError, Error},
    executor::Executor,
    protocol::{InnerTransportEvent, TransportService},
    transport::{
        manager::{
            address::{AddressRecord, AddressStore},
            handle::InnerTransportManagerCommand,
            types::{PeerContext, PeerState},
        },
        Endpoint, Transport, TransportEvent,
    },
    types::{protocol::ProtocolName, ConnectionId},
    BandwidthSink, PeerId,
};

use futures::{Stream, StreamExt};
use indexmap::IndexMap;
use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;
use parking_lot::RwLock;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

pub use handle::{TransportHandle, TransportManagerHandle};
pub use types::SupportedTransport;

mod address;
mod types;

pub(crate) mod handle;

// TODO: store `Multiaddr` in `Arc`
// TODO: limit number of peers and addresses
// TODO: rename constants
// TODO: add lots of documentation

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::transport-manager";

/// Score for a working address.
const SCORE_DIAL_SUCCESS: i32 = 100i32;

/// Score for a non-working address.
const SCORE_DIAL_FAILURE: i32 = -100i32;

/// [`TransportManager`] configuration.
#[derive(Debug)]
pub struct Config {
    /// Maximum connections.
    pub max_connections: usize,
}

/// [`TransportManager`] events.
#[derive(Debug)]
pub enum TransportManagerEvent {
    /// Connection established to remote peer.
    ConnectionEstablished {
        /// Peer ID.
        peer: PeerId,

        /// Connection ID.
        connection: ConnectionId,

        /// Endpoint.
        endpoint: Endpoint,
    },

    /// Connection closed to remote peer.
    ConnectionClosed {
        /// Peer ID.
        peer: PeerId,

        /// Connection ID.
        connection: ConnectionId,
    },

    /// Failed to dial remote peer.
    DialFailure {
        /// Connection ID.
        connection: ConnectionId,

        /// Dialed address.
        address: Multiaddr,

        /// Error.
        error: Error,
    },
}

// Protocol context.
#[derive(Debug, Clone)]
pub struct ProtocolContext {
    /// Codec used by the protocol.
    pub codec: ProtocolCodec,

    /// TX channel for sending events to protocol.
    pub tx: Sender<InnerTransportEvent>,

    /// Fallback names for the protocol.
    pub fallback_names: Vec<ProtocolName>,
}

impl ProtocolContext {
    /// Create new [`ProtocolContext`].
    fn new(
        codec: ProtocolCodec,
        tx: Sender<InnerTransportEvent>,
        fallback_names: Vec<ProtocolName>,
    ) -> Self {
        Self {
            tx,
            codec,
            fallback_names,
        }
    }
}

/// Transport context for enabled transports.
struct TransportContext {
    /// Polling index.
    index: usize,

    /// Registered transports.
    transports: IndexMap<SupportedTransport, Box<dyn Transport<Item = TransportEvent>>>,
}

impl TransportContext {
    /// Create new [`TransportContext`].
    pub fn new() -> Self {
        Self {
            index: 0usize,
            transports: IndexMap::new(),
        }
    }

    /// Get an iterator of supported transports.
    pub fn keys(&self) -> impl Iterator<Item = &SupportedTransport> {
        self.transports.keys()
    }

    /// Get mutable access to transport.
    pub fn get_mut(
        &mut self,
        key: &SupportedTransport,
    ) -> Option<&mut Box<dyn Transport<Item = TransportEvent>>> {
        self.transports.get_mut(key)
    }

    /// Register `transport` to `TransportContext`.
    pub fn register_transport(
        &mut self,
        name: SupportedTransport,
        transport: Box<dyn Transport<Item = TransportEvent>>,
    ) {
        assert!(self.transports.insert(name, transport).is_none());
    }
}

impl Stream for TransportContext {
    type Item = TransportEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let len = match self.transports.len() {
            0 => return Poll::Ready(None),
            len => len,
        };
        let start_index = self.index;

        loop {
            let index = self.index % len;
            self.index += 1;

            let (_, stream) = self.transports.get_index_mut(index).expect("transport to exist");
            match stream.poll_next_unpin(cx) {
                Poll::Pending => {}
                event => return event,
            }

            if self.index == start_index + len {
                break Poll::Pending;
            }
        }
    }
}

/// Litep2p connection manager.
pub struct TransportManager {
    /// Local peer ID.
    local_peer_id: PeerId,

    /// Keypair.
    keypair: Keypair,

    /// Bandwidth sink.
    bandwidth_sink: BandwidthSink,

    /// Installed protocols.
    protocols: HashMap<ProtocolName, ProtocolContext>,

    /// All names (main and fallback(s)) of the installed protocols.
    protocol_names: HashSet<ProtocolName>,

    /// Listen addresses.
    listen_addresses: HashSet<Multiaddr>,

    /// Next connection ID.
    next_connection_id: Arc<AtomicUsize>,

    /// Next substream ID.
    next_substream_id: Arc<AtomicUsize>,

    /// Installed transports.
    transports: TransportContext,

    /// Peers
    peers: Arc<RwLock<HashMap<PeerId, PeerContext>>>,

    /// Handle to [`TransportManager`].
    transport_manager_handle: TransportManagerHandle,

    /// RX channel for receiving events from installed transports.
    event_rx: Receiver<TransportManagerEvent>,

    /// RX channel for receiving commands from installed protocols.
    cmd_rx: Receiver<InnerTransportManagerCommand>,

    /// TX channel for transport events that is given to installed transports.
    event_tx: Sender<TransportManagerEvent>,

    /// Pending connections.
    pending_connections: HashMap<ConnectionId, PeerId>,
}

impl TransportManager {
    /// Create new [`TransportManager`].
    // TODO: don't return handle here
    pub fn new(
        keypair: Keypair,
        supported_transports: HashSet<SupportedTransport>,
        bandwidth_sink: BandwidthSink,
    ) -> (Self, TransportManagerHandle) {
        let local_peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let peers = Arc::new(RwLock::new(HashMap::new()));
        let (cmd_tx, cmd_rx) = channel(256);
        let (event_tx, event_rx) = channel(256);
        let handle = TransportManagerHandle::new(peers.clone(), cmd_tx, supported_transports);

        (
            Self {
                peers,
                cmd_rx,
                keypair,
                event_tx,
                event_rx,
                local_peer_id,
                bandwidth_sink,
                protocols: HashMap::new(),
                transports: TransportContext::new(),
                protocol_names: HashSet::new(),
                listen_addresses: HashSet::new(),
                transport_manager_handle: handle.clone(),
                pending_connections: HashMap::new(),
                next_substream_id: Arc::new(AtomicUsize::new(0usize)),
                next_connection_id: Arc::new(AtomicUsize::new(0usize)),
            },
            handle,
        )
    }

    /// Get iterator to installed protocols.
    pub fn protocols(&self) -> impl Iterator<Item = &ProtocolName> {
        self.protocols.keys()
    }

    /// Get iterator to installed transports
    pub fn installed_transports(&self) -> impl Iterator<Item = &SupportedTransport> {
        self.transports.keys()
    }

    /// Get next connection ID.
    fn next_connection_id(&mut self) -> ConnectionId {
        let connection_id = self.next_connection_id.fetch_add(1usize, Ordering::Relaxed);

        ConnectionId::from(connection_id)
    }

    /// Register protocol to the [`TransportManager`].
    ///
    /// This allocates new context for the protocol and returns a handle
    /// which the protocol can use the interact with the transport subsystem.
    pub fn register_protocol(
        &mut self,
        protocol: ProtocolName,
        fallback_names: Vec<ProtocolName>,
        codec: ProtocolCodec,
    ) -> TransportService {
        assert!(!self.protocol_names.contains(&protocol));

        for fallback in &fallback_names {
            if self.protocol_names.contains(fallback) {
                panic!("duplicate fallback protocol given: {fallback:?}");
            }
        }

        let (service, sender) = TransportService::new(
            self.local_peer_id,
            protocol.clone(),
            fallback_names.clone(),
            self.next_substream_id.clone(),
            self.transport_manager_handle.clone(),
        );

        self.protocols.insert(
            protocol.clone(),
            ProtocolContext::new(codec, sender, fallback_names.clone()),
        );
        self.protocol_names.insert(protocol);
        self.protocol_names.extend(fallback_names);

        service
    }

    /// Acquire `TransportHandle`.
    pub fn transport_handle(&self, executor: Arc<dyn Executor>) -> TransportHandle {
        TransportHandle {
            tx: self.event_tx.clone(),
            executor,
            keypair: self.keypair.clone(),
            protocols: self.protocols.clone(),
            bandwidth_sink: self.bandwidth_sink.clone(),
            protocol_names: self.protocol_names.iter().cloned().collect(),
            next_substream_id: self.next_substream_id.clone(),
            next_connection_id: self.next_connection_id.clone(),
        }
    }

    /// Register transport to `TransportManager`.
    pub(crate) fn register_transport(
        &mut self,
        name: SupportedTransport,
        transport: Box<dyn Transport<Item = TransportEvent>>,
    ) {
        tracing::debug!(target: LOG_TARGET, transport = ?name, "register transport");

        self.transports.register_transport(name, transport);
    }

    /// Register local listen address.
    pub fn register_listen_address(&mut self, address: Multiaddr) {
        self.listen_addresses.insert(address.clone());
        self.listen_addresses.insert(address.with(Protocol::P2p(
            Multihash::from_bytes(&self.local_peer_id.to_bytes()).unwrap(),
        )));
    }

    /// Add one or more known addresses for `peer`.
    pub fn add_known_address(
        &mut self,
        peer: PeerId,
        address: impl Iterator<Item = Multiaddr>,
    ) -> usize {
        self.transport_manager_handle.add_known_address(&peer, address)
    }

    /// Dial peer using `PeerId`.
    ///
    /// Returns an error if the peer is unknown or the peer is already connected.
    pub async fn dial(&mut self, peer: &PeerId) -> crate::Result<()> {
        if peer == &self.local_peer_id {
            return Err(Error::TriedToDialSelf);
        }

        let address = match self.peers.write().get_mut(&peer) {
            None => return Err(Error::PeerDoesntExist(*peer)),
            Some(PeerContext {
                state: PeerState::Connected { .. },
                ..
            }) => return Err(Error::AlreadyConnected),
            Some(PeerContext {
                state: PeerState::Dialing { .. },
                ..
            }) => return Ok(()),
            Some(PeerContext {
                state: PeerState::Disconnected { dial_record },
                addresses,
                ..
            }) => {
                if dial_record.is_some() {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        ?dial_record,
                        "peer is aready being dialed while",
                    );
                    return Ok(());
                }

                let address = addresses.pop().ok_or(Error::NoAddressAvailable(*peer))?;

                // TODO: start timer for the address and after 2 minutes reset peer score to try
                // again TODO: introduce new error type `Chilled` which means that
                // the peer shouldn't be considered dead but currently it's not
                // possible to dial it

                // TODO: this is not good
                // if address.score <= -200 {
                //     return Err(Error::NoAddressAvailable(*peer));
                // }

                address
            }
        };

        self.dial_address_inner(address).await
    }

    /// Dial peer using `Multiaddr`.
    ///
    /// Returns an error if address it not valid.
    pub async fn dial_address(&mut self, address: Multiaddr) -> crate::Result<()> {
        match AddressRecord::from_multiaddr(address) {
            Some(record) => self.dial_address_inner(record).await,
            None => return Err(Error::AddressError(AddressError::PeerIdMissing)),
        }
    }

    /// Dial peer using `AddressRecord` which holds the inner `Multiaddr` and score for the address.
    ///
    /// Returns an error if address it not valid.
    async fn dial_address_inner(&mut self, mut record: AddressRecord) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, address = ?record.address(), "dial remote peer");

        if self.listen_addresses.contains(record.as_ref()) {
            return Err(Error::TriedToDialSelf);
        }

        let mut protocol_stack = record.as_ref().iter();
        match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(record.address().clone()))?
        {
            Protocol::Ip4(_) | Protocol::Ip6(_) => {}
            Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_) => {}
            transport => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?transport,
                    "invalid transport, expected `ip4`/`ip6`"
                );
                return Err(Error::TransportNotSupported(record.address().clone()));
            }
        };

        let (supported_transport, remote_peer_id) = match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(record.address().clone()))?
        {
            Protocol::Tcp(_) => match protocol_stack.next() {
                Some(Protocol::Ws(_)) | Some(Protocol::Wss(_)) => match protocol_stack.next() {
                    Some(Protocol::P2p(hash)) => (
                        SupportedTransport::WebSocket,
                        PeerId::from_multihash(hash).map_err(|_| Error::InvalidData)?,
                    ),
                    _ => {
                        tracing::debug!(target: LOG_TARGET, address = ?record.address(), "peer id missing");
                        return Err(Error::AddressError(AddressError::PeerIdMissing));
                    }
                },
                Some(Protocol::P2p(hash)) => (
                    SupportedTransport::Tcp,
                    PeerId::from_multihash(hash).map_err(|_| Error::InvalidData)?,
                ),
                _ => {
                    tracing::debug!(target: LOG_TARGET, address = ?record.address(), "peer id missing");
                    return Err(Error::AddressError(AddressError::PeerIdMissing));
                }
            },
            Protocol::Udp(_) => match protocol_stack
                .next()
                .ok_or_else(|| Error::TransportNotSupported(record.address().clone()))?
            {
                Protocol::QuicV1 => match protocol_stack.next() {
                    Some(Protocol::P2p(hash)) => (
                        SupportedTransport::Quic,
                        PeerId::from_multihash(hash).map_err(|_| Error::InvalidData)?,
                    ),
                    _ => {
                        tracing::debug!(target: LOG_TARGET, address = ?record.address(), "peer id missing");
                        return Err(Error::AddressError(AddressError::PeerIdMissing));
                    }
                },
                _ => {
                    tracing::debug!(target: LOG_TARGET, address = ?record.address(), "expected `quic-v1`");
                    return Err(Error::TransportNotSupported(record.address().clone()));
                }
            },
            protocol => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `tcp`"
                );

                return Err(Error::TransportNotSupported(record.address().clone()));
            }
        };

        // set connection id for the address record and put peer into `Dialing` state
        let connection_id = self.next_connection_id();
        record.set_connection_id(connection_id);

        {
            let mut peers = self.peers.write();

            match peers.get_mut(&remote_peer_id) {
                None => {
                    drop(peers);
                    self.peers.write().insert(
                        remote_peer_id,
                        PeerContext {
                            state: PeerState::Dialing {
                                record: record.clone(),
                            },
                            addresses: AddressStore::new(),
                            secondary_connection: None,
                        },
                    );
                }
                Some(PeerContext {
                    state: PeerState::Dialing { .. } | PeerState::Connected { .. },
                    ..
                }) => return Ok(()),
                Some(PeerContext { ref mut state, .. }) => {
                    // TODO: verify that the address is not in `addresses` already
                    // addresses.insert(address.clone());
                    *state = PeerState::Dialing {
                        record: record.clone(),
                    };
                }
            }
        }

        self.transports
            .get_mut(&supported_transport)
            .ok_or(Error::TransportNotSupported(record.address().clone()))?
            .dial(connection_id, record.address().clone())?;
        self.pending_connections.insert(connection_id, remote_peer_id);

        Ok(())
    }

    /// Handle dial failure.
    fn on_dial_failure(
        &mut self,
        connection_id: &ConnectionId,
        address: &Multiaddr,
        _error: &Error,
    ) -> crate::Result<bool> {
        let peer = self.pending_connections.remove(&connection_id).ok_or_else(|| {
            tracing::error!(target: LOG_TARGET, "dial failed for a connection that doesn't exist");
            debug_assert!(false);

            Error::InvalidState
        })?;

        let mut peers = self.peers.write();
        let context = peers.get_mut(&peer).ok_or_else(|| {
            tracing::error!(
                target: LOG_TARGET,
                ?peer,
                ?connection_id,
                "dial failed for a peer that doens't exist",
            );
            debug_assert!(false);

            Error::InvalidState
        })?;

        match std::mem::replace(
            &mut context.state,
            PeerState::Disconnected { dial_record: None },
        ) {
            PeerState::Dialing { ref mut record } => {
                debug_assert_eq!(record.connection_id(), &Some(*connection_id));

                record.update_score(SCORE_DIAL_FAILURE);
                context.addresses.insert(record.clone());

                context.state = PeerState::Disconnected { dial_record: None };
                Ok(true)
            }
            PeerState::Connected {
                record,
                dial_record: Some(mut dial_record),
            } => {
                debug_assert_ne!(record.address(), address);
                debug_assert_eq!(dial_record.address(), address);

                dial_record.update_score(SCORE_DIAL_FAILURE);
                context.addresses.insert(dial_record);

                context.state = PeerState::Connected {
                    record,
                    dial_record: None,
                };
                Ok(true)
            }
            PeerState::Disconnected {
                dial_record: Some(mut dial_record),
            } => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?connection_id,
                    ?address,
                    ?dial_record,
                    "dial failed for a disconnected peer",
                );
                debug_assert_eq!(dial_record.address(), address);

                dial_record.update_score(SCORE_DIAL_FAILURE);
                context.addresses.insert(dial_record);

                Ok(true)
            }
            state => {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?peer,
                    ?connection_id,
                    ?state,
                    ?address,
                    "invalid state for dial failure",
                );
                context.state = state;

                debug_assert!(false);
                Ok(true)
            }
        }
    }

    /// Handle established connection.
    ///
    /// TODO: documentation
    /// TODO: should secondary connection be reported at all?
    fn on_connection_established(
        &mut self,
        peer: &PeerId,
        connection_id: &ConnectionId,
        endpoint: &Endpoint,
    ) -> crate::Result<bool> {
        if let Some(dialed_peer) = self.pending_connections.remove(&connection_id) {
            if &dialed_peer != peer {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?dialed_peer,
                    ?peer,
                    "peer ids do not match but transport was supposed to reject connection"
                );
                debug_assert!(false)
            }
        };

        let mut peers = self.peers.write();
        match peers.get_mut(&peer) {
            Some(context) => match context.state {
                PeerState::Connected { .. } => match context.secondary_connection {
                    Some(_) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?peer,
                            ?connection_id,
                            ?endpoint,
                            "secondary connection already exists, ignoring connection",
                        );
                        context
                            .addresses
                            .insert_with_score(endpoint.address().clone(), SCORE_DIAL_SUCCESS);

                        return Ok(false);
                    }
                    None => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?peer,
                            ?connection_id,
                            address = ?endpoint.address(),
                            "secondary connection",
                        );

                        context.secondary_connection = Some(AddressRecord::new(
                            peer,
                            endpoint.address().clone(),
                            SCORE_DIAL_SUCCESS,
                            Some(*connection_id),
                        ));
                    }
                },
                PeerState::Dialing { ref record, .. } => {
                    match record.connection_id() == &Some(*connection_id) {
                        true => {
                            tracing::trace!(
                                target: LOG_TARGET,
                                ?peer,
                                ?connection_id,
                                ?endpoint,
                                "connection opened to remote",
                            );

                            context.state = PeerState::Connected {
                                record: record.clone(),
                                dial_record: None,
                            };
                        }
                        false => {
                            tracing::trace!(
                                target: LOG_TARGET,
                                ?peer,
                                ?connection_id,
                                ?endpoint,
                                "connection opened by remote while local node was dialing",
                            );

                            context.state = PeerState::Connected {
                                record: AddressRecord::new(
                                    peer,
                                    endpoint.address().clone(),
                                    SCORE_DIAL_SUCCESS,
                                    Some(*connection_id),
                                ),
                                dial_record: Some(record.clone()),
                            };
                        }
                    }
                }
                PeerState::Disconnected {
                    ref mut dial_record,
                } => {
                    tracing::trace!(
                        target: LOG_TARGET,
                        ?peer,
                        ?connection_id,
                        ?endpoint,
                        ?dial_record,
                        "connection opened by remote or delayed dial succeeded",
                    );

                    let (record, dial_record) = match dial_record.take() {
                        Some(dial_record) =>
                            if dial_record.address() == endpoint.address() {
                                (dial_record, None)
                            } else {
                                (
                                    AddressRecord::new(
                                        peer,
                                        endpoint.address().clone(),
                                        SCORE_DIAL_SUCCESS,
                                        Some(*connection_id),
                                    ),
                                    Some(dial_record),
                                )
                            },
                        None => (
                            AddressRecord::new(
                                peer,
                                endpoint.address().clone(),
                                SCORE_DIAL_SUCCESS,
                                Some(*connection_id),
                            ),
                            None,
                        ),
                    };

                    context.state = PeerState::Connected {
                        record,
                        dial_record,
                    };
                }
            },
            None => {
                peers.insert(
                    *peer,
                    PeerContext {
                        state: PeerState::Connected {
                            record: AddressRecord::new(
                                peer,
                                endpoint.address().clone(),
                                SCORE_DIAL_SUCCESS,
                                Some(*connection_id),
                            ),
                            dial_record: None,
                        },
                        addresses: AddressStore::new(),
                        secondary_connection: None,
                    },
                );
            }
        }

        Ok(true)
    }

    /// Handle closed connection.
    ///
    /// Returns `bool` which indicates whether the event should be returned or not.
    fn on_connection_closed(
        &mut self,
        peer: &PeerId,
        connection_id: &ConnectionId,
    ) -> crate::Result<bool> {
        let mut peers = self.peers.write();
        let Some(context) = peers.get_mut(peer) else {
            tracing::warn!(
                target: LOG_TARGET,
                ?peer,
                ?connection_id,
                "cannot handle closed connection: peer doesn't exist",
            );
            debug_assert!(false);
            return Err(Error::PeerDoesntExist(*peer));
        };

        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?connection_id,
            "connection closed",
        );

        match std::mem::replace(
            &mut context.state,
            PeerState::Disconnected { dial_record: None },
        ) {
            PeerState::Connected {
                record,
                dial_record: actual_dial_record,
            } => match record.connection_id() == &Some(*connection_id) {
                // primary connection was closed
                //
                // if secondary connection exists, switch to using it while keeping peer in
                // `Connected` state and if there's only one connection, set peer
                // state to `Disconnected`
                true => match context.secondary_connection.take() {
                    None => {
                        context.addresses.insert(record);
                        context.state = PeerState::Disconnected {
                            dial_record: actual_dial_record,
                        };
                        return Ok(true);
                    }
                    Some(secondary_connection) => {
                        context.addresses.insert(record);
                        context.state = PeerState::Connected {
                            record: secondary_connection,
                            dial_record: actual_dial_record,
                        };
                        return Ok(false);
                    }
                },
                // secondary connection was closed
                false => match context.secondary_connection.take() {
                    Some(secondary_connection) => {
                        if secondary_connection.connection_id() != &Some(*connection_id) {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?connection_id,
                                "unknown connection was closed, potentially ignored tertiary connection",
                            );

                            context.secondary_connection = Some(secondary_connection);
                            context.state = PeerState::Connected {
                                record,
                                dial_record: actual_dial_record,
                            };

                            return Ok(false);
                        }

                        tracing::trace!(
                            target: LOG_TARGET,
                            ?peer,
                            ?connection_id,
                            "secondary connection closed",
                        );

                        context.addresses.insert(secondary_connection);
                        context.state = PeerState::Connected {
                            record,
                            dial_record: actual_dial_record,
                        };
                        return Ok(false);
                    }
                    None => {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?peer,
                            ?connection_id,
                            "non-primary connection was closed but secondary connection doesn't exist",
                        );

                        debug_assert!(false);
                        return Err(Error::InvalidState);
                    }
                },
            },
            PeerState::Disconnected { dial_record } => match context.secondary_connection.take() {
                Some(record) => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        ?peer,
                        ?connection_id,
                        ?record,
                        ?dial_record,
                        "peer is disconnected but secondary connection exists",
                    );

                    debug_assert!(false);
                    context.state = PeerState::Disconnected { dial_record };
                    return Err(Error::InvalidState);
                }
                None => {
                    context.state = PeerState::Disconnected { dial_record };
                    Ok(true)
                }
            },
            state => {
                tracing::warn!(target: LOG_TARGET, ?peer, ?connection_id, ?state, "invalid state for a closed connection");
                debug_assert!(false);
                return Err(Error::InvalidState);
            }
        }
    }

    /// Handle dial failure.
    async fn on_dial_failure_new(
        &mut self,
        connection_id: ConnectionId,
        address: Multiaddr,
        error: Error,
    ) -> Option<TransportManagerEvent> {
        match self.on_dial_failure(&connection_id, &address, &error) {
            Ok(true) => {
                match address.iter().last() {
                    Some(Protocol::P2p(hash)) => match PeerId::from_multihash(hash) {
                        Ok(peer) =>
                            for (_, context) in &self.protocols {
                                let _ = context
                                    .tx
                                    .send(InnerTransportEvent::DialFailure {
                                        peer,
                                        address: address.clone(),
                                    })
                                    .await;
                            },
                        Err(error) => {
                            tracing::warn!(target: LOG_TARGET, ?address, ?error, "failed to parse `PeerId` from `Multiaddr`");
                            debug_assert!(false);
                        }
                    },
                    _ => {
                        tracing::warn!(target: LOG_TARGET, ?address, "address doesn't contain `PeerId`");
                        debug_assert!(false);
                    }
                }

                Some(TransportManagerEvent::DialFailure {
                    connection: connection_id,
                    address,
                    error,
                })
            }
            Err(error) => {
                match address.iter().last() {
                    Some(Protocol::P2p(hash)) => match PeerId::from_multihash(hash) {
                        Ok(peer) =>
                            for (_, context) in &self.protocols {
                                let _ = context
                                    .tx
                                    .send(InnerTransportEvent::DialFailure {
                                        peer,
                                        address: address.clone(),
                                    })
                                    .await;
                            },
                        Err(error) => {
                            tracing::warn!(target: LOG_TARGET, ?address, ?error, "failed to parse `PeerId` from `Multiaddr`");
                            debug_assert!(false);
                        }
                    },
                    _ => {
                        tracing::warn!(target: LOG_TARGET, ?address, "address doesn't contain `PeerId`");
                        debug_assert!(false);
                    }
                }
                Some(TransportManagerEvent::DialFailure {
                    connection: connection_id,
                    address,
                    error,
                })
            }
            _ => None,
        }
    }

    /// Poll next event from [`TransportManager`].
    pub async fn next(&mut self) -> Option<TransportManagerEvent> {
        loop {
            tokio::select! {
                event = self.event_rx.recv() => {
                    let inner = event?;
                    let result = match &inner {
                        TransportManagerEvent::DialFailure {
                            address,
                            connection,
                            error,
                        } => self.on_dial_failure(&connection, &address, &error),
                        TransportManagerEvent::ConnectionEstablished {
                            peer,
                            connection,
                            endpoint,
                        } => self.on_connection_established(&peer, &connection, &endpoint),
                        TransportManagerEvent::ConnectionClosed {
                            peer,
                            connection: connection_id,
                        } => self.on_connection_closed(&peer, &connection_id),
                    };

                    match result {
                        Ok(true) => return Some(inner),
                        Err(error) => {
                            tracing::debug!(target: LOG_TARGET, ?error, "failed to handle transport event");
                            return Some(inner)
                        }
                        _ => {}
                    }
                },
                command = self.cmd_rx.recv() => match command? {
                    InnerTransportManagerCommand::DialPeer { peer } => {
                        if let Err(error) = self.dial(&peer).await {
                            tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to dial peer")
                        }
                    }
                    InnerTransportManagerCommand::DialAddress { address } => {
                        if let Err(error) = self.dial_address(address).await {
                            tracing::debug!(target: LOG_TARGET, ?error, "failed to dial peer")
                        }
                    }
                },
                event = self.transports.next() => match event {
                    Some(TransportEvent::DialFailure { connection_id, address, error }) => {
                        if let Some(event) = self.on_dial_failure_new(connection_id, address, error).await {
                            return Some(event);
                        }
                    }
                    None => panic!("transports exited"),
                    _ => panic!("event not supported"),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::ed25519::Keypair, executor::DefaultExecutor, transport::dummy::DummyTransport,
    };
    use std::{
        net::{Ipv4Addr, Ipv6Addr},
        sync::Arc,
    };

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn duplicate_protocol() {
        let sink = BandwidthSink::new();
        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), sink);

        manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
        manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
    }

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn fallback_protocol_as_duplicate_main_protocol() {
        let sink = BandwidthSink::new();
        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), sink);

        manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
        manager.register_protocol(
            ProtocolName::from("/notif/2"),
            vec![
                ProtocolName::from("/notif/2/new"),
                ProtocolName::from("/notif/1"),
            ],
            ProtocolCodec::UnsignedVarint(None),
        );
    }

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn duplicate_fallback_protocol() {
        let sink = BandwidthSink::new();
        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), sink);

        manager.register_protocol(
            ProtocolName::from("/notif/1"),
            vec![
                ProtocolName::from("/notif/1/new"),
                ProtocolName::from("/notif/1"),
            ],
            ProtocolCodec::UnsignedVarint(None),
        );
        manager.register_protocol(
            ProtocolName::from("/notif/2"),
            vec![
                ProtocolName::from("/notif/2/new"),
                ProtocolName::from("/notif/1/new"),
            ],
            ProtocolCodec::UnsignedVarint(None),
        );
    }

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn duplicate_transport() {
        let sink = BandwidthSink::new();
        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), sink);

        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport {}));
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport {}));
    }

    #[tokio::test]
    async fn tried_to_self_using_peer_id() {
        let keypair = Keypair::generate();
        let local_peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let sink = BandwidthSink::new();
        let (mut manager, _handle) = TransportManager::new(keypair, HashSet::new(), sink);

        assert!(manager.dial(&local_peer_id).await.is_err());
    }

    #[tokio::test]
    async fn try_to_dial_over_disabled_transport() {
        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(
            SupportedTransport::Tcp,
            Box::new(crate::transport::dummy::DummyTransport {}),
        );

        let address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Udp(8888))
            .with(Protocol::QuicV1)
            .with(Protocol::P2p(
                Multihash::from_bytes(&PeerId::random().to_bytes()).unwrap(),
            ));

        assert!(std::matches!(
            manager.dial_address(address).await,
            Err(Error::TransportNotSupported(_))
        ));
    }

    #[tokio::test]
    async fn successful_dial_reported_to_transport_manager() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());
        let mut handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(
            SupportedTransport::Tcp,
            Box::new(crate::transport::dummy::DummyTransport {}),
        );

        let peer = PeerId::random();
        let dial_address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));

        assert!(manager.dial_address(dial_address.clone()).await.is_ok());
        assert!(!manager.pending_connections.is_empty());

        handle
            ._report_connection_established(
                ConnectionId::from(0usize),
                peer,
                Endpoint::dialer(dial_address, ConnectionId::from(0usize)),
            )
            .await;
    }

    #[tokio::test]
    async fn try_to_dial_same_peer_twice() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(
            SupportedTransport::Tcp,
            Box::new(crate::transport::dummy::DummyTransport {}),
        );

        let peer = PeerId::random();
        let dial_address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));

        assert!(manager.dial_address(dial_address.clone()).await.is_ok());
        assert_eq!(manager.pending_connections.len(), 1);

        assert!(manager.dial_address(dial_address.clone()).await.is_ok());
        assert_eq!(manager.pending_connections.len(), 1);
    }

    #[tokio::test]
    async fn try_to_dial_same_peer_twice_diffrent_address() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(
            SupportedTransport::Tcp,
            Box::new(crate::transport::dummy::DummyTransport {}),
        );

        let peer = PeerId::random();

        assert!(manager
            .dial_address(
                Multiaddr::empty()
                    .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
                    .with(Protocol::Tcp(8888))
                    .with(Protocol::P2p(
                        Multihash::from_bytes(&peer.to_bytes()).unwrap(),
                    ))
            )
            .await
            .is_ok());
        assert_eq!(manager.pending_connections.len(), 1);

        assert!(manager
            .dial_address(
                Multiaddr::empty()
                    .with(Protocol::Ip6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)))
                    .with(Protocol::Tcp(8888))
                    .with(Protocol::P2p(
                        Multihash::from_bytes(&peer.to_bytes()).unwrap(),
                    ))
            )
            .await
            .is_ok());
        assert_eq!(manager.pending_connections.len(), 1);
    }

    #[tokio::test]
    async fn dial_non_existent_peer() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(
            SupportedTransport::Tcp,
            Box::new(crate::transport::dummy::DummyTransport {}),
        );

        assert!(manager.dial(&PeerId::random()).await.is_err());
    }

    #[tokio::test]
    async fn dial_non_peer_with_no_known_addresses() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(
            SupportedTransport::Tcp,
            Box::new(crate::transport::dummy::DummyTransport {}),
        );

        let peer = PeerId::random();
        manager.peers.write().insert(
            peer,
            PeerContext {
                state: PeerState::Disconnected { dial_record: None },
                addresses: AddressStore::new(),
                secondary_connection: None,
            },
        );

        assert!(manager.dial(&peer).await.is_err());
    }

    #[tokio::test]
    async fn check_supported_transport_when_adding_known_address() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (_manager, handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::from_iter([SupportedTransport::Tcp, SupportedTransport::Quic]),
            BandwidthSink::new(),
        );

        // ipv6
        let address = Multiaddr::empty()
            .with(Protocol::Ip6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&PeerId::random().to_bytes()).unwrap(),
            ));
        assert!(handle.supported_transport(&address));

        // ipv4
        let address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&PeerId::random().to_bytes()).unwrap(),
            ));
        assert!(handle.supported_transport(&address));

        // quic
        let address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Udp(8888))
            .with(Protocol::QuicV1)
            .with(Protocol::P2p(
                Multihash::from_bytes(&PeerId::random().to_bytes()).unwrap(),
            ));
        assert!(handle.supported_transport(&address));

        // websocket
        let address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::Ws(std::borrow::Cow::Owned("/".to_string())));
        assert!(!handle.supported_transport(&address));

        // websocket secure
        let address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::Wss(std::borrow::Cow::Owned("/".to_string())));
        assert!(!handle.supported_transport(&address));
    }

    // local node tried to dial a node and it failed but in the mean
    // time the remote node dialed local node and that succeeded.
    #[tokio::test]
    async fn on_dial_failure_already_connected() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(
            SupportedTransport::Tcp,
            Box::new(crate::transport::dummy::DummyTransport {}),
        );

        let peer = PeerId::random();
        let dial_address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));
        let connect_address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(192, 168, 1, 173)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));
        assert!(manager.dial_address(dial_address.clone()).await.is_ok());
        assert_eq!(manager.pending_connections.len(), 1);

        match &manager.peers.read().get(&peer).unwrap().state {
            PeerState::Dialing { record } => {
                assert_eq!(record.address(), &dial_address);
            }
            state => panic!("invalid state for peer: {state:?}"),
        }

        // remote peer connected to local node from a different address that was dialed
        manager
            .on_connection_established(
                &peer,
                &ConnectionId::from(1usize),
                &Endpoint::dialer(connect_address, ConnectionId::from(1usize)),
            )
            .unwrap();

        // dialing the peer failed
        manager
            .on_dial_failure(
                &ConnectionId::from(0usize),
                &dial_address,
                &Error::Disconnected,
            )
            .unwrap();

        let peers = manager.peers.read();
        let peer = peers.get(&peer).unwrap();

        match &peer.state {
            PeerState::Connected { dial_record, .. } => {
                assert!(dial_record.is_none());
                assert!(peer.addresses.contains(&dial_address));
            }
            state => panic!("invalid state: {state:?}"),
        }
    }

    // local node tried to dial a node and it failed but in the mean
    // time the remote node dialed local node and that succeeded.
    //
    // while the dial was still in progresss, the remote node disconnected after which
    // the dial failure was reported.
    #[tokio::test]
    async fn on_dial_failure_already_connected_and_disconnected() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(
            SupportedTransport::Tcp,
            Box::new(crate::transport::dummy::DummyTransport {}),
        );

        let peer = PeerId::random();
        let dial_address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));
        let connect_address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(192, 168, 1, 173)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));
        assert!(manager.dial_address(dial_address.clone()).await.is_ok());
        assert_eq!(manager.pending_connections.len(), 1);

        match &manager.peers.read().get(&peer).unwrap().state {
            PeerState::Dialing { record } => {
                assert_eq!(record.address(), &dial_address);
            }
            state => panic!("invalid state for peer: {state:?}"),
        }

        // remote peer connected to local node from a different address that was dialed
        manager
            .on_connection_established(
                &peer,
                &ConnectionId::from(1usize),
                &Endpoint::listener(connect_address, ConnectionId::from(1usize)),
            )
            .unwrap();

        // connection to remote was closed while the dial was still in progress
        manager.on_connection_closed(&peer, &ConnectionId::from(1usize)).unwrap();

        // verify that the peer state is `Disconnected`
        {
            let peers = manager.peers.read();
            let peer = peers.get(&peer).unwrap();

            match &peer.state {
                PeerState::Disconnected {
                    dial_record: Some(dial_record),
                    ..
                } => {
                    assert_eq!(dial_record.address(), &dial_address);
                }
                state => panic!("invalid state: {state:?}"),
            }
        }

        // dialing the peer failed
        manager
            .on_dial_failure(
                &ConnectionId::from(0usize),
                &dial_address,
                &Error::Disconnected,
            )
            .unwrap();

        let peers = manager.peers.read();
        let peer = peers.get(&peer).unwrap();

        match &peer.state {
            PeerState::Disconnected {
                dial_record: None, ..
            } => {
                assert!(peer.addresses.contains(&dial_address));
            }
            state => panic!("invalid state: {state:?}"),
        }
    }

    // local node tried to dial a node and it failed but in the mean
    // time the remote node dialed local node and that succeeded.
    //
    // while the dial was still in progresss, the remote node disconnected after which
    // the dial failure was reported.
    #[tokio::test]
    async fn on_dial_success_while_connected_and_disconnected() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(
            SupportedTransport::Tcp,
            Box::new(crate::transport::dummy::DummyTransport {}),
        );

        let peer = PeerId::random();
        let dial_address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));
        let connect_address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(192, 168, 1, 173)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));
        assert!(manager.dial_address(dial_address.clone()).await.is_ok());
        assert_eq!(manager.pending_connections.len(), 1);

        match &manager.peers.read().get(&peer).unwrap().state {
            PeerState::Dialing { record } => {
                assert_eq!(record.address(), &dial_address);
            }
            state => panic!("invalid state for peer: {state:?}"),
        }

        // remote peer connected to local node from a different address that was dialed
        manager
            .on_connection_established(
                &peer,
                &ConnectionId::from(1usize),
                &Endpoint::listener(connect_address, ConnectionId::from(1usize)),
            )
            .unwrap();

        // connection to remote was closed while the dial was still in progress
        manager.on_connection_closed(&peer, &ConnectionId::from(1usize)).unwrap();

        // verify that the peer state is `Disconnected`
        {
            let peers = manager.peers.read();
            let peer = peers.get(&peer).unwrap();

            match &peer.state {
                PeerState::Disconnected {
                    dial_record: Some(dial_record),
                    ..
                } => {
                    assert_eq!(dial_record.address(), &dial_address);
                }
                state => panic!("invalid state: {state:?}"),
            }
        }

        // the original dial succeeded
        manager
            .on_connection_established(
                &peer,
                &ConnectionId::from(0usize),
                &Endpoint::dialer(dial_address, ConnectionId::from(0usize)),
            )
            .unwrap();

        let peers = manager.peers.read();
        let peer = peers.get(&peer).unwrap();

        match &peer.state {
            PeerState::Connected {
                dial_record: None, ..
            } => {}
            state => panic!("invalid state: {state:?}"),
        }
    }

    #[tokio::test]
    async fn secondary_connection_is_tracked() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport {}));

        let peer = PeerId::random();
        let address1 = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));
        let address2 = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(192, 168, 1, 173)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));
        let address3 = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(192, 168, 10, 64)))
            .with(Protocol::Tcp(9999))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));

        // remote peer connected to local node
        manager
            .on_connection_established(
                &peer,
                &ConnectionId::from(0usize),
                &Endpoint::listener(address1, ConnectionId::from(0usize)),
            )
            .unwrap();

        // verify that the peer state is `Connected` with no seconary connection
        {
            let peers = manager.peers.read();
            let peer = peers.get(&peer).unwrap();

            match &peer.state {
                PeerState::Connected {
                    dial_record: None, ..
                } => {
                    assert!(peer.secondary_connection.is_none());
                }
                state => panic!("invalid state: {state:?}"),
            }
        }

        // second connection is established, verify that the seconary connection is tracked
        manager
            .on_connection_established(
                &peer,
                &ConnectionId::from(1usize),
                &Endpoint::listener(address2.clone(), ConnectionId::from(1usize)),
            )
            .unwrap();

        let peers = manager.peers.read();
        let context = peers.get(&peer).unwrap();

        match &context.state {
            PeerState::Connected {
                dial_record: None, ..
            } => {
                let seconary_connection = context.secondary_connection.as_ref().unwrap();
                assert_eq!(seconary_connection.address(), &address2);
                assert_eq!(seconary_connection.score(), SCORE_DIAL_SUCCESS);
            }
            state => panic!("invalid state: {state:?}"),
        }
        drop(peers);

        // tertiary connection is ignored
        manager
            .on_connection_established(
                &peer,
                &ConnectionId::from(2usize),
                &Endpoint::listener(address3.clone(), ConnectionId::from(2usize)),
            )
            .unwrap();

        let peers = manager.peers.read();
        let peer = peers.get(&peer).unwrap();

        match &peer.state {
            PeerState::Connected {
                dial_record: None, ..
            } => {
                let seconary_connection = peer.secondary_connection.as_ref().unwrap();
                assert_eq!(seconary_connection.address(), &address2);
                assert_eq!(seconary_connection.score(), SCORE_DIAL_SUCCESS);
                assert!(peer.addresses.contains(&address3));
            }
            state => panic!("invalid state: {state:?}"),
        }
    }

    #[tokio::test]
    async fn secondary_connection_closed() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport {}));

        let peer = PeerId::random();
        let address1 = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));
        let address2 = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(192, 168, 1, 173)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));

        // remote peer connected to local node
        let emit_event = manager
            .on_connection_established(
                &peer,
                &ConnectionId::from(0usize),
                &Endpoint::listener(address1, ConnectionId::from(0usize)),
            )
            .unwrap();
        assert!(emit_event);

        // verify that the peer state is `Connected` with no seconary connection
        {
            let peers = manager.peers.read();
            let peer = peers.get(&peer).unwrap();

            match &peer.state {
                PeerState::Connected {
                    dial_record: None, ..
                } => {
                    assert!(peer.secondary_connection.is_none());
                }
                state => panic!("invalid state: {state:?}"),
            }
        }

        // second connection is established, verify that the seconary connection is tracked
        let emit_event = manager
            .on_connection_established(
                &peer,
                &ConnectionId::from(1usize),
                &Endpoint::dialer(address2.clone(), ConnectionId::from(1usize)),
            )
            .unwrap();
        assert!(emit_event);

        let peers = manager.peers.read();
        let context = peers.get(&peer).unwrap();

        match &context.state {
            PeerState::Connected {
                dial_record: None, ..
            } => {
                let seconary_connection = context.secondary_connection.as_ref().unwrap();
                assert_eq!(seconary_connection.address(), &address2);
                assert_eq!(seconary_connection.score(), SCORE_DIAL_SUCCESS);
            }
            state => panic!("invalid state: {state:?}"),
        }
        drop(peers);

        // close the secondary connection and verify that the peer remains connected
        let emit_event = manager.on_connection_closed(&peer, &ConnectionId::from(1usize)).unwrap();
        assert!(!emit_event);

        let peers = manager.peers.read();
        let context = peers.get(&peer).unwrap();

        match &context.state {
            PeerState::Connected {
                dial_record: None,
                record,
            } => {
                assert!(context.secondary_connection.is_none());
                assert!(context.addresses.contains(&address2));
                assert_eq!(record.connection_id(), &Some(ConnectionId::from(0usize)));
            }
            state => panic!("invalid state: {state:?}"),
        }
    }

    #[tokio::test]
    async fn switch_to_secondary_connection() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport {}));

        let peer = PeerId::random();
        let address1 = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));
        let address2 = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(192, 168, 1, 173)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));

        // remote peer connected to local node
        let emit_event = manager
            .on_connection_established(
                &peer,
                &ConnectionId::from(0usize),
                &Endpoint::listener(address1.clone(), ConnectionId::from(0usize)),
            )
            .unwrap();
        assert!(emit_event);

        // verify that the peer state is `Connected` with no seconary connection
        {
            let peers = manager.peers.read();
            let peer = peers.get(&peer).unwrap();

            match &peer.state {
                PeerState::Connected {
                    dial_record: None, ..
                } => {
                    assert!(peer.secondary_connection.is_none());
                }
                state => panic!("invalid state: {state:?}"),
            }
        }

        // second connection is established, verify that the seconary connection is tracked
        let emit_event = manager
            .on_connection_established(
                &peer,
                &ConnectionId::from(1usize),
                &Endpoint::dialer(address2.clone(), ConnectionId::from(1usize)),
            )
            .unwrap();
        assert!(emit_event);

        let peers = manager.peers.read();
        let context = peers.get(&peer).unwrap();

        match &context.state {
            PeerState::Connected {
                dial_record: None, ..
            } => {
                let seconary_connection = context.secondary_connection.as_ref().unwrap();
                assert_eq!(seconary_connection.address(), &address2);
                assert_eq!(seconary_connection.score(), SCORE_DIAL_SUCCESS);
            }
            state => panic!("invalid state: {state:?}"),
        }
        drop(peers);

        // close the primary connection and verify that the peer remains connected
        // while the primary connection address is stored in peer addresses
        let emit_event = manager.on_connection_closed(&peer, &ConnectionId::from(0usize)).unwrap();
        assert!(!emit_event);

        let peers = manager.peers.read();
        let context = peers.get(&peer).unwrap();

        match &context.state {
            PeerState::Connected {
                dial_record: None,
                record,
            } => {
                assert!(context.secondary_connection.is_none());
                assert!(context.addresses.contains(&address1));
                assert_eq!(record.connection_id(), &Some(ConnectionId::from(1usize)));
            }
            state => panic!("invalid state: {state:?}"),
        }
    }

    // two connections already exist and a third was opened which is ignored by
    // `on_connection_established()`, when that connection is closed, verify that
    // it's handled gracefully
    #[tokio::test]
    async fn tertiary_connection_closed() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport {}));

        let peer = PeerId::random();
        let address1 = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));
        let address2 = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(192, 168, 1, 173)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));
        let address3 = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(192, 168, 1, 173)))
            .with(Protocol::Tcp(9999))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));

        // remote peer connected to local node
        let emit_event = manager
            .on_connection_established(
                &peer,
                &ConnectionId::from(0usize),
                &Endpoint::listener(address1, ConnectionId::from(0usize)),
            )
            .unwrap();
        assert!(emit_event);

        // verify that the peer state is `Connected` with no seconary connection
        {
            let peers = manager.peers.read();
            let peer = peers.get(&peer).unwrap();

            match &peer.state {
                PeerState::Connected {
                    dial_record: None, ..
                } => {
                    assert!(peer.secondary_connection.is_none());
                }
                state => panic!("invalid state: {state:?}"),
            }
        }

        // second connection is established, verify that the seconary connection is tracked
        let emit_event = manager
            .on_connection_established(
                &peer,
                &ConnectionId::from(1usize),
                &Endpoint::dialer(address2.clone(), ConnectionId::from(1usize)),
            )
            .unwrap();
        assert!(emit_event);

        let peers = manager.peers.read();
        let context = peers.get(&peer).unwrap();

        match &context.state {
            PeerState::Connected {
                dial_record: None, ..
            } => {
                let seconary_connection = context.secondary_connection.as_ref().unwrap();
                assert_eq!(seconary_connection.address(), &address2);
                assert_eq!(seconary_connection.score(), SCORE_DIAL_SUCCESS);
            }
            state => panic!("invalid state: {state:?}"),
        }
        drop(peers);

        // third connection is established, verify that it's discarded
        let emit_event = manager
            .on_connection_established(
                &peer,
                &ConnectionId::from(2usize),
                &Endpoint::dialer(address3.clone(), ConnectionId::from(2usize)),
            )
            .unwrap();
        assert!(!emit_event);

        let peers = manager.peers.read();
        let context = peers.get(&peer).unwrap();
        assert!(context.addresses.contains(&address3));
        drop(peers);

        // close the tertiary connection that was ignored
        let emit_event = manager.on_connection_closed(&peer, &ConnectionId::from(2usize)).unwrap();
        assert!(!emit_event);

        // verify that the state remains unchanged
        let peers = manager.peers.read();
        let context = peers.get(&peer).unwrap();

        match &context.state {
            PeerState::Connected {
                dial_record: None, ..
            } => {
                let seconary_connection = context.secondary_connection.as_ref().unwrap();
                assert_eq!(seconary_connection.address(), &address2);
                assert_eq!(seconary_connection.score(), SCORE_DIAL_SUCCESS);
            }
            state => panic!("invalid state: {state:?}"),
        }
        drop(peers);
    }
}
