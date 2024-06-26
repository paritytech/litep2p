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
    crypto::ed25519::Keypair,
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
    time::Duration,
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
const SCORE_CONNECT_SUCCESS: i32 = 100i32;

/// Score for a non-working address.
const SCORE_CONNECT_FAILURE: i32 = -100i32;

/// TODO:
enum ConnectionEstablishedResult {
    /// Accept connection and inform `Litep2p` about the connection.
    Accept,

    /// Reject connection.
    Reject,
}

/// [`crate::transport::manager::TransportManager`] events.
pub enum TransportManagerEvent {
    /// Connection closed to remote peer.
    ConnectionClosed {
        /// Peer ID.
        peer: PeerId,

        /// Connection ID.
        connection: ConnectionId,
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
    type Item = (SupportedTransport, TransportEvent);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let len = match self.transports.len() {
            0 => return Poll::Ready(None),
            len => len,
        };
        let start_index = self.index;

        loop {
            let index = self.index % len;
            self.index += 1;

            let (key, stream) = self.transports.get_index_mut(index).expect("transport to exist");
            match stream.poll_next_unpin(cx) {
                Poll::Pending => {}
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(event)) => return Poll::Ready(Some((*key, event))),
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

    /// Maximum parallel dial attempts per peer.
    max_parallel_dials: usize,

    /// Installed protocols.
    protocols: HashMap<ProtocolName, ProtocolContext>,

    /// All names (main and fallback(s)) of the installed protocols.
    protocol_names: HashSet<ProtocolName>,

    /// Listen addresses.
    listen_addresses: Arc<RwLock<HashSet<Multiaddr>>>,

    /// Next connection ID.
    next_connection_id: Arc<AtomicUsize>,

    /// Next substream ID.
    next_substream_id: Arc<AtomicUsize>,

    /// Installed transports.
    transports: TransportContext,

    /// Peers
    peers: Arc<RwLock<HashMap<PeerId, PeerContext>>>,

    /// Handle to [`crate::transport::manager::TransportManager`].
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
    /// Create new [`crate::transport::manager::TransportManager`].
    // TODO: don't return handle here
    pub fn new(
        keypair: Keypair,
        supported_transports: HashSet<SupportedTransport>,
        bandwidth_sink: BandwidthSink,
        max_parallel_dials: usize,
    ) -> (Self, TransportManagerHandle) {
        let local_peer_id = PeerId::from_public_key(&keypair.public().into());
        let peers = Arc::new(RwLock::new(HashMap::new()));
        let (cmd_tx, cmd_rx) = channel(256);
        let (event_tx, event_rx) = channel(256);
        let listen_addresses = Arc::new(RwLock::new(HashSet::new()));
        let handle = TransportManagerHandle::new(
            local_peer_id,
            peers.clone(),
            cmd_tx,
            supported_transports,
            Arc::clone(&listen_addresses),
        );

        (
            Self {
                peers,
                cmd_rx,
                keypair,
                event_tx,
                event_rx,
                local_peer_id,
                bandwidth_sink,
                listen_addresses,
                max_parallel_dials,
                protocols: HashMap::new(),
                transports: TransportContext::new(),
                protocol_names: HashSet::new(),
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

    /// Register protocol to the [`crate::transport::manager::TransportManager`].
    ///
    /// This allocates new context for the protocol and returns a handle
    /// which the protocol can use the interact with the transport subsystem.
    pub fn register_protocol(
        &mut self,
        protocol: ProtocolName,
        fallback_names: Vec<ProtocolName>,
        codec: ProtocolCodec,
        keep_alive: Duration,
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
            keep_alive,
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
        self.transport_manager_handle.register_transport(name);
    }

    /// Register local listen address.
    pub fn register_listen_address(&mut self, address: Multiaddr) {
        assert!(!address.iter().any(|protocol| std::matches!(protocol, Protocol::P2p(_))));

        let mut listen_addresses = self.listen_addresses.write();

        listen_addresses.insert(address.clone());
        listen_addresses.insert(address.with(Protocol::P2p(
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
    pub async fn dial(&mut self, peer: PeerId) -> crate::Result<()> {
        if peer == self.local_peer_id {
            return Err(Error::TriedToDialSelf);
        }
        let mut peers = self.peers.write();

        // if the peer is disconnected, return its context
        //
        // otherwise set the state back what it was and return dial status to caller
        let PeerContext {
            state,
            secondary_connection,
            mut addresses,
        } = match peers.remove(&peer) {
            None => return Err(Error::PeerDoesntExist(peer)),
            Some(
                context @ PeerContext {
                    state: PeerState::Connected { .. },
                    ..
                },
            ) => {
                peers.insert(peer, context);
                return Err(Error::AlreadyConnected);
            }
            Some(
                context @ PeerContext {
                    state: PeerState::Dialing { .. } | PeerState::Opening { .. },
                    ..
                },
            ) => {
                peers.insert(peer, context);
                return Ok(());
            }
            Some(context) => context,
        };

        if let PeerState::Disconnected {
            dial_record: Some(_),
        } = &state
        {
            tracing::debug!(
                target: LOG_TARGET,
                ?peer,
                "peer is aready being dialed",
            );

            peers.insert(
                peer,
                PeerContext {
                    state,
                    secondary_connection,
                    addresses,
                },
            );

            return Ok(());
        }

        let mut records: HashMap<_, _> = addresses
            .take(self.max_parallel_dials)
            .into_iter()
            .map(|record| (record.address().clone(), record))
            .collect();

        if records.is_empty() {
            return Err(Error::NoAddressAvailable(peer));
        }

        for record in records.values() {
            if self.listen_addresses.read().contains(record.as_ref()) {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?peer,
                    ?record,
                    "tried to dial self",
                );

                debug_assert!(false);
                return Err(Error::TriedToDialSelf);
            }
        }

        // set connection id for the address record and put peer into `Opening` state
        let connection_id =
            ConnectionId::from(self.next_connection_id.fetch_add(1usize, Ordering::Relaxed));

        tracing::debug!(
            target: LOG_TARGET,
            ?connection_id,
            addresses = ?records,
            "dial remote peer",
        );

        let mut transports = HashSet::new();
        let mut websocket = Vec::new();
        let mut quic = Vec::new();
        let mut tcp = Vec::new();

        for (address, record) in &mut records {
            record.set_connection_id(connection_id);

            let mut iter = address.iter();
            match iter.find(|protocol| std::matches!(protocol, Protocol::QuicV1)) {
                Some(_) => {
                    quic.push(address.clone());
                    transports.insert(SupportedTransport::Quic);
                }
                _ => match address
                    .iter()
                    .find(|protocol| std::matches!(protocol, Protocol::Ws(_) | Protocol::Wss(_)))
                {
                    Some(_) => {
                        websocket.push(address.clone());
                        transports.insert(SupportedTransport::WebSocket);
                    }
                    None => {
                        tcp.push(address.clone());
                        transports.insert(SupportedTransport::Tcp);
                    }
                },
            }
        }

        peers.insert(
            peer,
            PeerContext {
                state: PeerState::Opening {
                    records,
                    connection_id,
                    transports,
                },
                secondary_connection,
                addresses,
            },
        );

        if !tcp.is_empty() {
            self.transports
                .get_mut(&SupportedTransport::Tcp)
                .expect("transport to be supported")
                .open(connection_id, tcp)?;
        }

        if !quic.is_empty() {
            self.transports
                .get_mut(&SupportedTransport::Quic)
                .expect("transport to be supported")
                .open(connection_id, quic)?;
        }

        if !websocket.is_empty() {
            self.transports
                .get_mut(&SupportedTransport::WebSocket)
                .expect("transport to be supported")
                .open(connection_id, websocket)?;
        }

        self.pending_connections.insert(connection_id, peer);

        Ok(())
    }

    /// Dial peer using `Multiaddr`.
    ///
    /// Returns an error if address it not valid.
    pub async fn dial_address(&mut self, address: Multiaddr) -> crate::Result<()> {
        let mut record = AddressRecord::from_multiaddr(address)
            .ok_or(Error::AddressError(AddressError::PeerIdMissing))?;

        if self.listen_addresses.read().contains(record.as_ref()) {
            return Err(Error::TriedToDialSelf);
        }

        tracing::debug!(target: LOG_TARGET, address = ?record.address(), "dial remote peer over address");

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

        let supported_transport = match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(record.address().clone()))?
        {
            Protocol::Tcp(_) => match protocol_stack.next() {
                Some(Protocol::Ws(_)) | Some(Protocol::Wss(_)) => SupportedTransport::WebSocket,
                Some(Protocol::P2p(_)) => SupportedTransport::Tcp,
                _ => return Err(Error::TransportNotSupported(record.address().clone())),
            },
            Protocol::Udp(_) => match protocol_stack
                .next()
                .ok_or_else(|| Error::TransportNotSupported(record.address().clone()))?
            {
                Protocol::QuicV1 => SupportedTransport::Quic,
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

        // when constructing `AddressRecord`, `PeerId` was verified to be part of the address
        let remote_peer_id =
            PeerId::try_from_multiaddr(record.address()).expect("`PeerId` to exist");

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
                    state:
                        PeerState::Dialing { .. }
                        | PeerState::Connected { .. }
                        | PeerState::Opening { .. },
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
    fn on_dial_failure(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        let peer = self.pending_connections.remove(&connection_id).ok_or_else(|| {
            tracing::error!(
                target: LOG_TARGET,
                ?connection_id,
                "dial failed for a connection that doesn't exist",
            );
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
                debug_assert_eq!(record.connection_id(), &Some(connection_id));

                record.update_score(SCORE_CONNECT_FAILURE);
                context.addresses.insert(record.clone());

                context.state = PeerState::Disconnected { dial_record: None };
                Ok(())
            }
            PeerState::Opening { .. } => {
                todo!();
            }
            PeerState::Connected {
                record,
                dial_record: Some(mut dial_record),
            } => {
                dial_record.update_score(SCORE_CONNECT_FAILURE);
                context.addresses.insert(dial_record);

                context.state = PeerState::Connected {
                    record,
                    dial_record: None,
                };
                Ok(())
            }
            PeerState::Disconnected {
                dial_record: Some(mut dial_record),
            } => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?connection_id,
                    ?dial_record,
                    "dial failed for a disconnected peer",
                );

                dial_record.update_score(SCORE_CONNECT_FAILURE);
                context.addresses.insert(dial_record);

                Ok(())
            }
            state => {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?peer,
                    ?connection_id,
                    ?state,
                    "invalid state for dial failure",
                );
                context.state = state;

                debug_assert!(false);
                Ok(())
            }
        }
    }

    /// Handle closed connection.
    ///
    /// Returns `bool` which indicates whether the event should be returned or not.
    fn on_connection_closed(
        &mut self,
        peer: PeerId,
        connection_id: ConnectionId,
    ) -> crate::Result<Option<TransportEvent>> {
        let mut peers = self.peers.write();
        let Some(context) = peers.get_mut(&peer) else {
            tracing::warn!(
                target: LOG_TARGET,
                ?peer,
                ?connection_id,
                "cannot handle closed connection: peer doesn't exist",
            );
            debug_assert!(false);
            return Err(Error::PeerDoesntExist(peer));
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
            } => match record.connection_id() == &Some(connection_id) {
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

                        Ok(Some(TransportEvent::ConnectionClosed {
                            peer,
                            connection_id,
                        }))
                    }
                    Some(secondary_connection) => {
                        context.addresses.insert(record);
                        context.state = PeerState::Connected {
                            record: secondary_connection,
                            dial_record: actual_dial_record,
                        };

                        Ok(None)
                    }
                },
                // secondary connection was closed
                false => match context.secondary_connection.take() {
                    Some(secondary_connection) => {
                        if secondary_connection.connection_id() != &Some(connection_id) {
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

                            return Ok(None);
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
                        Ok(None)
                    }
                    None => {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?peer,
                            ?connection_id,
                            "non-primary connection was closed but secondary connection doesn't exist",
                        );

                        debug_assert!(false);
                        Err(Error::InvalidState)
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
                    Err(Error::InvalidState)
                }
                None => {
                    context.state = PeerState::Disconnected { dial_record };

                    Ok(Some(TransportEvent::ConnectionClosed {
                        peer,
                        connection_id,
                    }))
                }
            },
            state => {
                tracing::warn!(target: LOG_TARGET, ?peer, ?connection_id, ?state, "invalid state for a closed connection");
                debug_assert!(false);
                Err(Error::InvalidState)
            }
        }
    }

    fn on_connection_established(
        &mut self,
        peer: PeerId,
        endpoint: &Endpoint,
    ) -> crate::Result<ConnectionEstablishedResult> {
        if let Some(dialed_peer) = self.pending_connections.remove(&endpoint.connection_id()) {
            if dialed_peer != peer {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?dialed_peer,
                    ?peer,
                    ?endpoint,
                    "peer ids do not match but transport was supposed to reject connection"
                );
                debug_assert!(false);
                return Err(Error::InvalidState);
            }
        };

        let mut peers = self.peers.write();
        match peers.get_mut(&peer) {
            Some(context) => match context.state {
                PeerState::Connected {
                    ref mut dial_record,
                    ..
                } => match context.secondary_connection {
                    Some(_) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?peer,
                            connection_id = ?endpoint.connection_id(),
                            ?endpoint,
                            "secondary connection already exists, ignoring connection",
                        );

                        // insert address into the store only if we're the dialer
                        //
                        // if we're the listener, remote might have dialed with an ephemeral port
                        // which it might not be listening, making this address useless
                        if endpoint.is_listener() {
                            context.addresses.insert(AddressRecord::new(
                                &peer,
                                endpoint.address().clone(),
                                SCORE_CONNECT_SUCCESS,
                                None,
                            ))
                        }

                        return Ok(ConnectionEstablishedResult::Reject);
                    }
                    None => match dial_record.take() {
                        Some(record)
                            if record.connection_id() == &Some(endpoint.connection_id()) =>
                        {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                connection_id = ?endpoint.connection_id(),
                                address = ?endpoint.address(),
                                "dialed connection opened as secondary connection",
                            );

                            context.secondary_connection = Some(AddressRecord::new(
                                &peer,
                                endpoint.address().clone(),
                                SCORE_CONNECT_SUCCESS,
                                Some(endpoint.connection_id()),
                            ));
                        }
                        None => {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                connection_id = ?endpoint.connection_id(),
                                address = ?endpoint.address(),
                                "secondary connection",
                            );

                            context.secondary_connection = Some(AddressRecord::new(
                                &peer,
                                endpoint.address().clone(),
                                SCORE_CONNECT_SUCCESS,
                                Some(endpoint.connection_id()),
                            ));
                        }
                        Some(record) => tracing::warn!(
                            target: LOG_TARGET,
                            ?peer,
                            connection_id = ?endpoint.connection_id(),
                            address = ?endpoint.address(),
                            dial_record = ?record,
                            "unknown connection opened as secondary connection, discarding",
                        ),
                    },
                },
                PeerState::Dialing { ref record, .. } => {
                    match record.connection_id() == &Some(endpoint.connection_id()) {
                        true => {
                            tracing::trace!(
                                target: LOG_TARGET,
                                ?peer,
                                connection_id = ?endpoint.connection_id(),
                                ?endpoint,
                                ?record,
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
                                connection_id = ?endpoint.connection_id(),
                                ?endpoint,
                                "connection opened by remote while local node was dialing",
                            );

                            context.state = PeerState::Connected {
                                record: AddressRecord::new(
                                    &peer,
                                    endpoint.address().clone(),
                                    SCORE_CONNECT_SUCCESS,
                                    Some(endpoint.connection_id()),
                                ),
                                dial_record: Some(record.clone()),
                            };
                        }
                    }
                }
                PeerState::Opening {
                    ref mut records,
                    connection_id,
                    ref transports,
                } => {
                    debug_assert!(std::matches!(endpoint, &Endpoint::Listener { .. }));

                    tracing::trace!(
                        target: LOG_TARGET,
                        ?peer,
                        dial_connection_id = ?connection_id,
                        dial_records = ?records,
                        dial_transports = ?transports,
                        listener_endpoint = ?endpoint,
                        "inbound connection while opening an outbound connection",
                    );

                    // cancel all pending dials
                    transports.iter().for_each(|transport| {
                        self.transports
                            .get_mut(transport)
                            .expect("transport to exist")
                            .cancel(connection_id);
                    });

                    // since an inbound connection was removed, the outbound connection can be
                    // removed from pendind dials
                    //
                    // all records have the same `ConnectionId` so it doens't matter which of them
                    // is used to remove the pending dial
                    self.pending_connections.remove(
                        &records
                            .iter()
                            .next()
                            .expect("record to exist")
                            .1
                            .connection_id()
                            .expect("`ConnectionId` to exist"),
                    );

                    let record = match records.remove(endpoint.address()) {
                        Some(mut record) => {
                            record.update_score(SCORE_CONNECT_SUCCESS);
                            record.set_connection_id(endpoint.connection_id());
                            record
                        }
                        None => AddressRecord::new(
                            &peer,
                            endpoint.address().clone(),
                            SCORE_CONNECT_SUCCESS,
                            Some(endpoint.connection_id()),
                        ),
                    };
                    context.addresses.extend(records.iter().map(|(_, record)| record));

                    context.state = PeerState::Connected {
                        record,
                        dial_record: None,
                    };
                }
                PeerState::Disconnected {
                    ref mut dial_record,
                } => {
                    tracing::trace!(
                        target: LOG_TARGET,
                        ?peer,
                        connection_id = ?endpoint.connection_id(),
                        ?endpoint,
                        ?dial_record,
                        "connection opened by remote or delayed dial succeeded",
                    );

                    let (record, dial_record) = match dial_record.take() {
                        Some(mut dial_record) =>
                            if dial_record.address() == endpoint.address() {
                                dial_record.set_connection_id(endpoint.connection_id());
                                (dial_record, None)
                            } else {
                                (
                                    AddressRecord::new(
                                        &peer,
                                        endpoint.address().clone(),
                                        SCORE_CONNECT_SUCCESS,
                                        Some(endpoint.connection_id()),
                                    ),
                                    Some(dial_record),
                                )
                            },
                        None => (
                            AddressRecord::new(
                                &peer,
                                endpoint.address().clone(),
                                SCORE_CONNECT_SUCCESS,
                                Some(endpoint.connection_id()),
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
                    peer,
                    PeerContext {
                        state: PeerState::Connected {
                            record: AddressRecord::new(
                                &peer,
                                endpoint.address().clone(),
                                SCORE_CONNECT_SUCCESS,
                                Some(endpoint.connection_id()),
                            ),
                            dial_record: None,
                        },
                        addresses: AddressStore::new(),
                        secondary_connection: None,
                    },
                );
            }
        }

        Ok(ConnectionEstablishedResult::Accept)
    }

    fn on_connection_opened(
        &mut self,
        transport: SupportedTransport,
        connection_id: ConnectionId,
        address: Multiaddr,
    ) -> crate::Result<()> {
        let Some(peer) = self.pending_connections.remove(&connection_id) else {
            tracing::warn!(
                target: LOG_TARGET,
                ?connection_id,
                ?transport,
                ?address,
                "connection opened but dial record doesn't exist",
            );

            debug_assert!(false);
            return Err(Error::InvalidState);
        };

        let mut peers = self.peers.write();
        let context = peers.get_mut(&peer).ok_or_else(|| {
            tracing::warn!(
                target: LOG_TARGET,
                ?peer,
                ?connection_id,
                "connection opened but peer doesn't exist",
            );

            debug_assert!(false);
            Error::InvalidState
        })?;

        match std::mem::replace(
            &mut context.state,
            PeerState::Disconnected { dial_record: None },
        ) {
            PeerState::Opening {
                mut records,
                connection_id,
                transports,
            } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?connection_id,
                    ?address,
                    ?transport,
                    "connection opened to peer",
                );

                // cancel open attempts for other transports as connection already exists
                for transport in transports.iter() {
                    self.transports
                        .get_mut(transport)
                        .expect("transport to exist")
                        .cancel(connection_id);
                }

                // set peer state to `Dialing` to signal that the connection is fully opening
                //
                // set the succeeded `AddressRecord` as the one that is used for dialing and move
                // all other address records back to `AddressStore`. and ask
                // transport to negotiate the
                let mut dial_record = records.remove(&address).expect("address to exist");
                dial_record.update_score(SCORE_CONNECT_SUCCESS);

                // negotiate the connection
                match self
                    .transports
                    .get_mut(&transport)
                    .expect("transport to exist")
                    .negotiate(connection_id)
                {
                    Ok(()) => {
                        tracing::trace!(
                            target: LOG_TARGET,
                            ?peer,
                            ?connection_id,
                            ?dial_record,
                            ?transport,
                            "negotiation started"
                        );

                        self.pending_connections.insert(connection_id, peer);

                        context.state = PeerState::Dialing {
                            record: dial_record,
                        };

                        for (_, record) in records {
                            context.addresses.insert(record);
                        }

                        Ok(())
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?peer,
                            ?connection_id,
                            ?error,
                            "failed to negotiate connection",
                        );
                        context.state = PeerState::Disconnected { dial_record: None };

                        debug_assert!(false);
                        Err(Error::InvalidState)
                    }
                }
            }
            state => {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?peer,
                    ?connection_id,
                    ?state,
                    "connection opened but `PeerState` is not `Opening`",
                );
                context.state = state;

                debug_assert!(false);
                Err(Error::InvalidState)
            }
        }
    }

    /// Handle open failure for dialing attempt for `transport`
    fn on_open_failure(
        &mut self,
        transport: SupportedTransport,
        connection_id: ConnectionId,
    ) -> crate::Result<Option<PeerId>> {
        let Some(peer) = self.pending_connections.remove(&connection_id) else {
            tracing::warn!(
                target: LOG_TARGET,
                ?connection_id,
                "open failure but dial record doesn't exist",
            );

            debug_assert!(false);
            return Err(Error::InvalidState);
        };

        let mut peers = self.peers.write();
        let context = peers.get_mut(&peer).ok_or_else(|| {
            tracing::warn!(
                target: LOG_TARGET,
                ?peer,
                ?connection_id,
                "open failure but peer doesn't exist",
            );

            debug_assert!(false);
            Error::InvalidState
        })?;

        match std::mem::replace(
            &mut context.state,
            PeerState::Disconnected { dial_record: None },
        ) {
            PeerState::Opening {
                records,
                connection_id,
                mut transports,
            } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?connection_id,
                    ?transport,
                    "open failure for peer",
                );
                transports.remove(&transport);

                if transports.is_empty() {
                    for (_, mut record) in records {
                        record.update_score(SCORE_CONNECT_FAILURE);
                        context.addresses.insert(record);
                    }

                    tracing::trace!(
                        target: LOG_TARGET,
                        ?peer,
                        ?connection_id,
                        "open failure for last transport",
                    );

                    return Ok(Some(peer));
                }

                self.pending_connections.insert(connection_id, peer);
                context.state = PeerState::Opening {
                    records,
                    connection_id,
                    transports,
                };

                Ok(None)
            }
            state => {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?peer,
                    ?connection_id,
                    ?state,
                    "open failure but `PeerState` is not `Opening`",
                );
                context.state = state;

                debug_assert!(false);
                Err(Error::InvalidState)
            }
        }
    }

    /// Poll next event from [`crate::transport::manager::TransportManager`].
    pub async fn next(&mut self) -> Option<TransportEvent> {
        loop {
            tokio::select! {
                event = self.event_rx.recv() => match event? {
                    TransportManagerEvent::ConnectionClosed {
                        peer,
                        connection: connection_id,
                    } => match self.on_connection_closed(peer, connection_id) {
                        Ok(None) => {}
                        Ok(Some(event)) => return Some(event),
                        Err(error) => tracing::error!(
                            target: LOG_TARGET,
                            ?error,
                            "failed to handle closed connection",
                        ),
                    }
                },
                command = self.cmd_rx.recv() => match command? {
                    InnerTransportManagerCommand::DialPeer { peer } => {
                        if let Err(error) = self.dial(peer).await {
                            tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to dial peer")
                        }
                    }
                    InnerTransportManagerCommand::DialAddress { address } => {
                        if let Err(error) = self.dial_address(address).await {
                            tracing::debug!(target: LOG_TARGET, ?error, "failed to dial peer")
                        }
                    }
                },
                event = self.transports.next() => {
                    let (transport, event) = event?;

                    match event {
                        TransportEvent::DialFailure { connection_id, address, error } => {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?connection_id,
                                ?address,
                                ?error,
                                "failed to dial peer",
                            );

                            if let Ok(()) = self.on_dial_failure(connection_id) {
                                match address.iter().last() {
                                    Some(Protocol::P2p(hash)) => match PeerId::from_multihash(hash) {
                                        Ok(peer) => {
                                            tracing::trace!(
                                                target: LOG_TARGET,
                                                ?connection_id,
                                                ?error,
                                                ?address,
                                                num_protocols = self.protocols.len(),
                                                "dial failure, notify protocols",
                                            );

                                            for (protocol, context) in &self.protocols {
                                                tracing::trace!(
                                                    target: LOG_TARGET,
                                                    ?connection_id,
                                                    ?error,
                                                    ?address,
                                                    ?protocol,
                                                    "dial failure, notify protocol",
                                                );
                                                match context.tx.try_send(InnerTransportEvent::DialFailure {
                                                    peer,
                                                    address: address.clone(),
                                                }) {
                                                    Ok(()) => {}
                                                    Err(_) => {
                                                        tracing::trace!(
                                                            target: LOG_TARGET,
                                                            ?connection_id,
                                                            ?error,
                                                            ?address,
                                                            ?protocol,
                                                            "dial failure, channel to protocol clogged, use await",
                                                        );
                                                        let _ = context
                                                            .tx
                                                            .send(InnerTransportEvent::DialFailure {
                                                                peer,
                                                                address: address.clone(),
                                                            })
                                                            .await;
                                                    }
                                                }
                                            }

                                            tracing::trace!(
                                                target: LOG_TARGET,
                                                ?connection_id,
                                                ?error,
                                                ?address,
                                                "all protocols notified",
                                            );
                                        }
                                        Err(error) => {
                                            tracing::warn!(
                                                target: LOG_TARGET,
                                                ?address,
                                                ?connection_id,
                                                ?error,
                                                "failed to parse `PeerId` from `Multiaddr`",
                                            );
                                            debug_assert!(false);
                                        }
                                    },
                                    _ => {
                                        tracing::warn!(target: LOG_TARGET, ?address, ?connection_id, "address doesn't contain `PeerId`");
                                        debug_assert!(false);
                                    }
                                }

                                return Some(TransportEvent::DialFailure {
                                    connection_id,
                                    address,
                                    error,
                                })
                            }
                        }
                        TransportEvent::ConnectionEstablished { peer, endpoint } => {
                            match self.on_connection_established(peer, &endpoint) {
                                Err(error) => {
                                    tracing::debug!(
                                        target: LOG_TARGET,
                                        ?peer,
                                        ?endpoint,
                                        ?error,
                                        "failed to handle established connection",
                                    );

                                    let _ = self
                                        .transports
                                        .get_mut(&transport)
                                        .expect("transport to exist")
                                        .reject(endpoint.connection_id());
                                }
                                Ok(ConnectionEstablishedResult::Accept) => {
                                    tracing::trace!(
                                        target: LOG_TARGET,
                                        ?peer,
                                        ?endpoint,
                                        "accept connection",
                                    );

                                    let _ = self
                                        .transports
                                        .get_mut(&transport)
                                        .expect("transport to exist")
                                        .accept(endpoint.connection_id());

                                    return Some(TransportEvent::ConnectionEstablished {
                                        peer,
                                        endpoint,
                                    });
                                }
                                Ok(ConnectionEstablishedResult::Reject) => {
                                    tracing::trace!(
                                        target: LOG_TARGET,
                                        ?peer,
                                        ?endpoint,
                                        "reject connection",
                                    );

                                    let _ = self
                                        .transports
                                        .get_mut(&transport)
                                        .expect("transport to exist")
                                        .reject(endpoint.connection_id());
                                }
                            }
                        }
                        TransportEvent::ConnectionOpened { connection_id, address } => {
                            if let Err(error) = self.on_connection_opened(transport, connection_id, address) {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?connection_id,
                                    ?error,
                                    "failed to handle opened connection",
                                );
                            }
                        }
                        TransportEvent::OpenFailure { connection_id } => {
                            match self.on_open_failure(transport, connection_id) {
                                Err(error) => tracing::debug!(
                                    target: LOG_TARGET,
                                    ?connection_id,
                                    ?error,
                                    "failed to handle opened connection",
                                ),
                                Ok(Some(peer)) => {
                                    tracing::trace!(
                                        target: LOG_TARGET,
                                        ?peer,
                                        ?connection_id,
                                        num_protocols = self.protocols.len(),
                                        "inform protocols about open failure",
                                    );

                                    for (protocol, context) in &self.protocols {
                                        let _ = match context
                                            .tx
                                            .try_send(InnerTransportEvent::DialFailure {
                                                peer,
                                                address: Multiaddr::empty(),
                                            }) {
                                            Ok(_) => Ok(()),
                                            Err(_) => {
                                                tracing::trace!(
                                                    target: LOG_TARGET,
                                                    ?peer,
                                                    %protocol,
                                                    ?connection_id,
                                                    "call to protocol would, block try sending in a blocking way",
                                                );

                                                context
                                                    .tx
                                                    .send(InnerTransportEvent::DialFailure {
                                                        peer,
                                                        address: Multiaddr::empty(),
                                                    })
                                                    .await
                                            }
                                        };
                                    }

                                    return Some(TransportEvent::DialFailure {
                                        connection_id,
                                        address: Multiaddr::empty(),
                                        error: Error::Unknown,
                                    })
                                }
                                Ok(None) => {}
                            }
                        }
                        event => panic!("event not supported: {event:?}"),
                    }
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
            TransportManager::new(Keypair::generate(), HashSet::new(), sink, 8usize);

        manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
            Duration::from_secs(5),
        );
        manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
            Duration::from_secs(5),
        );
    }

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn fallback_protocol_as_duplicate_main_protocol() {
        let sink = BandwidthSink::new();
        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), sink, 8usize);

        manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
            Duration::from_secs(5),
        );
        manager.register_protocol(
            ProtocolName::from("/notif/2"),
            vec![
                ProtocolName::from("/notif/2/new"),
                ProtocolName::from("/notif/1"),
            ],
            ProtocolCodec::UnsignedVarint(None),
            Duration::from_secs(5),
        );
    }

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn duplicate_fallback_protocol() {
        let sink = BandwidthSink::new();
        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), sink, 8usize);

        manager.register_protocol(
            ProtocolName::from("/notif/1"),
            vec![
                ProtocolName::from("/notif/1/new"),
                ProtocolName::from("/notif/1"),
            ],
            ProtocolCodec::UnsignedVarint(None),
            Duration::from_secs(5),
        );
        manager.register_protocol(
            ProtocolName::from("/notif/2"),
            vec![
                ProtocolName::from("/notif/2/new"),
                ProtocolName::from("/notif/1/new"),
            ],
            ProtocolCodec::UnsignedVarint(None),
            Duration::from_secs(5),
        );
    }

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn duplicate_transport() {
        let sink = BandwidthSink::new();
        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), sink, 8usize);

        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));
    }

    #[tokio::test]
    async fn tried_to_self_using_peer_id() {
        let keypair = Keypair::generate();
        let local_peer_id = PeerId::from_public_key(&keypair.public().into());
        let sink = BandwidthSink::new();
        let (mut manager, _handle) = TransportManager::new(keypair, HashSet::new(), sink, 8usize);

        assert!(manager.dial(local_peer_id).await.is_err());
    }

    #[tokio::test]
    async fn try_to_dial_over_disabled_transport() {
        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));

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

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let peer = PeerId::random();
        let dial_address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));

        let transport = Box::new({
            let mut transport = DummyTransport::new();
            transport.inject_event(TransportEvent::ConnectionEstablished {
                peer,
                endpoint: Endpoint::dialer(dial_address.clone(), ConnectionId::from(0usize)),
            });
            transport
        });
        manager.register_transport(SupportedTransport::Tcp, transport);

        assert!(manager.dial_address(dial_address.clone()).await.is_ok());
        assert!(!manager.pending_connections.is_empty());

        {
            let peers = manager.peers.read();

            match peers.get(&peer) {
                Some(PeerContext {
                    state: PeerState::Dialing { .. },
                    ..
                }) => {}
                state => panic!("invalid state for peer: {state:?}"),
            }
        }

        match manager.next().await.unwrap() {
            TransportEvent::ConnectionEstablished {
                peer: event_peer,
                endpoint: event_endpoint,
                ..
            } => {
                assert_eq!(peer, event_peer);
                assert_eq!(
                    event_endpoint,
                    Endpoint::dialer(dial_address.clone(), ConnectionId::from(0usize))
                )
            }
            event => panic!("invalid event: {event:?}"),
        }
    }

    #[tokio::test]
    async fn try_to_dial_same_peer_twice() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));

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

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));

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

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));

        assert!(manager.dial(PeerId::random()).await.is_err());
    }

    #[tokio::test]
    async fn dial_non_peer_with_no_known_addresses() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));

        let peer = PeerId::random();
        manager.peers.write().insert(
            peer,
            PeerContext {
                state: PeerState::Disconnected { dial_record: None },
                addresses: AddressStore::new(),
                secondary_connection: None,
            },
        );

        assert!(manager.dial(peer).await.is_err());
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
            8usize,
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

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));

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
                peer,
                &Endpoint::dialer(connect_address, ConnectionId::from(1usize)),
            )
            .unwrap();

        // dialing the peer failed
        manager.on_dial_failure(ConnectionId::from(0usize)).unwrap();

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

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));

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
                peer,
                &Endpoint::listener(connect_address, ConnectionId::from(1usize)),
            )
            .unwrap();

        // connection to remote was closed while the dial was still in progress
        manager.on_connection_closed(peer, ConnectionId::from(1usize)).unwrap();

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
        manager.on_dial_failure(ConnectionId::from(0usize)).unwrap();

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

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let _handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));

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
                peer,
                &Endpoint::listener(connect_address, ConnectionId::from(1usize)),
            )
            .unwrap();

        // connection to remote was closed while the dial was still in progress
        manager.on_connection_closed(peer, ConnectionId::from(1usize)).unwrap();

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
                peer,
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

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));

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
                peer,
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
                peer,
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
                assert_eq!(seconary_connection.score(), SCORE_CONNECT_SUCCESS);
            }
            state => panic!("invalid state: {state:?}"),
        }
        drop(peers);

        // tertiary connection is ignored
        manager
            .on_connection_established(
                peer,
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
                assert_eq!(seconary_connection.score(), SCORE_CONNECT_SUCCESS);
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

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));

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
                peer,
                &Endpoint::listener(address1, ConnectionId::from(0usize)),
            )
            .unwrap();
        assert!(std::matches!(
            emit_event,
            ConnectionEstablishedResult::Accept
        ));

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
                peer,
                &Endpoint::dialer(address2.clone(), ConnectionId::from(1usize)),
            )
            .unwrap();
        assert!(std::matches!(
            emit_event,
            ConnectionEstablishedResult::Accept
        ));

        let peers = manager.peers.read();
        let context = peers.get(&peer).unwrap();

        match &context.state {
            PeerState::Connected {
                dial_record: None, ..
            } => {
                let seconary_connection = context.secondary_connection.as_ref().unwrap();
                assert_eq!(seconary_connection.address(), &address2);
                assert_eq!(seconary_connection.score(), SCORE_CONNECT_SUCCESS);
            }
            state => panic!("invalid state: {state:?}"),
        }
        drop(peers);

        // close the secondary connection and verify that the peer remains connected
        let emit_event = manager.on_connection_closed(peer, ConnectionId::from(1usize)).unwrap();
        assert!(emit_event.is_none());

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

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));

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
                peer,
                &Endpoint::listener(address1.clone(), ConnectionId::from(0usize)),
            )
            .unwrap();
        assert!(std::matches!(
            emit_event,
            ConnectionEstablishedResult::Accept
        ));

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
                peer,
                &Endpoint::dialer(address2.clone(), ConnectionId::from(1usize)),
            )
            .unwrap();
        assert!(std::matches!(
            emit_event,
            ConnectionEstablishedResult::Accept
        ));

        let peers = manager.peers.read();
        let context = peers.get(&peer).unwrap();

        match &context.state {
            PeerState::Connected {
                dial_record: None, ..
            } => {
                let seconary_connection = context.secondary_connection.as_ref().unwrap();
                assert_eq!(seconary_connection.address(), &address2);
                assert_eq!(seconary_connection.score(), SCORE_CONNECT_SUCCESS);
            }
            state => panic!("invalid state: {state:?}"),
        }
        drop(peers);

        // close the primary connection and verify that the peer remains connected
        // while the primary connection address is stored in peer addresses
        let emit_event = manager.on_connection_closed(peer, ConnectionId::from(0usize)).unwrap();
        assert!(emit_event.is_none());

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

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));

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
                peer,
                &Endpoint::listener(address1, ConnectionId::from(0usize)),
            )
            .unwrap();
        assert!(std::matches!(
            emit_event,
            ConnectionEstablishedResult::Accept
        ));

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
                peer,
                &Endpoint::dialer(address2.clone(), ConnectionId::from(1usize)),
            )
            .unwrap();
        assert!(std::matches!(
            emit_event,
            ConnectionEstablishedResult::Accept
        ));

        let peers = manager.peers.read();
        let context = peers.get(&peer).unwrap();

        match &context.state {
            PeerState::Connected {
                dial_record: None, ..
            } => {
                let seconary_connection = context.secondary_connection.as_ref().unwrap();
                assert_eq!(seconary_connection.address(), &address2);
                assert_eq!(seconary_connection.score(), SCORE_CONNECT_SUCCESS);
            }
            state => panic!("invalid state: {state:?}"),
        }
        drop(peers);

        // third connection is established, verify that it's discarded
        let emit_event = manager
            .on_connection_established(
                peer,
                &Endpoint::listener(address3.clone(), ConnectionId::from(2usize)),
            )
            .unwrap();
        assert!(std::matches!(
            emit_event,
            ConnectionEstablishedResult::Reject
        ));

        let peers = manager.peers.read();
        let context = peers.get(&peer).unwrap();
        assert!(context.addresses.contains(&address3));
        drop(peers);

        // close the tertiary connection that was ignored
        let emit_event = manager.on_connection_closed(peer, ConnectionId::from(2usize)).unwrap();
        assert!(emit_event.is_none());

        // verify that the state remains unchanged
        let peers = manager.peers.read();
        let context = peers.get(&peer).unwrap();

        match &context.state {
            PeerState::Connected {
                dial_record: None, ..
            } => {
                let seconary_connection = context.secondary_connection.as_ref().unwrap();
                assert_eq!(seconary_connection.address(), &address2);
                assert_eq!(seconary_connection.score(), SCORE_CONNECT_SUCCESS);
            }
            state => panic!("invalid state: {state:?}"),
        }
        drop(peers);
    }

    #[tokio::test]
    #[cfg(debug_assertions)]
    #[should_panic]
    async fn dial_failure_for_unknow_connection() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );

        manager.on_dial_failure(ConnectionId::random()).unwrap();
    }

    #[tokio::test]
    #[cfg(debug_assertions)]
    #[should_panic]
    async fn dial_failure_for_unknow_peer() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let connection_id = ConnectionId::random();
        let peer = PeerId::random();
        manager.pending_connections.insert(connection_id, peer);
        manager.on_dial_failure(connection_id).unwrap();
    }

    #[tokio::test]
    #[cfg(debug_assertions)]
    #[should_panic]
    async fn connection_closed_for_unknown_peer() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        manager.on_connection_closed(PeerId::random(), ConnectionId::random()).unwrap();
    }

    #[tokio::test]
    #[cfg(debug_assertions)]
    #[should_panic]
    async fn unknown_connection_opened() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        manager
            .on_connection_opened(
                SupportedTransport::Tcp,
                ConnectionId::random(),
                Multiaddr::empty(),
            )
            .unwrap();
    }

    #[tokio::test]
    #[cfg(debug_assertions)]
    #[should_panic]
    async fn connection_opened_for_unknown_peer() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let connection_id = ConnectionId::random();
        let peer = PeerId::random();

        manager.pending_connections.insert(connection_id, peer);
        manager
            .on_connection_opened(SupportedTransport::Tcp, connection_id, Multiaddr::empty())
            .unwrap();
    }

    #[tokio::test]
    #[cfg(debug_assertions)]
    #[should_panic]
    async fn connection_established_for_wrong_peer() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let connection_id = ConnectionId::random();
        let peer = PeerId::random();

        manager.pending_connections.insert(connection_id, peer);
        manager
            .on_connection_established(
                PeerId::random(),
                &Endpoint::dialer(Multiaddr::empty(), connection_id),
            )
            .unwrap();
    }

    #[tokio::test]
    #[cfg(debug_assertions)]
    #[should_panic]
    async fn open_failure_unknown_connection() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );

        manager
            .on_open_failure(SupportedTransport::Tcp, ConnectionId::random())
            .unwrap();
    }

    #[tokio::test]
    #[cfg(debug_assertions)]
    #[should_panic]
    async fn open_failure_unknown_peer() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let connection_id = ConnectionId::random();
        let peer = PeerId::random();

        manager.pending_connections.insert(connection_id, peer);
        manager.on_open_failure(SupportedTransport::Tcp, connection_id).unwrap();
    }

    #[tokio::test]
    async fn no_transports() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );

        assert!(manager.next().await.is_none());
    }

    #[tokio::test]
    async fn dial_already_connected_peer() {
        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );

        let peer = {
            let peer = PeerId::random();
            let mut peers = manager.peers.write();

            peers.insert(
                peer,
                PeerContext {
                    state: PeerState::Connected {
                        record: AddressRecord::from_multiaddr(
                            Multiaddr::empty()
                                .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                                .with(Protocol::Tcp(8888))
                                .with(Protocol::P2p(Multihash::from(peer))),
                        )
                        .unwrap(),
                        dial_record: None,
                    },
                    secondary_connection: None,
                    addresses: AddressStore::from_iter(
                        vec![Multiaddr::empty()
                            .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                            .with(Protocol::Tcp(8888))
                            .with(Protocol::P2p(Multihash::from(peer)))]
                        .into_iter(),
                    ),
                },
            );
            drop(peers);

            peer
        };

        match manager.dial(peer).await {
            Err(Error::AlreadyConnected) => {}
            _ => panic!("invalid return value"),
        }
    }

    #[tokio::test]
    async fn peer_already_being_dialed() {
        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );

        let peer = {
            let peer = PeerId::random();
            let mut peers = manager.peers.write();

            peers.insert(
                peer,
                PeerContext {
                    state: PeerState::Dialing {
                        record: AddressRecord::from_multiaddr(
                            Multiaddr::empty()
                                .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                                .with(Protocol::Tcp(8888))
                                .with(Protocol::P2p(Multihash::from(peer))),
                        )
                        .unwrap(),
                    },
                    secondary_connection: None,
                    addresses: AddressStore::from_iter(
                        vec![Multiaddr::empty()
                            .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                            .with(Protocol::Tcp(8888))
                            .with(Protocol::P2p(Multihash::from(peer)))]
                        .into_iter(),
                    ),
                },
            );
            drop(peers);

            peer
        };

        manager.dial(peer).await.unwrap();
    }

    #[tokio::test]
    async fn pending_connection_for_disconnected_peer() {
        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );

        let peer = {
            let peer = PeerId::random();
            let mut peers = manager.peers.write();

            peers.insert(
                peer,
                PeerContext {
                    state: PeerState::Disconnected {
                        dial_record: Some(
                            AddressRecord::from_multiaddr(
                                Multiaddr::empty()
                                    .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                                    .with(Protocol::Tcp(8888))
                                    .with(Protocol::P2p(Multihash::from(peer))),
                            )
                            .unwrap(),
                        ),
                    },
                    secondary_connection: None,
                    addresses: AddressStore::new(),
                },
            );
            drop(peers);

            peer
        };

        manager.dial(peer).await.unwrap();
    }

    #[tokio::test]
    async fn dial_address_invalid_transport() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );

        // transport doesn't start with ip/dns
        {
            let address = Multiaddr::empty().with(Protocol::P2p(Multihash::from(PeerId::random())));
            match manager.dial_address(address.clone()).await {
                Err(Error::TransportNotSupported(dial_address)) => {
                    assert_eq!(dial_address, address);
                }
                _ => panic!("invalid return value"),
            }
        }

        {
            // upd-based protocol but not quic
            let address = Multiaddr::empty()
                .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                .with(Protocol::Udp(8888))
                .with(Protocol::Utp)
                .with(Protocol::P2p(Multihash::from(PeerId::random())));
            match manager.dial_address(address.clone()).await {
                Err(Error::TransportNotSupported(dial_address)) => {
                    assert_eq!(dial_address, address);
                }
                res => panic!("invalid return value: {res:?}"),
            }
        }

        // not tcp nor udp
        {
            let address = Multiaddr::empty()
                .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                .with(Protocol::Sctp(8888))
                .with(Protocol::P2p(Multihash::from(PeerId::random())));
            match manager.dial_address(address.clone()).await {
                Err(Error::TransportNotSupported(dial_address)) => {
                    assert_eq!(dial_address, address);
                }
                _ => panic!("invalid return value"),
            }
        }

        // random protocol after tcp
        {
            let address = Multiaddr::empty()
                .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                .with(Protocol::Tcp(8888))
                .with(Protocol::Utp)
                .with(Protocol::P2p(Multihash::from(PeerId::random())));
            match manager.dial_address(address.clone()).await {
                Err(Error::TransportNotSupported(dial_address)) => {
                    assert_eq!(dial_address, address);
                }
                _ => panic!("invalid return value"),
            }
        }
    }

    #[tokio::test]
    async fn dial_address_peer_id_missing() {
        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );

        async fn call_manager(manager: &mut TransportManager, address: Multiaddr) {
            match manager.dial_address(address).await {
                Err(Error::AddressError(AddressError::PeerIdMissing)) => {}
                _ => panic!("invalid return value"),
            }
        }

        {
            call_manager(
                &mut manager,
                Multiaddr::empty()
                    .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                    .with(Protocol::Tcp(8888)),
            )
            .await;
        }

        {
            call_manager(
                &mut manager,
                Multiaddr::empty()
                    .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                    .with(Protocol::Tcp(8888))
                    .with(Protocol::Wss(std::borrow::Cow::Owned("".to_string()))),
            )
            .await;
        }

        {
            call_manager(
                &mut manager,
                Multiaddr::empty()
                    .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                    .with(Protocol::Udp(8888))
                    .with(Protocol::QuicV1),
            )
            .await;
        }
    }

    #[tokio::test]
    async fn inbound_connection_while_dialing() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let peer = PeerId::random();
        let dial_address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));

        let connection_id = ConnectionId::random();
        let transport = Box::new({
            let mut transport = DummyTransport::new();
            transport.inject_event(TransportEvent::ConnectionEstablished {
                peer,
                endpoint: Endpoint::listener(dial_address.clone(), connection_id),
            });
            transport
        });
        manager.register_transport(SupportedTransport::Tcp, transport);
        manager.add_known_address(
            peer,
            vec![Multiaddr::empty()
                .with(Protocol::Ip4(Ipv4Addr::new(192, 168, 1, 5)))
                .with(Protocol::Tcp(8888))
                .with(Protocol::P2p(Multihash::from(peer)))]
            .into_iter(),
        );

        assert!(manager.dial(peer).await.is_ok());
        assert!(!manager.pending_connections.is_empty());

        {
            let peers = manager.peers.read();

            match peers.get(&peer) {
                Some(PeerContext {
                    state: PeerState::Opening { .. },
                    ..
                }) => {}
                state => panic!("invalid state for peer: {state:?}"),
            }
        }

        match manager.next().await.unwrap() {
            TransportEvent::ConnectionEstablished {
                peer: event_peer,
                endpoint: event_endpoint,
                ..
            } => {
                assert_eq!(peer, event_peer);
                assert_eq!(
                    event_endpoint,
                    Endpoint::listener(dial_address.clone(), connection_id),
                );
            }
            event => panic!("invalid event: {event:?}"),
        }
        assert!(manager.pending_connections.is_empty());

        let peers = manager.peers.read();
        match peers.get(&peer).unwrap() {
            PeerContext {
                state:
                    PeerState::Connected {
                        record,
                        dial_record,
                    },
                secondary_connection,
                addresses,
            } => {
                assert!(!addresses.contains(record.address()));
                assert!(dial_record.is_none());
                assert!(secondary_connection.is_none());
                assert_eq!(record.address(), &dial_address);
                assert_eq!(record.connection_id(), &Some(connection_id));
            }
            state => panic!("invalid peer state: {state:?}"),
        }
    }

    #[tokio::test]
    async fn inbound_connection_for_same_address_while_dialing() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let peer = PeerId::random();
        let dial_address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));

        let connection_id = ConnectionId::random();
        let transport = Box::new({
            let mut transport = DummyTransport::new();
            transport.inject_event(TransportEvent::ConnectionEstablished {
                peer,
                endpoint: Endpoint::listener(dial_address.clone(), connection_id),
            });
            transport
        });
        manager.register_transport(SupportedTransport::Tcp, transport);
        manager.add_known_address(
            peer,
            vec![Multiaddr::empty()
                .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
                .with(Protocol::Tcp(8888))
                .with(Protocol::P2p(Multihash::from(peer)))]
            .into_iter(),
        );

        assert!(manager.dial(peer).await.is_ok());
        assert!(!manager.pending_connections.is_empty());

        {
            let peers = manager.peers.read();

            match peers.get(&peer) {
                Some(PeerContext {
                    state: PeerState::Opening { .. },
                    ..
                }) => {}
                state => panic!("invalid state for peer: {state:?}"),
            }
        }

        match manager.next().await.unwrap() {
            TransportEvent::ConnectionEstablished {
                peer: event_peer,
                endpoint: event_endpoint,
                ..
            } => {
                assert_eq!(peer, event_peer);
                assert_eq!(
                    event_endpoint,
                    Endpoint::listener(dial_address.clone(), connection_id),
                );
            }
            event => panic!("invalid event: {event:?}"),
        }
        assert!(manager.pending_connections.is_empty());

        let peers = manager.peers.read();
        match peers.get(&peer).unwrap() {
            PeerContext {
                state:
                    PeerState::Connected {
                        record,
                        dial_record,
                    },
                secondary_connection,
                addresses,
            } => {
                assert!(addresses.is_empty());
                assert!(dial_record.is_none());
                assert!(secondary_connection.is_none());
                assert_eq!(record.address(), &dial_address);
                assert_eq!(record.connection_id(), &Some(connection_id));
            }
            state => panic!("invalid peer state: {state:?}"),
        }
    }
}
