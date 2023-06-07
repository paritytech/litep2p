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
    crypto::{ed25519::Keypair, PublicKey},
    error::Error,
    new_config::Litep2pConfig,
    peer_id::PeerId,
    transport::{tcp_new::TcpTransport, TransportNew},
    LOG_TARGET,
};

use futures::{stream::FuturesUnordered, Stream, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};
use tokio_stream::{wrappers::ReceiverStream, StreamMap};

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
};

/// Litep2p events.
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
        /// Remote peer ID.
        peer: PeerId,
    },
}

/// Protocols supported by litep2p.
pub enum SupportedProtocol {
    Tcp,
}

/// [`Litep2p`] object.
pub struct Litep2p {
    /// Local peer ID.
    local_peer_id: PeerId,

    // Litep2p configuration.
    config: Litep2pConfig,

    /// TCP transport.
    tcp: TcpTransport,
}

impl Litep2p {
    /// Create new [`Litep2p`].
    pub async fn new(config: Litep2pConfig) -> crate::Result<Litep2p> {
        let local_peer_id = PeerId::from_public_key(&PublicKey::Ed25519(config.keypair().public()));

        // enable tcp transport if the config exists
        let tcp = match config.tcp() {
            Some(_) => <TcpTransport as TransportNew>::new(config.clone()).await?,
            None => panic!("tcp not enabled"),
        };

        Ok(Self {
            local_peer_id,
            config,
            tcp,
        })
    }

    /// Get local peer ID.
    pub fn local_peer_id(&self) -> &PeerId {
        &self.local_peer_id
    }

    /// Get listen address for protocol.
    pub fn listen_address(&self, protocol: SupportedProtocol) -> crate::Result<Multiaddr> {
        match protocol {
            SupportedProtocol::Tcp => Ok(self.tcp.listen_address()),
        }
    }

    /// Attempt to connect to peer at `address`.
    ///
    /// If the transport specified by `address` is not supported, an error is returned.
    /// The connection is established in the background and its result is reported through
    /// [`Litep2p::poll_next()`].
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
            Some("tcp") => self.tcp.open_connection(address),
            protocol => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `tcp`"
                );
                return Err(Error::TransportNotSupported(address));
            }
        }
    }

    /// Poll next event.
    pub async fn poll_next(&mut self) -> Option<()> {
        loop {
            tokio::select! {
                event = self.tcp.next_connection() => match event {
                    event => tracing::warn!(target: LOG_TARGET, "ignoring connection"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        crypto::ed25519::Keypair,
        new::{Litep2p, SupportedProtocol},
        new_config::{Litep2pConfig, Litep2pConfigBuilder},
        transport::tcp_new::config::TransportConfig,
        types::protocol::ProtocolName,
    };

    // generate config for testing
    fn generate_config(protocols: Vec<ProtocolName>) -> Litep2pConfig {
        let keypair = Keypair::generate();
        let mut config = Litep2pConfigBuilder::new()
            .with_keypair(keypair)
            .with_tcp(TransportConfig {
                listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            })
            .build();

        config.protocols = protocols;
        config
    }

    #[tokio::test]
    async fn two_litep2ps_work() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let config1 = generate_config(vec![ProtocolName::from("/notification/1")]);
        let config2 = generate_config(vec![
            ProtocolName::from("/notification/1"),
            ProtocolName::from("/notification/2"),
        ]);

        let mut litep2p1 = Litep2p::new(config1).await.unwrap();
        let mut litep2p2 = Litep2p::new(config2).await.unwrap();
        let address = litep2p2.listen_address(SupportedProtocol::Tcp).unwrap();

        litep2p1.connect(address).unwrap();

        loop {
            tokio::select! {
                event = litep2p1.poll_next() => {}
                event = litep2p2.poll_next() => {}
            }
        }
    }
}
