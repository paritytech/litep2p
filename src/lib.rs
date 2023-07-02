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

#![allow(unused)]

use crate::{
    codec::ProtocolCodec,
    config::Litep2pConfig,
    crypto::{ed25519::Keypair, PublicKey},
    error::Error,
    peer_id::PeerId,
    protocol::{
        libp2p::{identify::Identify, ping::Ping},
        notification::NotificationProtocol,
        request_response::RequestResponseProtocol,
        ConnectionEvent, ProtocolEvent,
    },
    transport::{
        quic::QuicTransport, tcp::TcpTransport, Transport, TransportCommand, TransportEvent,
    },
    types::protocol::ProtocolName,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use trust_dns_resolver::{
    config::{ResolverConfig, ResolverOpts},
    error::ResolveError,
    lookup_ip::LookupIp,
    AsyncResolver,
};

use std::{collections::HashMap, net::IpAddr, result};

// TODO: which of these need to be pub?
pub mod codec;
pub mod config;
pub mod crypto;
pub mod error;
pub mod peer_id;
pub mod protocol;
pub mod substream;
pub mod transport;
pub mod types;

mod mock;
mod multistream_select;

/// Public result type used by the crate.
pub type Result<T> = result::Result<T, error::Error>;

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p";

/// Default channel size.
const DEFAULT_CHANNEL_SIZE: usize = 64usize;

/// Litep2p events.
#[derive(Debug)]
pub enum Litep2pEvent {
    /// Connection established to peer.
    ConnectionEstablished {
        /// Remote peer ID.
        peer: PeerId,

        /// Remote address.
        address: Multiaddr,
    },

    /// Failed to dial peer.
    DialFailure {
        /// Address of the peer.
        address: Multiaddr,

        /// Dial error.
        error: Error,
    },
}

/// [`Litep2p`] object.
pub struct Litep2p {
    /// Local peer ID.
    local_peer_id: PeerId,

    /// Listen addresses.
    listen_addresses: Vec<Multiaddr>,

    /// RX channel for receiving events from transports.
    rx: Receiver<TransportEvent>,

    /// Supported transports and their command handles.
    transports: HashMap<SupportedTransport, Sender<TransportCommand>>,

    /// Pending connections.
    pending_connections: HashMap<usize, Multiaddr>,

    /// Pending DNS resolves.
    pending_dns_resolves:
        FuturesUnordered<BoxFuture<'static, (Multiaddr, result::Result<LookupIp, ResolveError>)>>,
}

#[derive(Debug)]
pub struct TransportService {
    rx: Receiver<ConnectionEvent>,
    _peers: HashMap<PeerId, Sender<ProtocolEvent>>,
}

impl TransportService {
    /// Create new [`ConnectionService`].
    pub fn new() -> (Self, Sender<ConnectionEvent>) {
        // TODO: maybe specify some other channel size
        let (tx, rx) = channel(DEFAULT_CHANNEL_SIZE);

        (
            Self {
                rx,
                _peers: HashMap::new(),
            },
            tx,
        )
    }

    /// Get next event from the transport.
    pub async fn next_event(&mut self) -> Option<ConnectionEvent> {
        self.rx.recv().await
    }
}

// TODO: move to protocol
/// Protocol information.
#[derive(Debug, Clone)]
pub struct ProtocolInfo {
    /// TX channel for sending connection events to the protocol.
    pub tx: Sender<ConnectionEvent>,

    /// Codec used by the protocol.
    pub codec: ProtocolCodec,
}

// TODO: move to transport maybe
/// Transport context.
#[derive(Debug, Clone)]
pub struct TransportContext {
    /// Enabled protocols.
    pub protocols: HashMap<ProtocolName, ProtocolInfo>,

    /// Keypair.
    pub keypair: Keypair,

    /// TX channel for sending events to [`Litep2p`].
    tx: Sender<TransportEvent>,
}

impl TransportContext {
    /// Create new [`TransportContext`].
    pub fn new(keypair: Keypair, tx: Sender<TransportEvent>) -> Self {
        Self {
            tx,
            keypair,
            protocols: HashMap::new(),
        }
    }

    /// Add new protocol.
    pub fn add_protocol(
        &mut self,
        protocol: ProtocolName,
        codec: ProtocolCodec,
    ) -> crate::Result<TransportService> {
        let (service, tx) = TransportService::new();

        match self
            .protocols
            .insert(protocol.clone(), ProtocolInfo { tx, codec })
        {
            Some(_) => Err(Error::ProtocolAlreadyExists(protocol)),
            None => Ok(service),
        }
    }

    /// Report to `Litep2p` that a peer connected.
    pub(crate) async fn report_connection_established(&mut self, peer: PeerId, address: Multiaddr) {
        let _ = self
            .tx
            .send(TransportEvent::ConnectionEstablished { peer, address })
            .await;
    }

    /// Report to `Litep2p` that a peer disconnected.
    pub(crate) async fn report_connection_closed(&mut self, peer: PeerId) {
        let _ = self
            .tx
            .send(TransportEvent::ConnectionClosed { peer })
            .await;
    }

    /// Report to `Litep2p` that dialing a remote peer failed.
    pub(crate) async fn report_dial_failure(&mut self, address: Multiaddr, error: Error) {
        let _ = self
            .tx
            .send(TransportEvent::DialFailure { address, error })
            .await;
    }
}

/// Supported protocols.
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
enum SupportedTransport {
    Tcp,
}

impl Litep2p {
    /// Create new [`Litep2p`].
    pub async fn new(mut config: Litep2pConfig) -> crate::Result<Litep2p> {
        let (tx, rx) = channel(DEFAULT_CHANNEL_SIZE);
        let local_peer_id = PeerId::from_public_key(&PublicKey::Ed25519(config.keypair.public()));
        let mut transport_ctx = TransportContext::new(config.keypair.clone(), tx);

        // TODO: zzz
        let mut listen_addresses = Vec::new();
        let mut protocols = Vec::new();
        let mut transports = HashMap::new();

        // start notification protocol event loops
        for (protocol, config) in config.notification_protocols.into_iter() {
            tracing::debug!(
                target: LOG_TARGET,
                ?protocol,
                "enable notification protocol",
            );
            protocols.push(protocol.clone());

            let service = transport_ctx.add_protocol(protocol, config.codec.clone())?;
            tokio::spawn(async move { NotificationProtocol::new(service, config).run().await });
        }

        // start request-response protocol event loops
        for (protocol, config) in config.request_response_protocols.into_iter() {
            tracing::debug!(
                target: LOG_TARGET,
                ?protocol,
                "enable request-response protocol",
            );
            protocols.push(protocol.clone());

            let service = transport_ctx.add_protocol(protocol, config.codec.clone())?;
            tokio::spawn(async move { RequestResponseProtocol::new(service, config).run().await });
        }

        // start ping protocol event loop if enabled
        if let Some(ping_config) = config.ping.take() {
            tracing::debug!(
                target: LOG_TARGET,
                protocol = ?ping_config.protocol,
                "enable ipfs ping protocol",
            );
            protocols.push(ping_config.protocol.clone());

            let service = transport_ctx
                .add_protocol(ping_config.protocol.clone(), ping_config.codec.clone())?;
            tokio::spawn(async move { Ping::new(service, ping_config).run().await });
        }

        if let Some(mut identify_config) = config.identify.take() {
            tracing::debug!(
                target: LOG_TARGET,
                protocol = ?identify_config.protocol,
                "enable ipfs identify protocol",
            );
            protocols.push(identify_config.protocol.clone());

            let service = transport_ctx.add_protocol(
                identify_config.protocol.clone(),
                identify_config.codec.clone(),
            )?;
            identify_config.public = Some(PublicKey::Ed25519(config.keypair.public()));
            identify_config.listen_addresses = Vec::new(); // TODO: zzz
            identify_config.protocols = protocols;

            tokio::spawn(async move { Identify::new(service, identify_config).run().await });
        }

        // enable tcp transport if the config exists
        if let Some(config) = config.tcp.take() {
            let (command_tx, command_rx) = channel(DEFAULT_CHANNEL_SIZE);
            transports.insert(SupportedTransport::Tcp, command_tx);

            let transport =
                <TcpTransport as Transport>::new(transport_ctx.clone(), config, command_rx).await?;
            listen_addresses.push(transport.listen_address());

            tokio::spawn(async move {
                if let Err(error) = transport.start().await {
                    tracing::error!(target: LOG_TARGET, "tcp failed");
                }
            });
        }

        if let Some(config) = config.quic.take() {
            let (command_tx, command_rx) = channel(DEFAULT_CHANNEL_SIZE);
            transports.insert(SupportedTransport::Tcp, command_tx);

            let transport =
                <QuicTransport as Transport>::new(transport_ctx.clone(), config, command_rx)
                    .await?;
            listen_addresses.push(transport.listen_address());

            tokio::spawn(async move {
                if let Err(error) = transport.start().await {
                    tracing::error!(target: LOG_TARGET, "quic failed");
                }
            });
        }

        // verify that at least one transport is specified
        if listen_addresses.is_empty() {
            return Err(Error::Other(String::from("No transport specified")));
        }

        Ok(Self {
            rx,
            local_peer_id,
            listen_addresses,
            transports,
            pending_connections: HashMap::new(),
            pending_dns_resolves: FuturesUnordered::new(),
        })
    }

    /// Get local peer ID.
    pub fn local_peer_id(&self) -> &PeerId {
        &self.local_peer_id
    }

    /// Get listen address for protocol.
    pub fn listen_addresses(&self) -> impl Iterator<Item = &Multiaddr> {
        self.listen_addresses.iter()
    }

    /// Attempt to connect to peer at `address`.
    ///
    /// If the transport specified by `address` is not supported, an error is returned.
    /// The connection is established in the background and its result is reported through
    /// [`Litep2p::next_event()`].
    pub async fn connect(&mut self, address: Multiaddr) -> crate::Result<()> {
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
            Protocol::Tcp(_) => {
                let connection_id = match self.transports.get_mut(&SupportedTransport::Tcp) {
                    Some(sender) => {
                        sender
                            .send(TransportCommand::Dial {
                                address: address.clone(),
                                connection_id: 0usize,
                            })
                            .await?;
                        0usize
                    }
                    None => return Err(Error::TransportNotSupported(address)),
                };

                self.pending_connections.insert(connection_id, address);
                Ok(())
            }
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
        result: result::Result<LookupIp, ResolveError>,
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

    /// Poll next event.
    pub async fn next_event(&mut self) -> crate::Result<Litep2pEvent> {
        loop {
            tokio::select! {
                event = self.rx.recv() => match event {
                    Some(TransportEvent::ConnectionEstablished { peer, address }) => {
                        return Ok(Litep2pEvent::ConnectionEstablished { peer, address })
                    }
                    Some(TransportEvent::DialFailure { error, address }) => {
                        return Ok(Litep2pEvent::DialFailure { address, error })
                    }
                    None => {
                        panic!("tcp transport failed");
                    }
                    event => {
                        tracing::info!(target: LOG_TARGET, ?event, "unhandle event from tcp");
                    }
                },
                event = self.pending_dns_resolves.select_next_some(), if !self.pending_dns_resolves.is_empty() => {
                    match self.on_resolved_dns_address(event.0.clone(), event.1).await {
                        Ok(address) => {
                            tracing::debug!(target: LOG_TARGET, ?address, "connect to remote peer");

                            let connection_id = match self.transports.get_mut(&SupportedTransport::Tcp) {
                                Some(sender) => {
                                    sender
                                        .send(TransportCommand::Dial {
                                            address: address.clone(),
                                            connection_id: 0usize,
                                        })
                                        .await?;
                                    tracing::info!(target: LOG_TARGET, "event sent!!!");
                                    0usize
                                }
                                None => return Err(Error::TransportNotSupported(address)),
                            };

                            // let connection_id = self.tcp.open_connection(address.clone())?;
                            self.pending_connections.insert(connection_id, address);
                        }
                        Err(error) => return Ok(Litep2pEvent::DialFailure { address: event.0, error }),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        config::{Litep2pConfig, Litep2pConfigBuilder},
        crypto::ed25519::Keypair,
        protocol::{
            libp2p::ping::{Config as PingConfig, PingEvent},
            notification::types::Config as NotificationConfig,
        },
        transport::tcp::config::TransportConfig as TcpTransportConfig,
        types::protocol::ProtocolName,
        Litep2p, Litep2pEvent,
    };
    use futures::Stream;
    use multiaddr::{Multiaddr, Protocol};

    #[tokio::test]
    async fn initialize_litep2p() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (config1, _service1) = NotificationConfig::new(
            ProtocolName::from("/notificaton/1"),
            1337usize,
            vec![1, 2, 3, 4],
            Vec::new(),
        );
        let (config2, _service2) = NotificationConfig::new(
            ProtocolName::from("/notificaton/2"),
            1337usize,
            vec![1, 2, 3, 4],
            Vec::new(),
        );
        let (ping_config, _ping_event_stream) = PingConfig::new(3);

        let config = Litep2pConfigBuilder::new()
            .with_tcp(TcpTransportConfig {
                listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            })
            .with_notification_protocol(config1)
            .with_notification_protocol(config2)
            .with_ipfs_ping(ping_config)
            .build();

        let _litep2p = Litep2p::new(config).await.unwrap();
    }

    // generate config for testing
    fn generate_config() -> (Litep2pConfig, Box<dyn Stream<Item = PingEvent> + Send>) {
        let keypair = Keypair::generate();
        let (ping_config, ping_event_stream) = PingConfig::new(3);

        (
            Litep2pConfigBuilder::new()
                .with_keypair(keypair)
                .with_tcp(TcpTransportConfig {
                    listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
                })
                .with_ipfs_ping(ping_config)
                .build(),
            ping_event_stream,
        )
    }

    #[tokio::test]
    async fn two_litep2ps_work() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (config1, _ping_event_stream1) = generate_config();
        let (config2, _ping_event_stream2) = generate_config();
        let mut litep2p1 = Litep2p::new(config1).await.unwrap();
        let mut litep2p2 = Litep2p::new(config2).await.unwrap();

        let address = litep2p2.listen_addresses().next().unwrap().clone();
        litep2p1.connect(address).await.unwrap();

        let (res1, res2) = tokio::join!(litep2p1.next_event(), litep2p2.next_event());

        assert!(std::matches!(
            res1,
            Ok(Litep2pEvent::ConnectionEstablished { .. })
        ));
        assert!(std::matches!(
            res2,
            Ok(Litep2pEvent::ConnectionEstablished { .. })
        ));
    }

    #[tokio::test]
    async fn dial_failure() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (config1, _ping_event_stream1) = generate_config();
        let (config2, _ping_event_stream2) = generate_config();
        let mut litep2p1 = Litep2p::new(config1).await.unwrap();
        let mut litep2p2 = Litep2p::new(config2).await.unwrap();

        litep2p1
            .connect("/ip6/::1/tcp/1".parse().unwrap())
            .await
            .unwrap();

        tokio::spawn(async move {
            loop {
                let _ = litep2p2.next_event().await;
            }
        });

        assert!(std::matches!(
            litep2p1.next_event().await,
            Ok(Litep2pEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn connect_over_dns() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair1 = Keypair::generate();
        let (ping_config1, _ping_event_stream1) = PingConfig::new(3);

        let config1 = Litep2pConfigBuilder::new()
            .with_keypair(keypair1)
            .with_tcp(TcpTransportConfig {
                listen_address: "/ip4/127.0.0.1/tcp/0".parse().unwrap(),
            })
            .with_ipfs_ping(ping_config1)
            .build();

        let keypair2 = Keypair::generate();
        let (ping_config2, _ping_event_stream2) = PingConfig::new(3);

        let config2 = Litep2pConfigBuilder::new()
            .with_keypair(keypair2)
            .with_tcp(TcpTransportConfig {
                listen_address: "/ip4/127.0.0.1/tcp/0".parse().unwrap(),
            })
            .with_ipfs_ping(ping_config2)
            .build();

        let mut litep2p1 = Litep2p::new(config1).await.unwrap();
        let mut litep2p2 = Litep2p::new(config2).await.unwrap();

        let address = litep2p2.listen_addresses().next().unwrap().clone();
        let tcp = address.iter().skip(1).next().unwrap();

        let mut new_address = Multiaddr::empty();
        new_address.push(Protocol::Dns("localhost".into()));
        new_address.push(tcp);

        litep2p1.connect(new_address).await.unwrap();
        let (res1, res2) = tokio::join!(litep2p1.next_event(), litep2p2.next_event());

        assert!(std::matches!(
            res1,
            Ok(Litep2pEvent::ConnectionEstablished { .. })
        ));
        assert!(std::matches!(
            res2,
            Ok(Litep2pEvent::ConnectionEstablished { .. })
        ));
    }
}
