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
    crypto::ed25519::Keypair,
    protocol::{
        libp2p::{identify, kademlia, ping},
        mdns::Config as MdnsConfig,
        notification, request_response, UserProtocol,
    },
    transport::{
        quic::config::TransportConfig as QuicTransportConfig,
        tcp::config::TransportConfig as TcpTransportConfig, webrtc::WebRtcTransportConfig,
        websocket::config::TransportConfig as WebSocketTransportConfig,
    },
    types::protocol::ProtocolName,
};

use std::collections::HashMap;

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

#[derive(Debug)]
pub struct Litep2pConfigBuilder {
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
    ping: Option<ping::PingConfig>,

    /// Identify protocol config.
    identify: Option<identify::Config>,

    /// Kademlia protocol config.
    kademlia: Option<kademlia::Config>,

    /// Notification protocols.
    notification_protocols: HashMap<ProtocolName, notification::NotificationConfig>,

    /// Request-response protocols.
    request_response_protocols: HashMap<ProtocolName, request_response::RequestResponseConfig>,

    /// User protocols.
    user_protocols: HashMap<ProtocolName, Box<dyn UserProtocol>>,

    /// mDNS configuration.
    mdns: Option<MdnsConfig>,
}

impl Default for Litep2pConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl Litep2pConfigBuilder {
    /// Create new empty [`LiteP2pConfigBuilder`].
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
            mdns: None,
            user_protocols: HashMap::new(),
            notification_protocols: HashMap::new(),
            request_response_protocols: HashMap::new(),
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
    pub fn with_notification_protocol(mut self, config: notification::NotificationConfig) -> Self {
        self.notification_protocols
            .insert(config.protocol_name().clone(), config);
        self
    }

    /// Enable IPFS Ping protocol.
    pub fn with_libp2p_ping(mut self, config: ping::PingConfig) -> Self {
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

    /// Install request-response protocol.
    pub fn with_request_response_protocol(
        mut self,
        config: request_response::RequestResponseConfig,
    ) -> Self {
        self.request_response_protocols
            .insert(config.protocol_name().clone(), config);
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
            user_protocols: self.user_protocols,
            notification_protocols: self.notification_protocols,
            request_response_protocols: self.request_response_protocols,
        }
    }
}

#[derive(Debug)]
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
    pub(crate) ping: Option<ping::PingConfig>,

    /// Identify protocol configuration, if enabled.
    pub(crate) identify: Option<identify::Config>,

    /// Kdaemlia protocol configuration, if enabled.
    pub(crate) kademlia: Option<kademlia::Config>,

    /// Notification protocols.
    pub(crate) notification_protocols: HashMap<ProtocolName, notification::NotificationConfig>,

    /// Request-response protocols.
    pub(crate) request_response_protocols:
        HashMap<ProtocolName, request_response::RequestResponseConfig>,

    /// User protocols.
    pub(crate) user_protocols: HashMap<ProtocolName, Box<dyn UserProtocol>>,

    /// mDNS configuration.
    pub(crate) mdns: Option<MdnsConfig>,
}
