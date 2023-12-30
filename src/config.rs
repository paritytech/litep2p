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

//! [`Litep2p`](`crate::Litep2p`) configuration.

use crate::{
    crypto::ed25519::Keypair,
    executor::{DefaultExecutor, Executor},
    protocol::{
        libp2p::{bitswap, identify, kademlia, ping},
        mdns::Config as MdnsConfig,
        notification, request_response, UserProtocol,
    },
    transport::{
        quic::config::TransportConfig as QuicTransportConfig,
        tcp::config::TransportConfig as TcpTransportConfig,
        webrtc::config::TransportConfig as WebRtcTransportConfig,
        websocket::config::TransportConfig as WebSocketTransportConfig, MAX_PARALLEL_DIALS,
    },
    types::protocol::ProtocolName,
    PeerId,
};

use multiaddr::Multiaddr;

use std::{collections::HashMap, sync::Arc};

/// Connection role.
#[derive(Debug, Copy, Clone)]
pub enum Role {
    /// Dialer.
    Dialer,

    /// Listener.
    Listener,
}

impl From<Role> for yamux::Mode {
    fn from(value: Role) -> Self {
        match value {
            Role::Dialer => yamux::Mode::Client,
            Role::Listener => yamux::Mode::Server,
        }
    }
}

/// Configuration builder for [`Litep2p`](`crate::Litep2p`).
pub struct ConfigBuilder {
    // TCP transport configuration.
    tcp: Option<TcpTransportConfig>,

    /// QUIC transport config.
    quic: Option<QuicTransportConfig>,

    /// WebRTC transport config.
    webrtc: Option<WebRtcTransportConfig>,

    /// WebSocket transport config.
    websocket: Option<WebSocketTransportConfig>,

    /// Keypair.
    keypair: Option<Keypair>,

    /// Ping protocol config.
    ping: Option<ping::Config>,

    /// Identify protocol config.
    identify: Option<identify::Config>,

    /// Kademlia protocol config.
    kademlia: Option<kademlia::Config>,

    /// Bitswap protocol config.
    bitswap: Option<bitswap::Config>,

    /// Notification protocols.
    notification_protocols: HashMap<ProtocolName, notification::Config>,

    /// Request-response protocols.
    request_response_protocols: HashMap<ProtocolName, request_response::Config>,

    /// User protocols.
    user_protocols: HashMap<ProtocolName, Box<dyn UserProtocol>>,

    /// mDNS configuration.
    mdns: Option<MdnsConfig>,

    /// Known addresess.
    known_addresses: Vec<(PeerId, Vec<Multiaddr>)>,

    /// Executor for running futures.
    executor: Option<Arc<dyn Executor>>,

    /// Maximum number of parallel dial attempts.
    max_parallel_dials: usize,
}

impl ConfigBuilder {
    /// Create new empty [`ConfigBuilder`].
    pub fn new() -> Self {
        Self {
            tcp: None,
            quic: None,
            webrtc: None,
            websocket: None,
            keypair: None,
            ping: None,
            identify: None,
            kademlia: None,
            bitswap: None,
            mdns: None,
            executor: None,
            max_parallel_dials: MAX_PARALLEL_DIALS,
            user_protocols: HashMap::new(),
            notification_protocols: HashMap::new(),
            request_response_protocols: HashMap::new(),
            known_addresses: Vec::new(),
        }
    }

    /// Add TCP transport configuration.
    pub fn with_tcp(mut self, config: TcpTransportConfig) -> Self {
        self.tcp = Some(config);
        self
    }

    /// Add QUIC transport configuration.
    pub fn with_quic(mut self, config: QuicTransportConfig) -> Self {
        self.quic = Some(config);
        self
    }

    /// Add WebRTC transport configuration.
    pub fn with_webrtc(mut self, config: WebRtcTransportConfig) -> Self {
        self.webrtc = Some(config);
        self
    }

    /// Add WebSocket transport configuration.
    pub fn with_websocket(mut self, config: WebSocketTransportConfig) -> Self {
        self.websocket = Some(config);
        self
    }

    /// Add keypair.
    pub fn with_keypair(mut self, keypair: Keypair) -> Self {
        self.keypair = Some(keypair);
        self
    }

    /// Install notification protocol.
    pub fn with_notification_protocol(mut self, config: notification::Config) -> Self {
        self.notification_protocols.insert(config.protocol_name().clone(), config);
        self
    }

    /// Enable IPFS Ping protocol.
    pub fn with_libp2p_ping(mut self, config: ping::Config) -> Self {
        self.ping = Some(config);
        self
    }

    /// Enable IPFS Identify protocol.
    pub fn with_libp2p_identify(mut self, config: identify::Config) -> Self {
        self.identify = Some(config);
        self
    }

    /// Enable IPFS Kademlia protocol.
    pub fn with_libp2p_kademlia(mut self, config: kademlia::Config) -> Self {
        self.kademlia = Some(config);
        self
    }

    /// Enable IPFS Bitswap protocol.
    pub fn with_libp2p_bitswap(mut self, config: bitswap::Config) -> Self {
        self.bitswap = Some(config);
        self
    }

    /// Install request-response protocol.
    pub fn with_request_response_protocol(mut self, config: request_response::Config) -> Self {
        self.request_response_protocols.insert(config.protocol_name().clone(), config);
        self
    }

    /// Install user protocol.
    pub fn with_user_protocol(mut self, protocol: Box<dyn UserProtocol>) -> Self {
        self.user_protocols.insert(protocol.protocol(), protocol);
        self
    }

    /// Enable mDNS for peer discoveries in the local network.
    pub fn with_mdns(mut self, config: MdnsConfig) -> Self {
        self.mdns = Some(config);
        self
    }

    /// Add known address(es) for one or more peers.
    pub fn with_known_addresses(
        mut self,
        addresses: impl Iterator<Item = (PeerId, Vec<Multiaddr>)>,
    ) -> Self {
        self.known_addresses = addresses.collect();
        self
    }

    /// Add executor for running futures spawned by `litep2p`.
    ///
    /// If no executor is specified, `litep2p` defaults to calling `tokio::spawn()`.
    pub fn with_executor(mut self, executor: Arc<dyn Executor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// How many addresses should litep2p attempt to dial in parallel when connection to a remote
    /// peer.
    pub fn with_max_parallel_dials(mut self, max_parallel_dials: usize) -> Self {
        self.max_parallel_dials = max_parallel_dials;
        self
    }

    /// Build [`Litep2pConfig`].
    ///
    /// Generates a default keypair if user didn't provide one.
    pub fn build(mut self) -> Litep2pConfig {
        let keypair = match self.keypair {
            Some(keypair) => keypair,
            None => Keypair::generate(),
        };

        Litep2pConfig {
            keypair,
            tcp: self.tcp.take(),
            mdns: self.mdns.take(),
            quic: self.quic.take(),
            webrtc: self.webrtc.take(),
            websocket: self.websocket.take(),
            ping: self.ping.take(),
            identify: self.identify.take(),
            kademlia: self.kademlia.take(),
            bitswap: self.bitswap.take(),
            max_parallel_dials: self.max_parallel_dials,
            executor: self.executor.map_or(Arc::new(DefaultExecutor {}), |executor| executor),
            user_protocols: self.user_protocols,
            notification_protocols: self.notification_protocols,
            request_response_protocols: self.request_response_protocols,
            known_addresses: self.known_addresses,
        }
    }
}

/// Configuration for [`Litep2p`](`crate::Litep2p`).
pub struct Litep2pConfig {
    // TCP transport configuration.
    pub(crate) tcp: Option<TcpTransportConfig>,

    /// QUIC transport config.
    pub(crate) quic: Option<QuicTransportConfig>,

    /// WebRTC transport config.
    pub(crate) webrtc: Option<WebRtcTransportConfig>,

    /// WebSocket transport config.
    pub(crate) websocket: Option<WebSocketTransportConfig>,

    /// Keypair.
    pub(crate) keypair: Keypair,

    /// Ping protocol configuration, if enabled.
    pub(crate) ping: Option<ping::Config>,

    /// Identify protocol configuration, if enabled.
    pub(crate) identify: Option<identify::Config>,

    /// Kademlia protocol configuration, if enabled.
    pub(crate) kademlia: Option<kademlia::Config>,

    /// Bitswap protocol configuration, if enabled.
    pub(crate) bitswap: Option<bitswap::Config>,

    /// Notification protocols.
    pub(crate) notification_protocols: HashMap<ProtocolName, notification::Config>,

    /// Request-response protocols.
    pub(crate) request_response_protocols: HashMap<ProtocolName, request_response::Config>,

    /// User protocols.
    pub(crate) user_protocols: HashMap<ProtocolName, Box<dyn UserProtocol>>,

    /// mDNS configuration.
    pub(crate) mdns: Option<MdnsConfig>,

    /// Executor.
    pub(crate) executor: Arc<dyn Executor>,

    /// Maximum number of parallel dial attempts.
    pub(crate) max_parallel_dials: usize,

    /// Known addresses.
    pub known_addresses: Vec<(PeerId, Vec<Multiaddr>)>,
}
