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

use crate::{crypto::ed25519::Keypair, transport::tcp_new::config, types::protocol::ProtocolName};

use multiaddr::Multiaddr;

#[derive(Debug, Clone)]
pub struct Litep2pConfigBuilder {
    // TCP transport configuration.
    pub(crate) tcp: Option<config::TransportConfig>,

    /// Keypair.
    pub(crate) keypair: Option<Keypair>,

    /// Installed protocols.
    pub(crate) protocols: Vec<ProtocolName>,

    /// Enable `/ipfs/ping/1.0.0`.
    pub(crate) enable_ping: bool,
}

impl Litep2pConfigBuilder {
    /// Create new empty [`LiteP2pConfigBuilder`].
    pub fn new() -> Self {
        Self {
            tcp: None,
            keypair: None,
            enable_ping: false,
            protocols: Vec::new(),
        }
    }

    /// Add TCP transport configuration.
    pub fn with_tcp(mut self, config: config::TransportConfig) -> Self {
        self.tcp = Some(config);
        self
    }

    /// Add keypair.
    pub fn with_keypair(mut self, keypair: Keypair) -> Self {
        self.keypair = Some(keypair);
        self
    }

    /// With `/ipfs/ping/1.0.0` protocol.
    pub fn with_ping(mut self) -> Self {
        self.enable_ping = true;
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
            enable_ping: self.enable_ping,
            protocols: self.protocols,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Litep2pConfig {
    // TCP transport configuration.
    tcp: Option<config::TransportConfig>,

    /// Keypair.
    keypair: Keypair,

    /// Enable `/ipfs/ping/1.0.0`.
    enable_ping: bool,

    /// Installed protocols.
    pub(crate) protocols: Vec<ProtocolName>,
}

impl Litep2pConfig {
    /// Get keypair.
    pub fn keypair(&self) -> &Keypair {
        &self.keypair
    }

    /// Get installed protocols.
    pub fn protocols(&self) -> &Vec<ProtocolName> {
        &self.protocols
    }

    /// Get TCP transport configuration.
    pub fn tcp(&self) -> &Option<config::TransportConfig> {
        &self.tcp
    }
}
