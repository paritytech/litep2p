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

use crate::types::ProtocolType;

use multiaddr::Multiaddr;

/// Connection role.
#[derive(Debug)]
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

pub struct LiteP2pConfiguration {
    /// Listening addresses.
    listen_addresses: Vec<Multiaddr>,
}

// Transport configuration.
#[derive(Debug)]
pub struct TransportConfig {
    /// Listening address for the transport.
    listen_address: Multiaddr,

    /// Supported protocols.
    supported_protocols: Vec<ProtocolType>,

    /// Maximum number of allowed connections:
    max_connections: usize,
}

impl TransportConfig {
    /// Create new [`TransportConfig`].
    pub fn new(
        listen_address: Multiaddr,
        supported_protocols: Vec<ProtocolType>,
        max_connections: usize,
    ) -> Self {
        Self {
            listen_address,
            supported_protocols,
            max_connections,
        }
    }

    /// Get listen address.
    pub fn listen_address(&self) -> &Multiaddr {
        &self.listen_address
    }

    /// Get listen address.
    pub fn supported_protocols(&self) -> impl Iterator<Item = &ProtocolType> {
        self.supported_protocols.iter()
    }

    /// Get listen address.
    pub fn max_connections(&self) -> usize {
        self.max_connections
    }
}
