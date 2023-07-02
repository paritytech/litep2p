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
    crypto::{
        ed25519::Keypair,
        tls::{certificate::generate, TlsProvider},
    },
    error::{AddressError, Error},
    peer_id::PeerId,
    transport::{
        quic::{config::Config, connection::QuicConnection},
        Transport, TransportError, TransportEvent,
    },
    TransportContext,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use s2n_quic::{
    client::Connect,
    connection::{Connection, Error as ConnectionError},
    Client, Server,
};
use tokio::sync::mpsc::{channel, Receiver, Sender};

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
};

mod connection;

pub mod config;

/// Logging target for the file.
const LOG_TARGET: &str = "quic";

/// Convert `SocketAddr` to `Multiaddr`
fn socket_addr_to_multi_addr(address: &SocketAddr) -> Multiaddr {
    let mut multiaddr = Multiaddr::from(address.ip());
    multiaddr.push(Protocol::Udp(address.port()));
    multiaddr.push(Protocol::QuicV1);

    multiaddr
}

#[derive(Debug, Clone)]
pub(crate) struct QuicError {}

/// Trait which allows `litep2p` to associate dial failures to opened connections.
impl TransportError for QuicError {
    /// Get connection ID.
    fn connection_id(&self) -> Option<usize> {
        None
    }

    /// Convert [`TransportError`] into `Error`
    fn into_error(self) -> Error {
        todo!();
    }
}

/// QUIC transport object.
#[derive(Debug)]
pub(crate) struct QuicTransport {
    /// QUIC server.
    server: Server,

    /// Transport context.
    context: TransportContext,

    /// Assigned listen addresss.
    listen_address: SocketAddr,

    /// Next connection ID.
    next_connection_id: usize,

    // /// Keypair used for signing certificates,
    // keypair: Keypair,
    /// Pending dials.
    pending_dials: HashMap<usize, Multiaddr>,

    /// Pending connections.
    pending_connections:
        FuturesUnordered<BoxFuture<'static, (usize, Result<Connection, ConnectionError>)>>,

    /// RX channel for receiving the client `PeerId`.
    rx: Receiver<PeerId>,

    /// TX channel for send the client `PeerId` to server.
    tx: Sender<PeerId>,
}

impl QuicTransport {
    /// Extract socket address and `PeerId`, if found, from `address`.
    fn get_socket_address(address: &Multiaddr) -> crate::Result<(SocketAddr, Option<PeerId>)> {
        tracing::trace!(target: LOG_TARGET, ?address, "parse multi address");

        let mut iter = address.iter();
        let socket_address = match iter.next() {
            Some(Protocol::Ip6(address)) => match iter.next() {
                Some(Protocol::Udp(port)) => SocketAddr::new(IpAddr::V6(address), port),
                protocol => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?protocol,
                        "invalid transport protocol, expected `Tcp`",
                    );
                    return Err(Error::AddressError(AddressError::InvalidProtocol));
                }
            },
            Some(Protocol::Ip4(address)) => match iter.next() {
                Some(Protocol::Udp(port)) => SocketAddr::new(IpAddr::V4(address), port),
                protocol => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?protocol,
                        "invalid transport protocol, expected `Tcp`",
                    );
                    return Err(Error::AddressError(AddressError::InvalidProtocol));
                }
            },
            protocol => {
                tracing::error!(target: LOG_TARGET, ?protocol, "invalid transport protocol");
                return Err(Error::AddressError(AddressError::InvalidProtocol));
            }
        };

        // verify that quic exists
        match iter.next() {
            Some(Protocol::QuicV1) => {}
            _ => return Err(Error::AddressError(AddressError::InvalidProtocol)),
        }

        let maybe_peer = match iter.next() {
            Some(Protocol::P2p(multihash)) => Some(PeerId::from_multihash(multihash)?),
            None => None,
            protocol => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `P2p` or `None`"
                );
                return Err(Error::AddressError(AddressError::InvalidProtocol));
            }
        };

        Ok((socket_address, maybe_peer))
    }

    /// Get next connection ID.
    fn next_connection_id(&mut self) -> usize {
        let connection = self.next_connection_id;
        self.next_connection_id += 1;
        connection
    }
}

#[async_trait::async_trait]
impl Transport for QuicTransport {
    type Config = Config;
    type Error = QuicError;

    /// Create new [`QuicTransport`] object.
    async fn new(context: TransportContext, config: Self::Config) -> crate::Result<Self>
    where
        Self: Sized,
    {
        tracing::info!(
            target: LOG_TARGET,
            listen_address = ?config.listen_address,
            "start quic transport",
        );

        let (listen_address, _) = Self::get_socket_address(&config.listen_address)?;
        // let keypair = Keypair::generate();
        let (certificate, key) = generate(&context.keypair)?;
        let (tx, rx) = channel(1);

        let provider = TlsProvider::new(key, certificate, None, Some(tx.clone()));
        let server = Server::builder()
            .with_tls(provider)
            .expect("TLS provider to be enabled successfully")
            .with_io(listen_address)?
            .start()?;
        let listen_address = server.local_addr()?;

        Ok(Self {
            server,
            // keypair,
            context,
            rx,
            tx,
            listen_address,
            next_connection_id: 0usize,
            pending_dials: HashMap::new(),
            pending_connections: FuturesUnordered::new(),
        })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr {
        socket_addr_to_multi_addr(&self.listen_address)
    }

    /// Try to open a connection to remote peer.
    ///
    /// The result is polled using [`Transport::next_connection()`].
    fn open_connection(&mut self, address: Multiaddr) -> crate::Result<usize> {
        tracing::debug!(target: LOG_TARGET, ?address, "open connection");

        let context = self.context.clone();
        let (socket_address, peer) = match Self::get_socket_address(&address)? {
            (address, Some(peer)) => (address, peer),
            _ => {
                return Err(Error::TransportError(String::from(
                    "Peer ID needed to open a connectino",
                )))
            }
        };

        let connection_id = self.next_connection_id();
        let (certificate, key) = generate(&self.context.keypair).unwrap();
        let provider = TlsProvider::new(key, certificate, Some(peer), None);

        let client = Client::builder()
            .with_tls(provider)
            .expect("TLS provider to be enabled successfully")
            .with_io("0.0.0.0:0")? // TODO: zzz
            .start()?;

        let connect = Connect::new(socket_address).with_server_name("localhost");

        self.pending_dials.insert(connection_id, address);
        self.pending_connections.push(Box::pin(async move {
            (connection_id, client.connect(connect).await)
        }));

        Ok(connection_id)
    }

    /// Poll next connection.
    async fn next_event(&mut self) -> Result<TransportEvent, Self::Error> {
        loop {
            tokio::select! {
                connection = self.server.accept() => match connection {
                    Some(connection) => {
                        let connection_id = self.next_connection_id();
                        let address = socket_addr_to_multi_addr(&connection.remote_addr().expect("remote address to be known"));
                        let quic_connection = QuicConnection::new(connection, connection_id);

                        tracing::info!(target: LOG_TARGET, ?address, "accepted connection from remote peer");

                        // TODO: so ugly
                        let peer = match self.rx.try_recv() {
                            Ok(peer) => peer,
                            Err(_) => {
                                tracing::error!(target: LOG_TARGET, "failed to receive client `PeerId` from tls verifier");
                                continue
                            }
                        };

                        tokio::spawn(async move {
                            if let Err(error) = quic_connection.start().await {
                                tracing::debug!(target: LOG_TARGET, ?error, "quic connection exited with an error");
                            }
                        });

                        return Ok(TransportEvent::ConnectionEstablished { peer, address });
                    }
                    None => {
                        // TODO: close transport
                        tracing::error!(target: LOG_TARGET, "failed to accept connection");
                    }
                },
                connection = self.pending_connections.select_next_some(), if !self.pending_connections.is_empty() => {
                    let (connection_id, result) = connection;

                    match result {
                        Ok(connection) => {
                            tokio::spawn(async move {
                                let quic_connection = QuicConnection::new(connection, connection_id);
                                if let Err(error) = quic_connection.start().await {
                                    tracing::debug!(target: LOG_TARGET, ?error, "quic connection exited with an error");
                                }
                            });

                            // TODO: fix
                            return Ok(TransportEvent::ConnectionEstablished {
                                peer: PeerId::random(),
                                address: Multiaddr::empty()
                            });
                        }
                        Err(error) => match self.pending_dials.remove(&connection_id) {
                            Some(address) => {
                                return Ok(TransportEvent::DialFailure {
                                    address,
                                    error: Error::TransportError(error.to_string()),
                                })
                            }
                            None => tracing::debug!(
                                target: LOG_TARGET,
                                ?error,
                                "failed to establish connection"
                            ),
                        },
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codec::ProtocolCodec,
        crypto::{ed25519::Keypair, PublicKey},
        types::protocol::ProtocolName,
        ProtocolInfo,
    };
    use tokio::sync::mpsc::channel;

    #[tokio::test]
    async fn connect_and_accept_works() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair1 = Keypair::generate();
        let (tx1, _rx1) = channel(64);
        let context1 = TransportContext {
            keypair: keypair1.clone(),
            protocols: HashMap::from_iter([(
                ProtocolName::from("/notif/1"),
                ProtocolInfo {
                    tx: tx1,
                    codec: ProtocolCodec::Identity(32),
                },
            )]),
        };
        let transport_config1 = config::Config {
            listen_address: "/ip4/127.0.0.1/udp/8888/quic-v1".parse().unwrap(),
        };

        let mut transport1 = QuicTransport::new(context1, transport_config1)
            .await
            .unwrap();

        let keypair2 = Keypair::generate();
        let (tx2, _rx2) = channel(64);
        let context2 = TransportContext {
            keypair: keypair2.clone(),
            protocols: HashMap::from_iter([(
                ProtocolName::from("/notif/1"),
                ProtocolInfo {
                    tx: tx2,
                    codec: ProtocolCodec::Identity(32),
                },
            )]),
        };
        let transport_config2 = config::Config {
            listen_address: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
        };

        let mut transport2 = QuicTransport::new(context2, transport_config2)
            .await
            .unwrap();

        let _peer1: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair1.public()));
        let _peer2: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair2.public()));

        let listen_address = Transport::listen_address(&transport1).to_string();
        let listen_address: Multiaddr = format!("{}/p2p/{}", listen_address, _peer1.to_string())
            .parse()
            .unwrap();

        let _ = transport2.open_connection(listen_address).unwrap();
        let (res1, res2) = tokio::join!(transport1.next_event(), transport2.next_event());

        assert!(std::matches!(
            res1,
            Ok(TransportEvent::ConnectionEstablished { .. })
        ));
        assert!(std::matches!(
            res2,
            Ok(TransportEvent::ConnectionEstablished { .. })
        ));
    }
}
