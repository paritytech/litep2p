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
    protocol::{InnerTransportEvent, ProtocolSet, TransportService},
    types::{protocol::ProtocolName, ConnectionId},
    BandwidthSink, PeerId,
};

use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;
use parking_lot::RwLock;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

// TODO: store `Multiaddr` in `Arc`

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::transport-manager";

/// Supported protocols.
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum SupportedTransport {
    /// TCP.
    Tcp,

    /// QUIC.
    Quic,

    /// WebRTC
    WebRtc,

    /// WebSocket
    WebSocket,
}

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

        /// Remote address.
        address: Multiaddr,
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

/// Inner commands sent from [`TransportManagerHandle`] to [`TransportManager`].
#[derive(Debug)]
pub enum InnerTransportManagerCommand {
    /// Dial peer.
    DialPeer {
        /// Remote peer ID.
        peer: PeerId,
    },

    /// Dial address.
    DialAddress {
        /// Remote address.
        address: Multiaddr,
    },
}

#[derive(Debug)]
pub enum TransportManagerCommand {
    Dial {
        /// Dial remote peer at `address`
        address: Multiaddr,

        /// Connection ID.
        connection: ConnectionId,
    },
}

/// Handle for communicating with [`TransportManager`].
#[derive(Debug, Clone)]
pub struct TransportManagerHandle {
    /// Peers.
    peers: Arc<RwLock<HashMap<PeerId, PeerContext>>>,

    /// TX channel for sending commands to [`TransportManager`].
    cmd_tx: Sender<InnerTransportManagerCommand>,
}

impl TransportManagerHandle {
    /// Create new [`TransportManagerHandle`].
    pub fn new(
        peers: Arc<RwLock<HashMap<PeerId, PeerContext>>>,
        cmd_tx: Sender<InnerTransportManagerCommand>,
    ) -> Self {
        Self { peers, cmd_tx }
    }

    /// Add one or more known addresses for peer.
    ///
    /// If peer doesn't exist, it will be added to known peers.
    pub fn add_known_address(&mut self, peer: &PeerId, addresses: impl Iterator<Item = Multiaddr>) {
        let mut peers = self.peers.write();

        match peers.get_mut(&peer) {
            Some(context) => context.addresses.extend(addresses),
            None => {
                peers.insert(
                    *peer,
                    PeerContext {
                        state: PeerState::Disconnected,
                        addresses: HashSet::from_iter(addresses),
                    },
                );
            }
        }
    }

    /// Dial peer using `PeerId`.
    ///
    /// Returns an error if the peer is unknown or the peer is already connected.
    // TODO: this must report some tokent to the caller so `DialFailure` can be reported to them
    pub async fn dial(&self, peer: &PeerId) -> crate::Result<()> {
        {
            match self.peers.read().get(&peer) {
                Some(PeerContext {
                    state: PeerState::Connected(_),
                    ..
                }) => return Err(Error::AlreadyConnected),
                Some(PeerContext {
                    state: PeerState::Disconnected,
                    addresses,
                }) if addresses.is_empty() => return Err(Error::NoAddressAvailable(*peer)),
                Some(PeerContext {
                    state: PeerState::Dialing(_),
                    ..
                }) => return Ok(()),
                None => return Err(Error::PeerDoesntExist(*peer)),
                _ => {}
            }
        }

        self.cmd_tx
            .send(InnerTransportManagerCommand::DialPeer { peer: *peer })
            .await
            .map_err(From::from)
    }

    /// Dial peer using `Multiaddr`.
    ///
    /// Returns an error if address it not valid.
    pub async fn dial_address(&self, address: Multiaddr) -> crate::Result<()> {
        if !address.iter().any(|protocol| std::matches!(protocol, Protocol::P2p(_))) {
            return Err(Error::AddressError(AddressError::PeerIdMissing));
        }

        self.cmd_tx
            .send(InnerTransportManagerCommand::DialAddress { address })
            .await
            .map_err(From::from)
    }
}

// TODO: add getters for these
#[derive(Debug)]
pub struct TransportHandle {
    pub keypair: Keypair,
    pub tx: Sender<TransportManagerEvent>,
    pub rx: Receiver<TransportManagerCommand>,
    pub protocols: HashMap<ProtocolName, ProtocolContext>,
    pub next_connection_id: Arc<AtomicUsize>,
    pub next_substream_id: Arc<AtomicUsize>,
    pub protocol_names: Vec<ProtocolName>,
    pub bandwidth_sink: BandwidthSink,
}

impl TransportHandle {
    pub fn protocol_set(&self) -> ProtocolSet {
        ProtocolSet::new(
            self.keypair.clone(),
            self.tx.clone(),
            self.next_substream_id.clone(),
            self.protocols.clone(),
        )
    }

    /// Get next connection ID.
    pub fn next_connection_id(&mut self) -> ConnectionId {
        let connection_id = self.next_connection_id.fetch_add(1usize, Ordering::Relaxed);

        ConnectionId::from(connection_id)
    }

    pub async fn _report_connection_established(
        &mut self,
        connection: ConnectionId,
        peer: PeerId,
        address: Multiaddr,
    ) {
        let _ = self
            .tx
            .send(TransportManagerEvent::ConnectionEstablished {
                connection,
                peer,
                address,
            })
            .await;
    }

    /// Report to `Litep2p` that a peer disconnected.
    pub async fn _report_connection_closed(&mut self, peer: PeerId, connection: ConnectionId) {
        let _ = self.tx.send(TransportManagerEvent::ConnectionClosed { peer, connection }).await;
    }

    /// Report to `Litep2p` that dialing a remote peer failed.
    pub async fn report_dial_failure(
        &mut self,
        connection: ConnectionId,
        address: Multiaddr,
        error: Error,
    ) {
        tracing::debug!(target: LOG_TARGET, ?connection, ?address, ?error, "dial failure");

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

        let _ = self
            .tx
            .send(TransportManagerEvent::DialFailure {
                connection,
                address,
                error,
            })
            .await;
    }

    /// Get next transport command.
    pub async fn next(&mut self) -> Option<TransportManagerCommand> {
        self.rx.recv().await
    }
}

struct TransportContext {
    tx: Sender<TransportManagerCommand>,
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

/// Peer state.
#[derive(Debug)]
enum PeerState {
    /// `Litep2p` is connected to peer.
    Connected(Multiaddr),

    /// Peer is being dialed.
    Dialing(Multiaddr),

    /// `Litep2p` is not connected to peer.
    Disconnected,
}

/// Peer context.
#[derive(Debug)]
pub struct PeerContext {
    /// Peer state.
    state: PeerState,

    /// Known addresses of peer.
    addresses: HashSet<Multiaddr>,
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
    transports: HashMap<SupportedTransport, TransportContext>,

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
    pub fn new(keypair: Keypair, bandwidth_sink: BandwidthSink) -> (Self, TransportManagerHandle) {
        let local_peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let peers = Arc::new(RwLock::new(HashMap::new()));
        let (cmd_tx, cmd_rx) = channel(256);
        let (event_tx, event_rx) = channel(256);
        let handle = TransportManagerHandle::new(peers.clone(), cmd_tx);

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
                transports: HashMap::new(),
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

    /// Get iterato to installed protocols.
    pub fn protocols(&self) -> impl Iterator<Item = &ProtocolName> {
        self.protocols.keys()
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

    /// Register transport protocol to [`TransportManager`].
    pub fn register_transport(&mut self, transport: SupportedTransport) -> TransportHandle {
        assert!(!self.transports.contains_key(&transport));

        let (tx, rx) = channel(256);
        self.transports.insert(transport, TransportContext { tx });

        TransportHandle {
            rx,
            tx: self.event_tx.clone(),
            keypair: self.keypair.clone(),
            protocols: self.protocols.clone(),
            bandwidth_sink: self.bandwidth_sink.clone(),
            protocol_names: self.protocol_names.iter().cloned().collect(),
            next_substream_id: self.next_substream_id.clone(),
            next_connection_id: self.next_connection_id.clone(),
        }
    }

    /// Register local listen address.
    pub fn register_listen_address(&mut self, address: Multiaddr) {
        self.listen_addresses.insert(address.clone());
        self.listen_addresses.insert(address.with(Protocol::P2p(
            Multihash::from_bytes(&self.local_peer_id.to_bytes()).unwrap(),
        )));
    }

    /// Add one or more known addresses for `peer`.
    pub fn add_known_address(&mut self, peer: PeerId, address: impl Iterator<Item = Multiaddr>) {
        self.transport_manager_handle.add_known_address(&peer, address);
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
                state: PeerState::Connected(_),
                ..
            }) => return Err(Error::AlreadyConnected),
            Some(PeerContext {
                state: PeerState::Dialing(_),
                ..
            }) => return Ok(()),
            Some(PeerContext {
                state: PeerState::Disconnected,
                addresses,
            }) => {
                let next_address =
                    addresses.iter().next().ok_or(Error::NoAddressAvailable(*peer))?.clone();
                addresses.remove(&next_address);

                next_address
            }
        };

        self.dial_address(address).await
    }

    /// Dial peer using `Multiaddr`.
    ///
    /// Returns an error if address it not valid.
    pub async fn dial_address(&mut self, address: Multiaddr) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?address, "dial remote peer");

        if self.listen_addresses.contains(&address) {
            return Err(Error::TriedToDialSelf);
        }

        let mut protocol_stack = address.iter();
        match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
        {
            Protocol::Ip4(_) | Protocol::Ip6(_) => {}
            Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_) => {}
            transport => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?transport,
                    "invalid transport, expected `ip4`/`ip6`"
                );
                return Err(Error::TransportNotSupported(address));
            }
        };

        let (supported_transport, remote_peer_id) = match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
        {
            Protocol::Tcp(_) => match protocol_stack.next() {
                Some(Protocol::Ws(_)) | Some(Protocol::Wss(_)) => match protocol_stack.next() {
                    Some(Protocol::P2p(hash)) => (
                        SupportedTransport::WebSocket,
                        PeerId::from_multihash(hash).map_err(|_| Error::InvalidData)?,
                    ),
                    _ => {
                        tracing::debug!(target: LOG_TARGET, ?address, "peer id missing");
                        return Err(Error::TransportNotSupported(address.clone()));
                    }
                },
                Some(Protocol::P2p(hash)) => (
                    SupportedTransport::Tcp,
                    PeerId::from_multihash(hash).map_err(|_| Error::InvalidData)?,
                ),
                _ => {
                    tracing::debug!(target: LOG_TARGET, ?address, "peer id missing");
                    return Err(Error::TransportNotSupported(address.clone()));
                }
            },
            Protocol::Udp(_) => match protocol_stack
                .next()
                .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
            {
                Protocol::QuicV1 => match protocol_stack.next() {
                    Some(Protocol::P2p(hash)) => (
                        SupportedTransport::Quic,
                        PeerId::from_multihash(hash).map_err(|_| Error::InvalidData)?,
                    ),
                    _ => {
                        tracing::debug!(target: LOG_TARGET, ?address, "peer id missing");
                        return Err(Error::TransportNotSupported(address.clone()));
                    }
                },
                _ => {
                    tracing::debug!(target: LOG_TARGET, ?address, "expected `quic-v1`");
                    return Err(Error::TransportNotSupported(address.clone()));
                }
            },
            protocol => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `tcp`"
                );

                return Err(Error::TransportNotSupported(address));
            }
        };

        {
            let mut peers = self.peers.write();

            match peers.get_mut(&remote_peer_id) {
                None => {
                    drop(peers);
                    self.peers.write().insert(
                        remote_peer_id,
                        PeerContext {
                            state: PeerState::Dialing(address.clone()),
                            addresses: HashSet::from_iter(vec![address.clone()].into_iter()),
                        },
                    );
                }
                Some(PeerContext {
                    state: PeerState::Dialing(_) | PeerState::Connected(_),
                    ..
                }) => return Ok(()),
                Some(PeerContext {
                    ref mut state,
                    addresses,
                }) => {
                    addresses.insert(address.clone());
                    *state = PeerState::Dialing(address.clone());
                }
            }
        }

        let connection = self.next_connection_id();
        let _ = self
            .transports
            .get_mut(&supported_transport)
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
            .tx
            .send(TransportManagerCommand::Dial {
                address: address.clone(),
                connection,
            })
            .await?;
        self.pending_connections.insert(connection, remote_peer_id);

        Ok(())
    }

    /// Handle transport manager event.
    fn on_transport_manager_event(
        &mut self,
        event: TransportManagerEvent,
    ) -> TransportManagerEvent {
        match &event {
            TransportManagerEvent::DialFailure {
                address,
                connection,
                error,
            } => match self.pending_connections.remove(&connection) {
                None => {
                    tracing::error!(target: LOG_TARGET, "dial failed for a connection that doesn't exist");
                    debug_assert!(false);
                    event
                }
                Some(peer) => {
                    tracing::debug!(target: LOG_TARGET, ?peer, ?address, ?error, "dial failure");

                    if let Some(context) = self.peers.write().get_mut(&peer) {
                        context.state = PeerState::Disconnected;
                    }

                    event
                }
            },
            TransportManagerEvent::ConnectionEstablished {
                peer,
                connection,
                address,
            } => {
                // TODO: remove duplicate code
                match self.pending_connections.remove(&connection) {
                    Some(dialed_peer) => {
                        if &dialed_peer != peer {
                            tracing::warn!(target: LOG_TARGET, ?dialed_peer, ?peer, "peer IDs do not match");
                            // TODO: which peer ID should be reported to the protocol?
                            todo!();
                        }

                        match self.peers.write().get_mut(&dialed_peer) {
                            Some(context) => {
                                context.state = PeerState::Connected(address.clone());
                                context.addresses.insert(address.clone());
                            }
                            None => {
                                self.peers.write().insert(
                                    *peer,
                                    PeerContext {
                                        state: PeerState::Connected(address.clone()),
                                        addresses: HashSet::from_iter(
                                            vec![address.clone()].into_iter(),
                                        ),
                                    },
                                );
                            }
                        }
                    }
                    None => {
                        self.peers.write().insert(
                            *peer,
                            PeerContext {
                                state: PeerState::Connected(address.clone()),
                                addresses: HashSet::from_iter(vec![address.clone()].into_iter()),
                            },
                        );
                    }
                }

                event
            }
            TransportManagerEvent::ConnectionClosed { peer, .. } => {
                if let Some(context) = self.peers.write().get_mut(peer) {
                    context.state = PeerState::Disconnected;
                }
                event
            }
        }
    }

    /// Poll next event from [`TransportManager`].
    pub async fn next(&mut self) -> Option<TransportManagerEvent> {
        loop {
            tokio::select! {
                event = self.event_rx.recv() => return Some(self.on_transport_manager_event(event?)),
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
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::ed25519::Keypair;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn duplicate_protocol() {
        let sink = BandwidthSink::new();
        let (mut manager, _handle) = TransportManager::new(Keypair::generate(), sink);

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
        let (mut manager, _handle) = TransportManager::new(Keypair::generate(), sink);

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
        let (mut manager, _handle) = TransportManager::new(Keypair::generate(), sink);

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
        let (mut manager, _handle) = TransportManager::new(Keypair::generate(), sink);

        manager.register_transport(SupportedTransport::Tcp);
        manager.register_transport(SupportedTransport::Tcp);
    }

    #[tokio::test]
    async fn tried_to_self_using_peer_id() {
        let keypair = Keypair::generate();
        let local_peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let sink = BandwidthSink::new();
        let (mut manager, _handle) = TransportManager::new(keypair, sink);

        assert!(manager.dial(&local_peer_id).await.is_err());
    }

    #[tokio::test]
    async fn try_to_dial_over_disabled_transport() {
        let sink = BandwidthSink::new();
        let (mut manager, _handle) = TransportManager::new(Keypair::generate(), sink);
        let _handle = manager.register_transport(SupportedTransport::Tcp);

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

        let sink = BandwidthSink::new();
        let (mut manager, _handle) = TransportManager::new(Keypair::generate(), sink);
        let mut handle = manager.register_transport(SupportedTransport::Tcp);

        let peer = PeerId::random();
        let dial_address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));

        assert!(manager.dial_address(dial_address.clone()).await.is_ok());
        assert!(!manager.pending_connections.is_empty());

        match handle.next().await {
            Some(TransportManagerCommand::Dial {
                address,
                connection,
            }) => {
                assert_eq!(address, dial_address);
                assert_eq!(connection, ConnectionId::from(0usize));
            }
            _ => panic!("invalid command received"),
        }

        handle
            ._report_connection_established(ConnectionId::from(0usize), peer, dial_address)
            .await;
    }

    #[tokio::test]
    async fn try_to_dial_same_peer_twice() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let sink = BandwidthSink::new();
        let (mut manager, _handle) = TransportManager::new(Keypair::generate(), sink);
        let _handle = manager.register_transport(SupportedTransport::Tcp);

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

        let sink = BandwidthSink::new();
        let (mut manager, _handle) = TransportManager::new(Keypair::generate(), sink);
        let _handle = manager.register_transport(SupportedTransport::Tcp);

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

        let sink = BandwidthSink::new();
        let (mut manager, _handle) = TransportManager::new(Keypair::generate(), sink);
        let _handle = manager.register_transport(SupportedTransport::Tcp);

        assert!(manager.dial(&PeerId::random()).await.is_err());
    }

    #[tokio::test]
    async fn dial_non_peer_with_no_known_addresses() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let sink = BandwidthSink::new();
        let (mut manager, _handle) = TransportManager::new(Keypair::generate(), sink);
        let _handle = manager.register_transport(SupportedTransport::Tcp);

        let peer = PeerId::random();
        manager.peers.write().insert(
            peer,
            PeerContext {
                state: PeerState::Disconnected,
                addresses: HashSet::new(),
            },
        );

        assert!(manager.dial(&peer).await.is_err());
    }
}
