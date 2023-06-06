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
    config::{Config, Role},
    crypto::{
        ed25519::Keypair,
        noise::{self, Encrypted, NoiseConfiguration},
        PublicKey,
    },
    error::{AddressError, Error, SubstreamError},
    peer_id::PeerId,
    transport::{
        tcp_new::{
            config::TransportConfig,
            connection::{Substream, TcpConnection},
        },
        Connection, ConnectionNew, Direction, Transport, TransportEvent, TransportNew,
        TransportService,
    },
    types::{protocol::ProtocolName, ProtocolId, ProtocolType, RequestId, SubstreamId},
    DEFAULT_CHANNEL_SIZE,
};

use futures::{
    future::BoxFuture,
    stream::{FuturesUnordered, StreamExt},
    AsyncRead, AsyncWrite,
};
use multiaddr::{Multiaddr, Protocol};
use multistream_select::{dialer_select_proto, listener_select_proto, Negotiated, Version};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{instrument, Level};

use std::{
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
};

mod config;
mod connection;

/// Logging target for the file.
const LOG_TARGET: &str = "tcp";

/// TCP transport.
#[derive(Debug)]
pub struct TcpTransport {
    /// Configuration.
    config: Config,

    /// TCP listener.
    listener: TcpListener,

    /// Assigned listen addresss.
    listen_address: SocketAddr,

    /// Pending connections.
    pending_connections: FuturesUnordered<BoxFuture<'static, crate::Result<TcpConnection>>>,
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
}

#[async_trait::async_trait]
impl TransportNew for TcpTransport {
    type Connection = TcpConnection;
    type Config = TransportConfig;

    /// Create new [`TcpTransport`].
    async fn new(config: Config, transport_config: TransportConfig) -> crate::Result<Self> {
        tracing::info!(
            target: LOG_TARGET,
            listen_address = ?transport_config.listen_address,
            "start tcp transport",
        );

        let (listen_address, _) = Self::get_socket_address(&transport_config.listen_address)?;
        let listener = TcpListener::bind(listen_address).await?;
        let listen_address = listener.local_addr()?;

        Ok(Self {
            config,
            listener,
            listen_address,
            pending_connections: FuturesUnordered::new(),
        })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr {
        let listen_address = self.listen_address();
        let mut multiaddr = Multiaddr::from(listen_address.ip());
        multiaddr.push(Protocol::Tcp(listen_address.port()));
        multiaddr
    }

    /// Open connection to remote peer at `address`.
    fn open_connection(&mut self, address: Multiaddr) -> crate::Result<()> {
        let config = self.config.clone();
        let (socket_address, peer) = Self::get_socket_address(&address)?;

        self.pending_connections.push(Box::pin(async move {
            TcpConnection::open_connection(config, socket_address, peer).await
        }));

        Ok(())
    }

    /// Poll next connection from [`TcpTransport`].
    async fn next_connection(&mut self) -> crate::Result<Self::Connection> {
        loop {
            tokio::select! {
                connection = self.listener.accept() => match connection {
                    Ok((connection, address)) => {
                        let config = self.config.clone();
                        self.pending_connections.push(Box::pin(async move {
                            TcpConnection::accept_connection(connection, config, address).await
                        }));
                    }
                    Err(err) => return Err(err.into()),
                },
                connection = self.pending_connections.select_next_some(), if !self.pending_connections.is_empty() => {
                    return connection
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{protocol::Libp2pProtocol, types::protocol::ProtocolName};

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

    // build two connected peers
    async fn build_two_connected_peers() -> (TcpConnection, TcpConnection) {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair1 = Keypair::generate();
        let config = Config::new(
            keypair1.clone(),
            vec![ProtocolName::from("/notification/1")],
        );

        let mut transport1 = TcpTransport::new(
            config,
            TransportConfig {
                listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            },
        )
        .await
        .unwrap();

        let keypair2 = Keypair::generate();
        let config = Config::new(
            keypair2.clone(),
            vec![
                ProtocolName::from("/notification/1"),
                ProtocolName::from("/notification/2"),
            ],
        );

        let mut transport2 = TcpTransport::new(
            config,
            TransportConfig {
                listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            },
        )
        .await
        .unwrap();

        let peer1: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair1.public().clone()));
        let peer2: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair2.public().clone()));

        tracing::info!(target: LOG_TARGET, "peer1 {peer1}, peer2 {peer2}");

        let listen_address = TransportNew::listen_address(&transport1);
        let _ = transport2.open_connection(listen_address).unwrap();

        let (res1, res2) = tokio::join!(transport1.next_connection(), transport2.next_connection());
        (res1.unwrap(), res2.unwrap())
    }

    #[tokio::test]
    async fn connect_and_accept_works() {
        let (mut stream1, mut stream2) = build_two_connected_peers().await;
        let substream = stream1.open_substream(ProtocolName::from("/notification/1"));

        let mut stream1_res = None;
        let mut stream2_res = None;

        loop {
            tokio::select! {
                event = stream1.next_substream() => match event {
                    Ok(stream) => {
                        stream1_res = Some(stream);
                    }
                    _ => panic!("error 1"),
                },
                event = stream2.next_substream() => match event {
                    Ok(stream) => {
                        stream2_res = Some(stream);
                    }
                    _ => panic!("error 1"),
                }
            }

            if stream2_res.is_some() && stream1_res.is_some() {
                break;
            }
        }
    }
}
