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
    error::Error,
    peer_id::PeerId,
    protocol::{InnerTransportEvent, ProtocolSet, TransportService},
    types::{protocol::ProtocolName, ConnectionId},
};

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;
use parking_lot::RwLock;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use trust_dns_resolver::{
    config::{ResolverConfig, ResolverOpts},
    error::ResolveError,
    lookup_ip::LookupIp,
    AsyncResolver,
};

use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

/// Logging target for the file.
const LOG_TARGET: &str = "transport-manager";

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
    pub fn add_know_address(&mut self, peer: &PeerId, addresses: impl Iterator<Item = Multiaddr>) {
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
    pub async fn dial(&self, peer: &PeerId) -> crate::Result<()> {
        {
            match self.peers.read().get(&peer) {
                Some(PeerContext {
                    state: PeerState::Connected(_),
                    ..
                }) => return Err(Error::AlreadyConnected),
                Some(PeerContext {
                    state: PeerState::Connected(_),
                    addresses,
                }) if addresses.is_empty() => return Err(Error::NoAddressAvailable(*peer)),
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
    pub async fn _report_connection_closed(&mut self, peer: PeerId) {
        let _ = self
            .tx
            .send(TransportManagerEvent::ConnectionClosed { peer })
            .await;
    }

    /// Report to `Litep2p` that dialing a remote peer failed.
    pub async fn report_dial_failure(
        &mut self,
        connection: ConnectionId,
        address: Multiaddr,
        error: Error,
    ) {
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
}

impl ProtocolContext {
    /// Create new [`ProtocolContext`].
    fn new(codec: ProtocolCodec, tx: Sender<InnerTransportEvent>) -> Self {
        Self { tx, codec }
    }
}

/// Peer state.
#[derive(Debug)]
enum PeerState {
    /// `Litep2p` is connected to peer.
    Connected(Multiaddr),

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

    /// Installed protocols.
    protocols: HashMap<ProtocolName, ProtocolContext>,

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
    pending_connections: HashMap<ConnectionId, Multiaddr>,

    /// Pending DNS resolves.
    pending_dns_resolves: FuturesUnordered<
        BoxFuture<'static, (ConnectionId, Multiaddr, Result<LookupIp, ResolveError>)>,
    >,
}

impl TransportManager {
    /// Create new [`TransportManager`].
    // TODO: don't return handle here
    pub fn new(keypair: Keypair) -> (Self, TransportManagerHandle) {
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
                protocols: HashMap::new(),
                transports: HashMap::new(),
                listen_addresses: HashSet::new(),
                transport_manager_handle: handle.clone(),
                pending_connections: HashMap::new(),
                pending_dns_resolves: FuturesUnordered::new(),
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
        codec: ProtocolCodec,
    ) -> TransportService {
        let (service, sender) = TransportService::new(
            self.local_peer_id,
            protocol.clone(),
            self.next_substream_id.clone(),
            self.transport_manager_handle.clone(),
        );

        self.protocols
            .insert(protocol, ProtocolContext::new(codec, sender));

        service
    }

    /// Register transport protocol to [`TransportManager`].
    pub fn register_transport(&mut self, transport: SupportedTransport) -> TransportHandle {
        let (tx, rx) = channel(256);
        self.transports.insert(transport, TransportContext { tx });

        TransportHandle {
            rx,
            tx: self.event_tx.clone(),
            keypair: self.keypair.clone(),
            protocols: self.protocols.clone(),
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

    /// Dial peer using `PeerId`.
    ///
    /// Returns an error if the peer is unknown or the peer is already connected.
    pub async fn dial(&mut self, peer: &PeerId) -> crate::Result<()> {
        if peer == &self.local_peer_id {
            return Err(Error::TriedToDialSelf);
        }

        // TODO: implement
        Ok(())
    }

    /// Dial peer using `Multiaddr`.
    ///
    /// Returns an error if address it not valid.
    pub async fn dial_address(&mut self, address: Multiaddr) -> crate::Result<()> {
        if self.listen_addresses.contains(&address) {
            return Err(Error::TriedToDialSelf);
        }

        let mut protocol_stack = address.iter();

        match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
        {
            Protocol::Ip4(_) | Protocol::Ip6(_) => {}
            Protocol::Dns(addr) | Protocol::Dns4(addr) | Protocol::Dns6(addr) => {
                let dns_address = addr.to_string();
                let original = address.clone();
                let connection = self.next_connection_id();

                self.pending_dns_resolves.push(Box::pin(async move {
                    match AsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default()) {
                        Ok(resolver) => {
                            (connection, original, resolver.lookup_ip(dns_address).await)
                        }
                        Err(error) => (connection, original, Err(error)),
                    }
                }));

                return Ok(());
            }
            transport => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?transport,
                    "invalid transport, expected `ip4`/`ip6`"
                );
                return Err(Error::TransportNotSupported(address));
            }
        }

        let supported_transport = match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
        {
            Protocol::Tcp(_) => match protocol_stack.next() {
                Some(Protocol::Ws(_)) => SupportedTransport::WebSocket,
                _ => SupportedTransport::Tcp,
            },
            Protocol::Udp(_) => match protocol_stack
                .next()
                .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
            {
                Protocol::QuicV1 => SupportedTransport::Quic,
                _ => return Err(Error::TransportNotSupported(address.clone())),
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

        let connection =
            ConnectionId::from(self.next_connection_id.fetch_add(1usize, Ordering::Relaxed));
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
        self.pending_connections.insert(connection, address);

        Ok(())
    }

    /// Handle resolved DNS address.
    async fn on_resolved_dns_address(
        &mut self,
        address: Multiaddr,
        result: Result<LookupIp, ResolveError>,
    ) -> crate::Result<Multiaddr> {
        tracing::trace!(
            target: LOG_TARGET,
            ?address,
            ?result,
            "dns address resolved"
        );

        let Ok(resolved) = result else {
            return Err(Error::DnsAddressResolutionFailed);
        };

        let mut address_iter = resolved.iter();
        let mut protocol_stack = address.into_iter();
        let mut new_address = Multiaddr::empty();

        match protocol_stack.next().expect("entry to exist") {
            Protocol::Dns4(_) => match address_iter.next() {
                Some(IpAddr::V4(inner)) => {
                    new_address.push(Protocol::Ip4(inner));
                }
                _ => return Err(Error::TransportNotSupported(address)),
            },
            Protocol::Dns6(_) => match address_iter.next() {
                Some(IpAddr::V6(inner)) => {
                    new_address.push(Protocol::Ip6(inner));
                }
                _ => return Err(Error::TransportNotSupported(address)),
            },
            Protocol::Dns(_) => {
                // TODO: zzz
                let mut ip6 = Vec::new();
                let mut ip4 = Vec::new();

                for ip in address_iter {
                    match ip {
                        IpAddr::V4(inner) => ip4.push(inner),
                        IpAddr::V6(inner) => ip6.push(inner),
                    }
                }

                if !ip6.is_empty() {
                    new_address.push(Protocol::Ip6(ip6[0]));
                } else if !ip4.is_empty() {
                    new_address.push(Protocol::Ip4(ip4[0]));
                } else {
                    return Err(Error::TransportNotSupported(address));
                }
            }
            _ => panic!("somehow got invalid dns address"),
        };

        for protocol in protocol_stack {
            new_address.push(protocol);
        }

        Ok(new_address)
    }

    /// Poll next event from [`TransportManager`].
    pub async fn next(&mut self) -> Option<TransportManagerEvent> {
        loop {
            tokio::select! {
                event = self.event_rx.recv() => return event,
                event = self.pending_dns_resolves.select_next_some(), if !self.pending_dns_resolves.is_empty() => {
                    match self.on_resolved_dns_address(event.1.clone(), event.2).await {
                        Ok(address) => {
                            tracing::debug!(target: LOG_TARGET, ?address, "connect to remote peer");

                            // TODO: no unwraps
                            self.dial_address(address.clone()).await.unwrap();
                        }
                        Err(error) => return Some(TransportManagerEvent::DialFailure { connection: event.0, address: event.1, error }),
                    }
                }
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
