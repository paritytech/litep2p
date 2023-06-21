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
        libp2p::{identify, ping},
        notification, request_response,
    },
    transport::tcp::config::TransportConfig as TcpTransportConfig,
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

impl Into<yamux::Mode> for Role {
    fn into(self) -> yamux::Mode {
        match self {
            Role::Dialer => yamux::Mode::Client,
            Role::Listener => yamux::Mode::Server,
        }
    }
}

#[derive(Debug)]
pub struct Litep2pConfigBuilder {
    // TCP transport configuration.
    tcp: Option<TcpTransportConfig>,

    /// Keypair.
    keypair: Option<Keypair>,

    /// Ping protocol config.
    ping: Option<ping::Config>,

    /// Identify protocol config.
    identify: Option<identify::Config>,

    /// Notification protocols.
    notification_protocols: HashMap<ProtocolName, notification::types::Config>,

    /// Request-response protocols.
    request_response_protocols: HashMap<ProtocolName, request_response::types::Config>,
}

impl Litep2pConfigBuilder {
    /// Create new empty [`LiteP2pConfigBuilder`].
    pub fn new() -> Self {
        Self {
            tcp: None,
            keypair: None,
            ping: None,
            identify: None,
            notification_protocols: HashMap::new(),
            request_response_protocols: HashMap::new(),
        }
    }

    /// Add TCP transport configuration.
    pub fn with_tcp(mut self, config: TcpTransportConfig) -> Self {
        self.tcp = Some(config);
        self
    }

    /// Add keypair.
    pub fn with_keypair(mut self, keypair: Keypair) -> Self {
        self.keypair = Some(keypair);
        self
    }

    /// Install notification protocol.
    pub fn with_notification_protocol(mut self, config: notification::types::Config) -> Self {
        self.notification_protocols
            .insert(config.protocol_name().clone(), config);
        self
    }

    /// Enable IPFS Ping protocol.
    pub fn with_ipfs_ping(mut self, config: ping::Config) -> Self {
        self.ping = Some(config);
        self
    }

    /// Enable IPFS Identify protocol.
    pub fn with_ipfs_identify(mut self, config: identify::Config) -> Self {
        self.identify = Some(config);
        self
    }

    /// Install request-response protocol.
    pub fn with_request_response_protocol(
        mut self,
        config: request_response::types::Config,
    ) -> Self {
        self.request_response_protocols
            .insert(config.protocol_name().clone(), config);
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
            ping: self.ping.take(),
            identify: self.identify.take(),
            notification_protocols: self.notification_protocols,
            _request_response_protocols: self.request_response_protocols,
        }
    }
}

#[derive(Debug)]
pub struct Litep2pConfig {
    // TCP transport configuration.
    pub(crate) tcp: Option<TcpTransportConfig>,

    /// Keypair.
    pub(crate) keypair: Keypair,

    /// Ping protocol configuration, if enabled.
    pub(crate) ping: Option<ping::Config>,

    /// Identify protocol configuration, if enabled.
    pub(crate) identify: Option<identify::Config>,

    /// Notification protocols.
    pub(crate) notification_protocols: HashMap<ProtocolName, notification::types::Config>,

    /// Request-response protocols.
    pub(crate) _request_response_protocols: HashMap<ProtocolName, request_response::types::Config>,
}
