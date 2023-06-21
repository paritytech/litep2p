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
    config::Role,
    crypto::{
        ed25519::Keypair,
        noise::{self, Encrypted, NoiseConfiguration},
        PublicKey,
    },
    error::{AddressError, Error, SubstreamError},
    multistream_select::{dialer_select_proto, listener_select_proto, Negotiated, Version},
    peer_id::PeerId,
    transport::{
        tcp_new::{
            config::TransportConfig,
            connection::{Substream, TcpConnection},
        },
        Connection, Direction, NewTransportEvent, Transport, TransportError, TransportEvent,
        TransportNew, TransportService,
    },
    types::{protocol::ProtocolName, ProtocolId, ProtocolType, RequestId, SubstreamId},
    TransportContext, DEFAULT_CHANNEL_SIZE,
};

use futures::{
    future::BoxFuture,
    stream::{FuturesUnordered, StreamExt},
    AsyncRead, AsyncWrite,
};
use multiaddr::{Multiaddr, Protocol};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{instrument, Level};

use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
};

mod connection;

pub mod config;

/// Logging target for the file.
const LOG_TARGET: &str = "tcp";

/// Convert `SocketAddr` to `Multiaddr`
pub(crate) fn socket_addr_to_multi_addr(address: &SocketAddr) -> Multiaddr {
    let mut multiaddr = Multiaddr::from(address.ip());
    multiaddr.push(Protocol::Tcp(address.port()));
    multiaddr
}

#[derive(Debug)]
pub struct TcpError {
    /// Error.
    error: Error,

    /// Connection ID.
    connection_id: Option<usize>,
}

impl TcpError {
    pub fn new(error: Error, connection_id: Option<usize>) -> Self {
        Self {
            error,
            connection_id,
        }
    }
}

impl TransportError for TcpError {
    fn connection_id(&self) -> Option<usize> {
        self.connection_id
    }

    fn into_error(self) -> Error {
        self.error
    }
}

/// TCP transport.
#[derive(Debug)]
pub struct TcpTransport {
    /// Transport context.
    context: TransportContext,

    /// TCP listener.
    listener: TcpListener,

    /// Assigned listen addresss.
    listen_address: SocketAddr,

    /// Next connection ID.
    next_connection_id: usize,

    /// Pending dials.
    pending_dials: HashMap<usize, (Multiaddr)>,

    /// Pending connections.
    pending_connections: FuturesUnordered<BoxFuture<'static, Result<TcpConnection, TcpError>>>,
}

impl TcpTransport {
    /// Extract socket address and `PeerId`, if found, from `address`.
    fn get_socket_address(address: &Multiaddr) -> crate::Result<(SocketAddr, Option<PeerId>)> {
        tracing::trace!(target: LOG_TARGET, ?address, "parse multi address");

        let mut iter = address.iter();
        let socket_address = match iter.next() {
            Some(Protocol::Ip6(address)) => match iter.next() {
                Some(Protocol::Tcp(port)) => SocketAddr::new(IpAddr::V6(address), port),
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
                Some(Protocol::Tcp(port)) => SocketAddr::new(IpAddr::V4(address), port),
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

    /// Get assigned listen address.
    fn listen_address(&self) -> &SocketAddr {
        &self.listen_address
    }

    /// Get next substream ID.
    fn next_connection_id(&mut self) -> usize {
        let connection = self.next_connection_id;
        self.next_connection_id += 1;
        connection
    }
}

#[async_trait::async_trait]
impl TransportNew for TcpTransport {
    type Error = TcpError;
    type Config = TransportConfig;

    /// Create new [`TcpTransport`].
    async fn new(context: TransportContext, config: Self::Config) -> crate::Result<Self> {
        tracing::info!(
            target: LOG_TARGET,
            listen_address = ?config.listen_address,
            "start tcp transport",
        );

        let (listen_address, _) = Self::get_socket_address(&config.listen_address)?;
        let listener = TcpListener::bind(listen_address).await?;
        let listen_address = listener.local_addr()?;

        Ok(Self {
            context,
            listener,
            listen_address,
            next_connection_id: 0usize,
            pending_dials: HashMap::new(),
            pending_connections: FuturesUnordered::new(),
        })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr {
        socket_addr_to_multi_addr(self.listen_address())
    }

    /// Open connection to remote peer at `address`.
    fn open_connection(&mut self, address: Multiaddr) -> crate::Result<usize> {
        tracing::debug!(target: LOG_TARGET, ?address, "open connection");

        let context = self.context.clone();
        let (socket_address, peer) = Self::get_socket_address(&address)?;
        let connection_id = self.next_connection_id();

        self.pending_dials.insert(connection_id, address.clone());
        self.pending_connections.push(Box::pin(async move {
            TcpConnection::open_connection(context, connection_id, socket_address, peer)
                .await
                .map_err(|error| TcpError::new(error, Some(connection_id)))
        }));

        Ok(connection_id)
    }

    /// Poll next connection from [`TcpTransport`].
    async fn next_event(&mut self) -> Result<NewTransportEvent, TcpError> {
        loop {
            tokio::select! {
                connection = self.listener.accept() => match connection {
                    Ok((connection, address)) => {
                        let context = self.context.clone();
                        let connection_id = self.next_connection_id();

                        self.pending_connections.push(Box::pin(async move {
                            TcpConnection::accept_connection(context, connection, connection_id, address)
                                .await
                                .map_err(|error| TcpError::new(error, Some(connection_id)))
                        }));
                    }
                    Err(err) => return Err(TcpError::new(err.into(), None)),
                },
                connection = self.pending_connections.select_next_some(), if !self.pending_connections.is_empty() => {
                    match connection {
                        Ok(connection) => {
                            let peer = *connection.peer();
                            let address = connection.address().clone();

                            tokio::spawn(async move {
                                if let Err(error) = connection.start().await {
                                    tracing::error!(target: LOG_TARGET, ?peer, "connection failure");
                                }
                            });

                            return Ok(NewTransportEvent::ConnectionEstablished { peer, address })
                        }
                        Err(error) => {
                            match error.connection_id {
                                Some(connection_id) => match self.pending_dials.remove(&connection_id) {
                                    Some(address) => {
                                        return Ok(NewTransportEvent::DialFailure {
                                            address,
                                            error: error.error,
                                        })
                                    }
                                    None => tracing::debug!(
                                        target: LOG_TARGET,
                                        ?error,
                                        "failed to establish connection"
                                    ),
                                },
                                None => {
                                    tracing::debug!(target: LOG_TARGET, ?error, "failed to establish connection")
                                }
                            }
                        }
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
        new_config::Litep2pConfigBuilder,
        protocol::{libp2p::new_ping::Config as PingConfig, Libp2pProtocol},
        types::protocol::ProtocolName,
        ProtocolInfo,
    };
    use tokio::sync::mpsc::channel;

    #[test]
    fn parse_multiaddresses() {
        assert!(TcpTransport::get_socket_address(
            &"/ip6/::1/tcp/8888".parse().expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpTransport::get_socket_address(
            &"/ip4/127.0.0.1/tcp/8888"
                .parse()
                .expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpTransport::get_socket_address(
            &"/ip6/::1/tcp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpTransport::get_socket_address(
            &"/ip4/127.0.0.1/tcp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpTransport::get_socket_address(
            &"/ip6/::1/udp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_err());
        assert!(TcpTransport::get_socket_address(
            &"/ip4/127.0.0.1/udp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_err());
    }

    #[tokio::test]
    async fn connect_and_accept_works() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair1 = Keypair::generate();
        let (tx1, rx1) = channel(64);
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
        let transport_config1 = TransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
        };

        let mut transport1 = TcpTransport::new(context1, transport_config1)
            .await
            .unwrap();

        let keypair2 = Keypair::generate();
        let (tx2, rx2) = channel(64);
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
        let transport_config2 = TransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
        };

        let mut transport2 = TcpTransport::new(context2, transport_config2)
            .await
            .unwrap();

        let peer1: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair1.public().clone()));
        let peer2: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair2.public().clone()));

        let listen_address = TransportNew::listen_address(&transport1);
        let _ = transport2.open_connection(listen_address).unwrap();
        let (res1, res2) = tokio::join!(transport1.next_event(), transport2.next_event());

        assert!(std::matches!(
            res1,
            Ok(NewTransportEvent::ConnectionEstablished { .. })
        ));
        assert!(std::matches!(
            res2,
            Ok(NewTransportEvent::ConnectionEstablished { .. })
        ));
    }

    #[tokio::test]
    async fn dial_failure() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair1 = Keypair::generate();
        let (tx1, rx1) = channel(64);
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
        let transport_config1 = TransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
        };

        let mut transport1 = TcpTransport::new(context1, transport_config1)
            .await
            .unwrap();

        let keypair2 = Keypair::generate();
        let (tx2, rx2) = channel(64);
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
        let transport_config2 = TransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
        };

        let mut transport2 = TcpTransport::new(context2, transport_config2)
            .await
            .unwrap();

        let peer1: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair1.public().clone()));
        let peer2: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair2.public().clone()));

        tracing::info!(target: LOG_TARGET, "peer1 {peer1}, peer2 {peer2}");

        // let listen_address = TransportNew::listen_address(&transport1);
        let _ = transport2
            .open_connection("/ip6/::1/tcp/1".parse().unwrap())
            .unwrap();

        // spawn the other conection in the background as it won't return anything
        tokio::spawn(async move {
            loop {
                let _ = transport1.next_event().await;
            }
        });

        assert!(std::matches!(
            transport2.next_event().await,
            Ok(NewTransportEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn multistream_select_not_supported() {}

    #[tokio::test]
    async fn yamux_not_supported() {}

    #[tokio::test]
    async fn noise_not_supported() {}

    #[tokio::test]
    async fn multistream_select_timeout() {}

    #[tokio::test]
    async fn yamux_timeout() {}

    #[tokio::test]
    async fn noise_timeout() {}
}
