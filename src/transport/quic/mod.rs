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
    crypto::tls::{certificate::generate, TlsProvider},
    error::{AddressError, Error},
    peer_id::PeerId,
    transport::{
        quic::{config::Config, connection::QuicConnection},
        Transport, TransportCommand, TransportContext, TransportEvent,
    },
    types::ConnectionId,
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
    connection_id: ConnectionId,

    // /// Keypair used for signing certificates,
    // keypair: Keypair,
    /// Pending dials.
    pending_dials: HashMap<ConnectionId, Multiaddr>,

    /// Pending connections.
    pending_connections: FuturesUnordered<
        BoxFuture<'static, (ConnectionId, PeerId, Result<Connection, ConnectionError>)>,
    >,

    /// RX channel for receiving commands from `Litep2p`.
    command_rx: Receiver<TransportCommand>,

    /// RX channel for receiving the client `PeerId`.
    rx: Receiver<PeerId>,

    /// TX channel for send the client `PeerId` to server.
    _tx: Sender<PeerId>,
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
}

#[async_trait::async_trait]
impl Transport for QuicTransport {
    type Config = Config;

    /// Create new [`QuicTransport`] object.
    async fn new(
        context: TransportContext,
        config: Self::Config,
        command_rx: Receiver<TransportCommand>,
    ) -> crate::Result<Self>
    where
        Self: Sized,
    {
        tracing::info!(
            target: LOG_TARGET,
            listen_address = ?config.listen_address,
            "start quic transport",
        );

        let (listen_address, _) = Self::get_socket_address(&config.listen_address)?;
        let (certificate, key) = generate(&context.keypair)?;
        let (_tx, rx) = channel(1);

        let provider = TlsProvider::new(key, certificate, None, Some(_tx.clone()));
        let server = Server::builder()
            .with_tls(provider)
            .expect("TLS provider to be enabled successfully")
            .with_io(listen_address)?
            .start()?;
        let listen_address = server.local_addr()?;

        Ok(Self {
            rx,
            _tx,
            server,
            context,
            command_rx,
            listen_address,
            connection_id: ConnectionId::new(),
            pending_dials: HashMap::new(),
            pending_connections: FuturesUnordered::new(),
        })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr {
        socket_addr_to_multi_addr(&self.listen_address)
    }

    /// Start [`QuicTransport`] event loop.
    async fn start(mut self) -> crate::Result<()> {
        loop {
            tokio::select! {
                connection = self.server.accept() => match connection {
                    Some(connection) => {
                        // TODO: verify that this can't clash with `Litep2p`'s connection id
                        let connection_id = self.connection_id.next();
                        let context = self.context.clone();
                        let address = socket_addr_to_multi_addr(&connection.remote_addr().expect("remote address to be known"));

                        let peer = match self.rx.try_recv() {
                            Ok(peer) => peer,
                            Err(_) => {
                                tracing::error!(target: LOG_TARGET, "failed to receive client `PeerId` from tls verifier");
                                continue
                            }
                        };

                        // the only way `QuiConnection::new()` can fail if the protocols have exited.
                        // at which point the transport can be closed as well
                        let quic_connection = QuicConnection::new(
                            peer,
                            context,
                            connection,
                            connection_id
                        ).await?;

                        tracing::info!(target: LOG_TARGET, ?address, ?peer, "accepted connection from remote peer");

                        tokio::spawn(async move {
                            if let Err(error) = quic_connection.start().await {
                                tracing::debug!(target: LOG_TARGET, ?error, "quic connection exited with an error");
                            }
                        });

                        self.context.report_connection_established(peer, address).await;
                    }
                    None => {
                        tracing::error!(target: LOG_TARGET, "failed to accept connection, closing quic transport");
                        return Ok(())
                    }
                },
                connection = self.pending_connections.select_next_some(), if !self.pending_connections.is_empty() => {
                    let (connection_id, peer, result) = connection;

                    match result {
                        Ok(connection) => {
                            // TODO: remove from pending diadls
                            let context = self.context.clone();
                            tokio::spawn(async move {
                                // TODO: no unwraps
                                let quic_connection = QuicConnection::new(peer, context, connection, connection_id).await.unwrap();
                                if let Err(error) = quic_connection.start().await {
                                    tracing::debug!(target: LOG_TARGET, ?error, "quic connection exited with an error");
                                }
                            });

                            // the only way this can fail is if the `Litep2p` has shut down at which
                            // point transport can be closed as well.
                            self.context.tx.send(TransportEvent::ConnectionEstablished {
                                peer: PeerId::random(),
                                address: Multiaddr::empty()
                            }).await?;
                        }
                        Err(error) => match self.pending_dials.remove(&connection_id) {
                            Some(address) => self.context.report_dial_failure(
                                address,
                                Error::TransportError(error.to_string())
                            ).await,
                            None => tracing::debug!(
                                target: LOG_TARGET,
                                ?error,
                                "failed to establish connection"
                            ),
                        },
                    }
                }
                command = self.command_rx.recv() => match command.ok_or(Error::EssentialTaskClosed)? {
                    TransportCommand::Dial { address, connection_id } => {
                        tracing::debug!(target: LOG_TARGET, ?address, "open connection");

                        let _context = self.context.clone();
                        let (socket_address, Some(peer)) = Self::get_socket_address(&address)? else {
                            let _ = self.context
                                .report_dial_failure(
                                    address,
                                    Error::AddressError(AddressError::PeerIdMissing),
                                )
                                .await;
                            continue
                        };

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
                            (connection_id, peer, client.connect(connect).await)
                        }));
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
        protocol::ProtocolInfo,
        types::protocol::ProtocolName,
    };
    use tokio::sync::mpsc::channel;

    #[tokio::test]
    async fn connect_and_accept_works() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair1 = Keypair::generate();
        let (tx1, _rx1) = channel(64);
        let (event_tx1, mut event_rx1) = channel(64);
        let (_command_tx1, command_rx1) = channel(64);

        let context1 = TransportContext {
            local_peer_id: PeerId::random(),
            tx: event_tx1,
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

        let transport1 = QuicTransport::new(context1, transport_config1, command_rx1)
            .await
            .unwrap();

        let _peer1: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair1.public()));
        let listen_address = Transport::listen_address(&transport1).to_string();
        let listen_address: Multiaddr = format!("{}/p2p/{}", listen_address, _peer1.to_string())
            .parse()
            .unwrap();
        tokio::spawn(transport1.start());

        let keypair2 = Keypair::generate();
        let (tx2, _rx2) = channel(64);
        let (event_tx2, mut event_rx2) = channel(64);
        let (command_tx2, command_rx2) = channel(64);

        let context2 = TransportContext {
            local_peer_id: PeerId::random(),
            tx: event_tx2,
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

        let transport2 = QuicTransport::new(context2, transport_config2, command_rx2)
            .await
            .unwrap();
        tokio::spawn(transport2.start());

        command_tx2
            .send(TransportCommand::Dial {
                address: listen_address,
                connection_id: ConnectionId::new(),
            })
            .await
            .unwrap();

        let (res1, res2) = tokio::join!(event_rx1.recv(), event_rx2.recv());

        assert!(std::matches!(
            res1,
            Some(TransportEvent::ConnectionEstablished { .. })
        ));
        assert!(std::matches!(
            res2,
            Some(TransportEvent::ConnectionEstablished { .. })
        ));
    }
}
