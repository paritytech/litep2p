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
    config::Litep2pConfig,
    crypto::PublicKey,
    error::Error,
    protocol::{
        libp2p::{bitswap::Bitswap, identify::Identify, kademlia::Kademlia, ping::Ping},
        mdns::Mdns,
        notification::NotificationProtocol,
        request_response::RequestResponseProtocol,
    },
    transport::{
        manager::{SupportedTransport, TransportManager, TransportManagerEvent},
        quic::QuicTransport,
        tcp::TcpTransport,
        webrtc::WebRtcTransport,
        websocket::WebSocketTransport,
        Transport,
    },
};

use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;

pub use peer_id::PeerId;

pub(crate) mod peer_id;
pub(crate) mod substream;

pub mod codec;
pub mod config;
pub mod crypto;
pub mod error;
pub mod protocol;
pub mod transport;
pub mod types;

mod mock;
mod multistream_select;

/// Public result type used by the crate.
pub type Result<T> = std::result::Result<T, error::Error>;

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

    /// Connection closed to remote peer.
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

            let service = transport_manager.register_protocol(
                protocol,
                config.fallback_names.clone(),
                config.codec,
            );
            tokio::spawn(async move { NotificationProtocol::new(service, config).run().await });
        }

        // start request-response protocol event loops
        for (protocol, config) in config.request_response_protocols.into_iter() {
            tracing::debug!(
                target: LOG_TARGET,
                ?protocol,
                "enable request-response protocol",
            );

            let service = transport_manager.register_protocol(
                protocol,
                config.fallback_names.clone(),
                config.codec,
            );
            tokio::spawn(async move { RequestResponseProtocol::new(service, config).run().await });
        }

        // start user protocol event loops
        for (protocol_name, protocol) in config.user_protocols.into_iter() {
            tracing::debug!(target: LOG_TARGET, protocol = ?protocol_name, "enable user protocol");

            let service =
                transport_manager.register_protocol(protocol_name, Vec::new(), protocol.codec());
            tokio::spawn(async move { protocol.run(service).await });
        }

        // start ping protocol event loop if enabled
        if let Some(ping_config) = config.ping.take() {
            tracing::debug!(
                target: LOG_TARGET,
                protocol = ?ping_config.protocol,
                "enable ipfs ping protocol",
            );

            let service = transport_manager.register_protocol(
                ping_config.protocol.clone(),
                Vec::new(),
                ping_config.codec,
            );
            tokio::spawn(async move { Ping::new(service, ping_config).run().await });
        }

        // start kademlia protocol event loop if enabled
        if let Some(kademlia_config) = config.kademlia.take() {
            tracing::debug!(
                target: LOG_TARGET,
                protocol = ?kademlia_config.protocol,
                "enable ipfs kademlia protocol",
            );

            let service = transport_manager.register_protocol(
                kademlia_config.protocol.clone(),
                Vec::new(),
                kademlia_config.codec,
            );
            tokio::spawn(async move { Kademlia::new(service, kademlia_config).run().await });
        }

        // start identify protocol event loop if enabled
        let mut identify_info = match config.identify.take() {
            None => None,
            Some(mut identify_config) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    protocol = ?identify_config.protocol,
                    "enable ipfs identify protocol",
                );

                let service = transport_manager.register_protocol(
                    identify_config.protocol.clone(),
                    Vec::new(),
                    identify_config.codec.clone(),
                );
                identify_config.public = Some(PublicKey::Ed25519(config.keypair.public()));

                Some((service, identify_config))
            }
        };

        // start bitswap protocol event loop if enabled
        if let Some(bitswap_config) = config.bitswap.take() {
            tracing::debug!(
                target: LOG_TARGET,
                protocol = ?bitswap_config.protocol,
                "enable ipfs bitswap protocol",
            );

            let service = transport_manager.register_protocol(
                bitswap_config.protocol.clone(),
                Vec::new(),
                bitswap_config.codec,
            );
            tokio::spawn(async move { Bitswap::new(service, bitswap_config).run().await });
        }

        // enable tcp transport if the config exists
        if let Some(config) = config.tcp.take() {
            let service = transport_manager.register_transport(SupportedTransport::Tcp);
            let transport = <TcpTransport as Transport>::new(service, config).await?;

            transport_manager.register_listen_address(transport.listen_address());
            listen_addresses.push(transport.listen_address().with(Protocol::P2p(
                Multihash::from_bytes(&local_peer_id.to_bytes()).unwrap(),
            )));

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

            transport_manager.register_listen_address(transport.listen_address());
            listen_addresses.push(transport.listen_address().with(Protocol::P2p(
                Multihash::from_bytes(&local_peer_id.to_bytes()).unwrap(),
            )));

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

            transport_manager.register_listen_address(transport.listen_address());
            listen_addresses.push(transport.listen_address().with(Protocol::P2p(
                Multihash::from_bytes(&local_peer_id.to_bytes()).unwrap(),
            )));

            tokio::spawn(async move {
                if let Err(error) = transport.start().await {
                    tracing::error!(target: LOG_TARGET, ?error, "webrtc failed");
                }
            });
        }

        // enable websocket transport if the config exists
        if let Some(config) = config.websocket.take() {
            let service = transport_manager.register_transport(SupportedTransport::WebSocket);
            let transport = <WebSocketTransport as Transport>::new(service, config).await?;

            transport_manager.register_listen_address(transport.listen_address());
            listen_addresses.push(transport.listen_address().with(Protocol::P2p(
                Multihash::from_bytes(&local_peer_id.to_bytes()).unwrap(),
            )));

            tokio::spawn(async move {
                if let Err(error) = transport.start().await {
                    tracing::error!(target: LOG_TARGET, ?error, "websocket failed");
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

        // if identify was enabled, give it the enabled protocols and listen addresses and start it
        if let Some((service, mut identify_config)) = identify_info.take() {
            identify_config.listen_addresses = listen_addresses.clone();
            identify_config.protocols = transport_manager.protocols().cloned().collect();

            tokio::spawn(async move { Identify::new(service, identify_config).run().await });
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
    // TODO: remove
    pub async fn connect(&mut self, address: Multiaddr) -> crate::Result<()> {
        self.transport_manager.dial_address(address).await
    }

    /// Dial peer.
    pub async fn dial(&mut self, peer: &PeerId) -> crate::Result<()> {
        self.transport_manager.dial(peer).await
    }

    /// Dial address.
    pub async fn dial_address(&mut self, address: Multiaddr) -> crate::Result<()> {
        self.transport_manager.dial_address(address).await
    }

    /// Poll next event.
    pub async fn next_event(&mut self) -> Option<Litep2pEvent> {
        match self.transport_manager.next().await? {
            TransportManagerEvent::ConnectionEstablished { peer, address, .. } =>
                Some(Litep2pEvent::ConnectionEstablished { peer, address }),
            TransportManagerEvent::ConnectionClosed { peer, .. } =>
                Some(Litep2pEvent::ConnectionClosed { peer }),
            TransportManagerEvent::DialFailure { address, error, .. } =>
                Some(Litep2pEvent::DialFailure { address, error }),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        config::Litep2pConfigBuilder,
        protocol::{libp2p::ping, notification::Config as NotificationConfig},
        transport::tcp::config::TransportConfig as TcpTransportConfig,
        types::protocol::ProtocolName,
        Litep2p, Litep2pEvent, PeerId,
    };
    use multiaddr::{Multiaddr, Protocol};
    use multihash::Multihash;
    use std::net::Ipv4Addr;

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
            false,
        );
        let (config2, _service2) = NotificationConfig::new(
            ProtocolName::from("/notificaton/2"),
            1337usize,
            vec![1, 2, 3, 4],
            Vec::new(),
            false,
        );
        let (ping_config, _ping_event_stream) = ping::Config::default();

        let config = Litep2pConfigBuilder::new()
            .with_tcp(TcpTransportConfig {
                listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
                yamux_config: Default::default(),
            })
            .with_quic(crate::transport::quic::config::TransportConfig {
                listen_address: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
            })
            .with_notification_protocol(config1)
            .with_notification_protocol(config2)
            .with_libp2p_ping(ping_config)
            .build();

        let _litep2p = Litep2p::new(config).await.unwrap();
    }

    #[tokio::test]
    async fn no_transport_given() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (config1, _service1) = NotificationConfig::new(
            ProtocolName::from("/notificaton/1"),
            1337usize,
            vec![1, 2, 3, 4],
            Vec::new(),
            false,
        );
        let (config2, _service2) = NotificationConfig::new(
            ProtocolName::from("/notificaton/2"),
            1337usize,
            vec![1, 2, 3, 4],
            Vec::new(),
            false,
        );
        let (ping_config, _ping_event_stream) = ping::Config::default();

        let config = Litep2pConfigBuilder::new()
            .with_notification_protocol(config1)
            .with_notification_protocol(config2)
            .with_libp2p_ping(ping_config)
            .build();

        assert!(Litep2p::new(config).await.is_err());
    }

    #[tokio::test]
    async fn dial_same_address_twice() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let (config1, _service1) = NotificationConfig::new(
            ProtocolName::from("/notificaton/1"),
            1337usize,
            vec![1, 2, 3, 4],
            Vec::new(),
            false,
        );
        let (config2, _service2) = NotificationConfig::new(
            ProtocolName::from("/notificaton/2"),
            1337usize,
            vec![1, 2, 3, 4],
            Vec::new(),
            false,
        );
        let (ping_config, _ping_event_stream) = ping::Config::default();

        let config = Litep2pConfigBuilder::new()
            .with_tcp(TcpTransportConfig {
                listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
                yamux_config: Default::default(),
            })
            .with_quic(crate::transport::quic::config::TransportConfig {
                listen_address: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
            })
            .with_notification_protocol(config1)
            .with_notification_protocol(config2)
            .with_libp2p_ping(ping_config)
            .build();

        let peer = PeerId::random();
        let address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(255, 254, 253, 252)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));

        let mut litep2p = Litep2p::new(config).await.unwrap();
        litep2p.dial_address(address.clone()).await.unwrap();
        litep2p.dial_address(address.clone()).await.unwrap();

        match litep2p.next_event().await {
            Some(Litep2pEvent::DialFailure { .. }) => {}
            _ => panic!("invalid event received"),
        }

        // verify that the second same dial was ignored and the dial failure is reported only once
        match tokio::time::timeout(std::time::Duration::from_secs(20), litep2p.next_event()).await {
            Err(_) => {}
            _ => panic!("invalid event received"),
        }
    }
}
