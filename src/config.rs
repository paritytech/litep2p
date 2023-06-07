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
        notification::NotificationProtocolConfig, request_response::RequestResponseProtocolConfig,
        Libp2pProtocol, NotificationProtocol,
    },
    transport::tcp_new::config,
    types::{self, protocol::ProtocolName},
};

use multiaddr::Multiaddr;

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

pub struct LiteP2pConfiguration {
    /// Listening addresses.
    pub(crate) listen_addresses: Vec<Multiaddr>,

    /// Notification protocols.
    pub(crate) notification_protocols: Vec<NotificationProtocolConfig>,

    /// Request-response protocols.
    pub(crate) request_response_protocols: Vec<RequestResponseProtocolConfig>,
}

impl LiteP2pConfiguration {
    pub fn new(
        listen_addresses: Vec<Multiaddr>,
        notification_protocols: Vec<NotificationProtocolConfig>,
        request_response_protocols: Vec<RequestResponseProtocolConfig>,
    ) -> Self {
        Self {
            listen_addresses,
            notification_protocols,
            request_response_protocols,
        }
    }
}

// Transport configuration.
#[derive(Debug)]
pub struct TransportConfig {
    /// Listening address for the transport.
    listen_address: Multiaddr,

    // /// Supported request-response protocols.
    // libp2p_protocols: Vec<Libp2pProtocol>,
    libp2p_protocols: Vec<String>,

    /// Supported notification protocols.
    notification_protocols: Vec<NotificationProtocol>,

    /// Maximum number of allowed connections:
    max_connections: usize,
}

impl TransportConfig {
    /// Create new [`TransportConfig`].
    pub fn new(
        listen_address: Multiaddr,
        libp2p_protocols: Vec<String>,
        notification_protocols: Vec<NotificationProtocol>,
        max_connections: usize,
    ) -> Self {
        Self {
            listen_address,
            libp2p_protocols,
            notification_protocols,
            max_connections,
        }
    }

    /// Get listen address.
    pub fn listen_address(&self) -> &Multiaddr {
        &self.listen_address
    }

    /// Get libp2p address.
    // pub fn libp2p_protocols(&self) -> impl Iterator<Item = &Libp2pProtocol> {
    pub fn libp2p_protocols(&self) -> impl Iterator<Item = &String> {
        self.libp2p_protocols.iter()
    }

    /// Get notification address.
    pub fn notification_protocols(&self) -> impl Iterator<Item = &NotificationProtocol> {
        self.notification_protocols.iter()
    }

    /// Get the number of maximum open connections.
    pub fn max_connections(&self) -> usize {
        self.max_connections
    }
}

/// Litep2p configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Keypair.
    keypair: Keypair,

    /// Supported protocols.
    protocols: Vec<ProtocolName>,
}

impl Config {
    pub fn new(keypair: Keypair, protocols: Vec<ProtocolName>) -> Self {
        Self { keypair, protocols }
    }

    /// Get keypair.
    pub fn keypair(&self) -> &Keypair {
        &self.keypair
    }

    /// Get protocols.
    pub fn protocols(&self) -> &Vec<ProtocolName> {
        &self.protocols
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::ed25519::Keypair;

    #[test]
    fn collect_protocols() {
        let keypair = Keypair::generate();
        let config = Config::new(
            keypair,
            vec![
                ProtocolName::from("/notification/1"),
                ProtocolName::from("/notification/2"),
            ],
        );

        let protocols: Vec<ProtocolName> = config.protocols().clone();
        let protocols = protocols
            .iter()
            .map(|protocol| &**protocol)
            .collect::<Vec<&str>>();

        println!("{protocols:?}");
    }
}
