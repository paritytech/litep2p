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
    config::Litep2pConfig,
    crypto::PublicKey,
    error::Error,
    peer_id::PeerId,
    protocol::{
        libp2p::{identify::Identify, kademlia::Kademlia, ping::Ping},
        notification::NotificationProtocol,
        request_response::RequestResponseProtocol,
    },
    transport::{
        manager::{SupportedTransport, TransportManager, TransportManagerEvent},
        quic::QuicTransport,
        tcp::TcpTransport,
        webrtc::WebRtcTransport,
        websocket::WebSocketTransport,
        Transport, TransportCommand, TransportEvent,
    },
    types::ConnectionId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use protocol::mdns::Mdns;
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

    /// Conneciton closed to remote peer.
    ConnectionClosed {
        /// Peer ID.
        peer: PeerId,
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

    /// Transport manager.
    transport_manager: TransportManager,
}

impl Litep2p {
    /// Create new [`Litep2p`].
    pub async fn new(mut config: Litep2pConfig) -> crate::Result<Litep2p> {
        let local_peer_id = PeerId::from_public_key(&PublicKey::Ed25519(config.keypair.public()));
        let mut listen_addresses = vec![];

        let (mut transport_manager, transport_handle) =
            TransportManager::new(config.keypair.clone());

        // start notification protocol event loops
        for (protocol, config) in config.notification_protocols.into_iter() {
            tracing::debug!(
                target: LOG_TARGET,
                ?protocol,
                "enable notification protocol",
            );

            let service = transport_manager.register_protocol(protocol, config.codec);
            tokio::spawn(async move { NotificationProtocol::new(service, config).run().await });
        }

        // start request-response protocol event loops
        for (protocol, config) in config.request_response_protocols.into_iter() {
            tracing::debug!(
                target: LOG_TARGET,
                ?protocol,
                "enable request-response protocol",
            );

            let service = transport_manager.register_protocol(protocol, config.codec);
            tokio::spawn(async move { RequestResponseProtocol::new(service, config).run().await });
        }

        for (protocol_name, protocol) in config.user_protocols.into_iter() {
            tracing::debug!(target: LOG_TARGET, protocol = ?protocol_name, "enable user protocol");
            // protocols.push(protocol_name.clone());

            let service = transport_manager.register_protocol(protocol_name, protocol.codec());
            // let service = transport_ctx.add_protocol(protocol_name, protocol.codec())?;
            tokio::spawn(async move { protocol.run(service).await });
        }

        // start ping protocol event loop if enabled
        if let Some(ping_config) = config.ping.take() {
            tracing::debug!(
                target: LOG_TARGET,
                protocol = ?ping_config.protocol,
                "enable ipfs ping protocol",
            );

            let service = transport_manager
                .register_protocol(ping_config.protocol.clone(), ping_config.codec);
            tokio::spawn(async move { Ping::new(service, ping_config).run().await });
        }

        // start kademlia protocol event loop if enabled
        if let Some(kademlia_config) = config.kademlia.take() {
            tracing::debug!(
                target: LOG_TARGET,
                protocol = ?kademlia_config.protocol,
                "enable ipfs kademlia protocol",
            );

            let service = transport_manager
                .register_protocol(kademlia_config.protocol.clone(), kademlia_config.codec);
            tokio::spawn(async move { Kademlia::new(service, kademlia_config).run().await });
        }

        // start identify protocol event loop if enabled
        if let Some(mut identify_config) = config.identify.take() {
            tracing::debug!(
                target: LOG_TARGET,
                protocol = ?identify_config.protocol,
                "enable ipfs identify protocol",
            );

            let service = transport_manager.register_protocol(
                identify_config.protocol.clone(),
                identify_config.codec.clone(),
            );
            identify_config.public = Some(PublicKey::Ed25519(config.keypair.public()));

            // TODO: fix these
            // identify_config.listen_addresses = Vec::new();
            // identify_config.protocols = protocols;

            tokio::spawn(async move { Identify::new(service, identify_config).run().await });
        }

        // enable tcp transport if the config exists
        if let Some(config) = config.tcp.take() {
            let service = transport_manager.register_transport(SupportedTransport::Tcp);

            let transport = <TcpTransport as Transport>::new(service, config).await?;
            listen_addresses.push(transport.listen_address());

            tokio::spawn(async move {
                if let Err(error) = transport.start().await {
                    tracing::error!(target: LOG_TARGET, ?error, "tcp failed");
                }
            });
        }

        // enable quic transport if the config exists
        if let Some(config) = config.quic.take() {
            let service = transport_manager.register_transport(SupportedTransport::Quic);
            let transport = <QuicTransport as Transport>::new(service, config).await?;
            listen_addresses.push(transport.listen_address());

            tokio::spawn(async move {
                if let Err(error) = transport.start().await {
                    tracing::error!(target: LOG_TARGET, ?error, "quic failed");
                }
            });
        }

        // enable webrtc transport if the config exists
        if let Some(config) = config.webrtc.take() {
            let service = transport_manager.register_transport(SupportedTransport::WebRtc);
            let transport = <WebRtcTransport as Transport>::new(service, config).await?;
            listen_addresses.push(transport.listen_address());

            tokio::spawn(async move {
                if let Err(error) = transport.start().await {
                    tracing::error!(target: LOG_TARGET, ?error, "webrtc failed");
                }
            });
        }

        // enable websocket transport if the config exists
        if let Some(config) = config.websocket.take() {
            let service = transport_manager.register_transport(SupportedTransport::WebRtc);
            let transport = <WebSocketTransport as Transport>::new(service, config).await?;
            listen_addresses.push(transport.listen_address());

            tokio::spawn(async move {
                if let Err(error) = transport.start().await {
                    tracing::error!(target: LOG_TARGET, ?error, "quic failed");
                }
            });
        }

        // enable mdns if the config exists
        if let Some(config) = config.mdns.take() {
            let mdns = Mdns::new(transport_handle, config, listen_addresses.clone())?;

            tokio::spawn(async move {
                if let Err(error) = mdns.start().await {
                    tracing::error!(target: LOG_TARGET, ?error, "mdns failed");
                }
            });
        }

        // verify that at least one transport is specified
        if listen_addresses.is_empty() {
            return Err(Error::Other(String::from("No transport specified")));
        }

        Ok(Self {
            local_peer_id,
            listen_addresses,
            transport_manager,
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
        self.transport_manager.dial_address(address).await
    }

    /// Poll next event.
    pub async fn next_event(&mut self) -> Option<Litep2pEvent> {
        match self.transport_manager.next().await? {
            TransportManagerEvent::ConnectionEstablished { peer, address } => {
                Some(Litep2pEvent::ConnectionEstablished { peer, address })
            }
            TransportManagerEvent::ConnectionClosed { peer } => {
                Some(Litep2pEvent::ConnectionClosed { peer })
            }
            TransportManagerEvent::DialFailure { address, error } => {
                Some(Litep2pEvent::DialFailure { address, error })
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
            .with_quic(crate::transport::quic::config::Config {
                listen_address: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
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
            Some(Litep2pEvent::ConnectionEstablished { .. })
        ));
        assert!(std::matches!(
            res2,
            Some(Litep2pEvent::ConnectionEstablished { .. })
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
            Some(Litep2pEvent::DialFailure { .. })
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
            Some(Litep2pEvent::ConnectionEstablished { .. })
        ));
        assert!(std::matches!(
            res2,
            Some(Litep2pEvent::ConnectionEstablished { .. })
        ));
    }

    #[tokio::test]
    #[ignore]
    async fn wss_test() {
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

        let mut litep2p1 = Litep2p::new(config1).await.unwrap();
        let address = "/dns/polkadot-connect-0.parity.io/tcp/443/wss/p2p/12D3KooWEPmjoRpDSUuiTjvyNDd8fejZ9eNWH5bE965nyBMDrB4o";

        litep2p1
            .connect(Multiaddr::try_from(address).unwrap())
            .await
            .unwrap();

        loop {
            let _ = litep2p1.next_event().await.unwrap();
        }

        // let (res1, res2) = tokio::join!(litep2p1.next_event(), litep2p2.next_event());
        // assert!(std::matches!(
        //     res1,
        //     Ok(Litep2pEvent::ConnectionEstablished { .. })
        // ));
        // assert!(std::matches!(
        //     res2,
        //     Ok(Litep2pEvent::ConnectionEstablished { .. })
        // ));
    }
}
