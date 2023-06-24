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
    transport::{tcp::TcpTransport, Transport, TransportEvent},
    types::protocol::ProtocolName,
};

use multiaddr::Multiaddr;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use std::collections::HashMap;

pub mod codec;
pub mod config;
pub mod crypto;
pub mod peer_id;
pub mod protocol;
pub mod substream;
pub mod transport;
pub mod types;

mod error;
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

    /// TCP transport.
    tcp: TcpTransport,

    /// Listen addresses.
    listen_addresses: Vec<Multiaddr>,

    /// Pending connections.
    pending_connections: HashMap<usize, Multiaddr>,
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

/// Transport context.
#[derive(Debug, Clone)]
pub struct TransportContext {
    /// Enabled protocols.
    pub protocols: HashMap<ProtocolName, ProtocolInfo>,

    /// Keypair.
    pub keypair: Keypair,
}

impl TransportContext {
    /// Create new [`TransportContext`].
    pub fn new(keypair: Keypair) -> Self {
        Self {
            protocols: HashMap::new(),
            keypair,
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
}

impl Litep2p {
    /// Create new [`Litep2p`].
    pub async fn new(mut config: Litep2pConfig) -> crate::Result<Litep2p> {
        let local_peer_id = PeerId::from_public_key(&PublicKey::Ed25519(config.keypair.public()));
        let mut transport_ctx = TransportContext::new(config.keypair.clone());
        let listen_addresses = Vec::new();
        let mut protocols = Vec::new();

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
            identify_config.listen_addresses = listen_addresses;
            identify_config.protocols = protocols;

            tokio::spawn(async move { Identify::new(service, identify_config).run().await });
        }

        // enable tcp transport if the config exists
        let tcp = match config.tcp.take() {
            Some(config) => <TcpTransport as Transport>::new(transport_ctx, config).await?,
            None => panic!("tcp not enabled"),
        };
        let listen_addresses = vec![tcp.listen_address()];

        Ok(Self {
            tcp,
            local_peer_id,
            listen_addresses,
            pending_connections: HashMap::new(),
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
    pub fn connect(&mut self, address: Multiaddr) -> crate::Result<()> {
        let mut protocol_stack = address.protocol_stack();

        match protocol_stack.next() {
            Some("ip4") | Some("ip6") => {}
            transport => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?transport,
                    "invalid transport, expected `ip4`/`ip6`"
                );
                return Err(Error::TransportNotSupported(address));
            }
        }

        match protocol_stack.next() {
            Some("tcp") => {
                let connection_id = self.tcp.open_connection(address.clone())?;
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

    /// Poll next event.
    pub async fn next_event(&mut self) -> crate::Result<Litep2pEvent> {
        loop {
            tokio::select! {
                event = self.tcp.next_event() => match event {
                    Ok(TransportEvent::ConnectionEstablished { peer, address }) => {
                        return Ok(Litep2pEvent::ConnectionEstablished { peer, address })
                    }
                    Ok(TransportEvent::DialFailure { error, address }) => {
                        return Ok(Litep2pEvent::DialFailure { address, error })
                    }
                    Err(error) => {
                        panic!("tcp transport failed: {error:?}");
                    }
                    event => {
                        tracing::info!(target: LOG_TARGET, ?event, "unhandle event from tcp");
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
        litep2p1.connect(address).unwrap();

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

        litep2p1.connect("/ip6/::1/tcp/1".parse().unwrap()).unwrap();

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
}
