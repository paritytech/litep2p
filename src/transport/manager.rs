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
    sync::Arc,
};

/// Logging target for the file.
const LOG_TARGET: &str = "transport-manager";

/// Connection limit.
pub const MAX_CONNECTIONS: usize = 10_000;

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
        /// Dialed address.
        address: Multiaddr,

        /// Error.
        error: Error,
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
    /// Connected peers.
    connected: Arc<RwLock<HashMap<PeerId, HashSet<Multiaddr>>>>,

    /// Known but unconnected peers.
    unconnected: Arc<RwLock<HashMap<PeerId, HashSet<Multiaddr>>>>,
}

impl TransportManagerHandle {
    /// Create new [`TransportManagerHandle`].
    pub fn new(
        connected: Arc<RwLock<HashMap<PeerId, HashSet<Multiaddr>>>>,
        unconnected: Arc<RwLock<HashMap<PeerId, HashSet<Multiaddr>>>>,
    ) -> Self {
        Self {
            connected,
            unconnected,
        }
    }

    /// Add one or more known addresses for peer.
    ///
    /// If peer doesn't exist, it will be added to known peers.
    pub fn add_know_address(&mut self, peer: &PeerId, addresses: impl Iterator<Item = Multiaddr>) {
        let mut connected = self.connected.write();

        match connected.get_mut(&peer) {
            Some(known_addresses) => known_addresses.extend(addresses),
            None => {
                drop(connected);

                let mut unconnected = self.unconnected.write();
                unconnected.entry(*peer).or_default().extend(addresses);
            }
        }
    }

    /// Dial peer using `PeerId`.
    ///
    /// Returns an error if the peer is unknown or the peer is already connected.
    pub async fn dial(&self, peer: &PeerId) -> crate::Result<()> {
        if self.connected.read().contains_key(peer) {
            return Err(Error::AlreadyConnected);
        }
        let unconnected = self.unconnected.read();

        match unconnected.get(&peer) {
            None => return Err(Error::NoAddressAvailable(*peer)),
            Some(addresses) => {
                drop(unconnected);
                todo!("send command to `TransportManager`");
            }
        }
    }

    /// Dial peer using `Multiaddr`.
    ///
    /// Returns an error if address it not valid.
    pub async fn dial_address(&self, address: Multiaddr) -> crate::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct TransportHandle {
    pub keypair: Keypair,
    pub tx: Sender<TransportManagerEvent>,
    pub rx: Receiver<TransportManagerCommand>,
    pub protocols: HashMap<ProtocolName, ProtocolContext>,
}

impl TransportHandle {
    pub fn protocol_set(&self) -> ProtocolSet {
        let (tx, rx) = channel(256);

        ProtocolSet {
            rx,
            tx,
            mgr_tx: self.tx.clone(),
            keypair: self.keypair.clone(),
            protocols: self.protocols.clone(),
        }
    }

    pub async fn report_connection_established(&mut self, peer: PeerId, address: Multiaddr) {
        let _ = self
            .tx
            .send(TransportManagerEvent::ConnectionEstablished { peer, address })
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
    pub async fn report_dial_failure(&mut self, address: Multiaddr, error: Error) {
        let _ = self
            .tx
            .send(TransportManagerEvent::DialFailure { address, error })
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

/// Litep2p connection manager.
pub struct TransportManager {
    /// Local peer ID.
    local_peer_id: PeerId,

    /// Keypair.
    keypair: Keypair,

    /// Installed protocols.
    protocols: HashMap<ProtocolName, ProtocolContext>,

    /// Installed transports.
    transports: HashMap<SupportedTransport, TransportContext>,

    /// Connected peers.
    connected: Arc<RwLock<HashMap<PeerId, HashSet<Multiaddr>>>>,

    /// Known but unconnected peers.
    unconnected: Arc<RwLock<HashMap<PeerId, HashSet<Multiaddr>>>>,

    /// Handle to [`TransportManager`].
    transport_manager_handle: TransportManagerHandle,

    /// RX channel for receiving events from installed transports.
    event_rx: Receiver<TransportManagerEvent>,

    /// TX channel for transport events that is given to installed transports.
    event_tx: Sender<TransportManagerEvent>,

    /// Next connection ID.
    next_connection_id: ConnectionId,

    /// Pending connections.
    pending_connections: HashMap<ConnectionId, Multiaddr>,

    /// Pending DNS resolves.
    pending_dns_resolves:
        FuturesUnordered<BoxFuture<'static, (Multiaddr, Result<LookupIp, ResolveError>)>>,
}

impl TransportManager {
    /// Create new [`TransportManager`].
    // TODO: don't return handle here
    pub fn new(keypair: Keypair) -> (Self, TransportManagerHandle) {
        let local_peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let connected = Arc::new(RwLock::new(HashMap::new()));
        let unconnected = Arc::new(RwLock::new(HashMap::new()));
        let handle = TransportManagerHandle::new(Arc::clone(&connected), Arc::clone(&unconnected));
        let (event_tx, event_rx) = channel(256);

        (
            Self {
                keypair,
                event_tx,
                event_rx,
                connected,
                unconnected,
                local_peer_id,
                protocols: HashMap::new(),
                transports: HashMap::new(),
                next_connection_id: ConnectionId::new(),
                transport_manager_handle: handle.clone(),
                pending_connections: HashMap::new(),
                pending_dns_resolves: FuturesUnordered::new(),
            },
            handle,
        )
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
        }
    }

    /// Dial remote peer over TCP.
    async fn dial_tcp(&mut self, address: Multiaddr) -> crate::Result<ConnectionId> {
        let connection = self.next_connection_id.next();

        let _ = self
            .transports
            .get_mut(&SupportedTransport::Tcp)
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
            .tx
            .send(TransportManagerCommand::Dial {
                address,
                connection,
            })
            .await?;

        Ok(connection)
    }

    /// Dial remote peer over WebSocket.
    async fn dial_ws(&mut self, address: Multiaddr) -> crate::Result<ConnectionId> {
        let connection = self.next_connection_id.next();

        let _ = self
            .transports
            .get_mut(&SupportedTransport::WebSocket)
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
            .tx
            .send(TransportManagerCommand::Dial {
                address,
                connection,
            })
            .await?;

        Ok(connection)
    }

    /// Dial remote peer over QUIC.
    async fn dial_quic(&mut self, address: Multiaddr) -> crate::Result<ConnectionId> {
        let connection = self.next_connection_id.next();

        let _ = self
            .transports
            .get_mut(&SupportedTransport::Quic)
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
            .tx
            .send(TransportManagerCommand::Dial {
                address,
                connection,
            })
            .await?;

        Ok(connection)
    }

    /// Dial peer using `PeerId`.
    ///
    /// Returns an error if the peer is unknown or the peer is already connected.
    pub fn dial(&mut self, peer: &PeerId) -> crate::Result<()> {
        Ok(())
    }

    /// Dial peer using `Multiaddr`.
    ///
    /// Returns an error if address it not valid.
    pub async fn dial_address(&mut self, address: Multiaddr) -> crate::Result<()> {
        let mut protocol_stack = address.iter();

        match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
        {
            Protocol::Ip4(_) | Protocol::Ip6(_) => {}
            Protocol::Dns(addr) | Protocol::Dns4(addr) | Protocol::Dns6(addr) => {
                let dns_address = addr.to_string();
                let original = address.clone();

                self.pending_dns_resolves.push(Box::pin(async move {
                    match AsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default()) {
                        Ok(resolver) => (original, resolver.lookup_ip(dns_address).await),
                        Err(error) => (original, Err(error)),
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

        match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
        {
            Protocol::Tcp(_) => match protocol_stack.next() {
                Some(Protocol::Ws(_)) => {
                    let connection_id = self.dial_ws(address.clone()).await?;
                    self.pending_connections.insert(connection_id, address);
                    Ok(())
                }
                _ => {
                    let connection_id = self.dial_tcp(address.clone()).await?;
                    self.pending_connections.insert(connection_id, address);
                    Ok(())
                }
            },
            Protocol::Udp(_) => match protocol_stack
                .next()
                .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
            {
                Protocol::QuicV1 => {
                    let connection_id = self.dial_quic(address.clone()).await?;
                    self.pending_connections.insert(connection_id, address);

                    Ok(())
                }
                _ => Err(Error::TransportNotSupported(address.clone())),
            },
            protocol => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `tcp`"
                );

                Err(Error::TransportNotSupported(address))
            }
        }
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
                    match self.on_resolved_dns_address(event.0.clone(), event.1).await {
                        Ok(address) => {
                            tracing::debug!(target: LOG_TARGET, ?address, "connect to remote peer");

                            // TODO: no unwraps
                            let connection_id = self.dial_tcp(address.clone()).await.unwrap();
                            self.pending_connections.insert(connection_id, address);
                        }
                        Err(error) => return Some(TransportManagerEvent::DialFailure { address: event.0, error }),
                    }
                }
            }
        }
    }
}
