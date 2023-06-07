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

pub struct Litep2pConfiguration {
    // TCP transport configuration.
    pub(crate) tcp: Option<config::TransportConfig>,

    /// Keypair.
    pub(crate) keypair: Option<Keypair>,
}

impl Litep2pConfiguration {
    /// Create new empty [`NewLiteP2pConfiguration`].
    pub fn new() -> Self {
        Self {
            tcp: None,
            keypair: None,
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
    pub fn with_ping(self) -> Self {
        self
    }

    /// Build [`NewLiteP2pConfiguration`].
    ///
    /// Generates a default keypair if user didn't provide one.
    pub fn build(mut self) -> Self {
        if self.keypair.is_none() {
            self.keypair = Some(Keypair::generate());
        }

        self
    }
}
