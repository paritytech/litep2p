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

//! TCP transport implementation.

use crate::{
    config::TransportConfig,
    crypto::{
        ed25519,
        noise::{self, NoiseConfiguration},
        PublicKey,
    },
    error::{AddressError, Error},
    peer_id::PeerId,
    transport::{ConnectionContext, Transport, TransportService},
    types::{ProtocolId, ProtocolType, RequestId, SubstreamId},
};

use futures::io::{AsyncRead, AsyncWrite};
use multiaddr::{Multiaddr, Protocol};
use multistream_select::{dialer_select_proto, Version};
use tokio::net::TcpStream;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::Level;

use std::net::{IpAddr, SocketAddr};

/// Logging target for the file.
const LOG_TARGET: &str = "transport::tcp";

pub struct TcpTransportService;

impl TcpTransportService {
    fn new() -> Self {
        Self {}
    }

    /// Extract socket address and `PeerId`, if found, from `address`.
    fn get_socket_address(address: Multiaddr) -> crate::Result<(SocketAddr, Option<PeerId>)> {
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

    /// Negotiate protocol.
    async fn negotiate_protocol(
        io: impl AsyncRead + AsyncWrite + Unpin,
        protocols: Vec<&str>,
    ) -> crate::Result<impl AsyncRead + AsyncWrite + Unpin> {
        tracing::span!(target: LOG_TARGET, Level::TRACE, "negotiate protocol").enter();
        tracing::event!(
            target: LOG_TARGET,
            Level::TRACE,
            ?protocols,
            "negotiating protocols",
        );

        let (protocol, mut io) = dialer_select_proto(io, protocols, Version::V1).await?;

        tracing::event!(
            target: LOG_TARGET,
            Level::TRACE,
            ?protocol,
            "protocol negotiated",
        );

        Ok(io)
    }
}

#[async_trait::async_trait]
impl TransportService for TcpTransportService {
    async fn open_connection(
        &mut self,
        address: Multiaddr,
        noise_config: NoiseConfiguration,
    ) -> crate::Result<ConnectionContext> {
        tracing::span!(target: LOG_TARGET, Level::DEBUG, "open connection").enter();
        tracing::event!(
            target: LOG_TARGET,
            Level::DEBUG,
            ?address,
            "try to establish outbound connection",
        );

        let (socket_address, peer) = Self::get_socket_address(address)?;
        tracing::event!(
            target: LOG_TARGET,
            Level::TRACE,
            ?socket_address,
            ?peer,
            "multiaddress parsed"
        );

        // create `futures::{AsyncRead, AsyncWrite}` compatible I/O
        let io = TcpStream::connect(socket_address).await?;
        let io = TokioAsyncReadCompatExt::compat(io).into_inner();
        let io = TokioAsyncWriteCompatExt::compat_write(io);

        // negotiate `noise`
        let io = Self::negotiate_protocol(io, vec!["/noise"]).await?;
        tracing::event!(
            target: LOG_TARGET,
            Level::TRACE,
            "`multistream-select` and `noise` negotiated"
        );

        // perform noise handshake
        let (io, peer) = noise::handshake(io, noise_config).await?;
        tracing::event!(target: LOG_TARGET, Level::TRACE, "noise handshake done");

        // negotiate `yamux`
        let io = Self::negotiate_protocol(io, vec!["/yamux/1.0.0"]).await?;
        tracing::event!(target: LOG_TARGET, Level::TRACE, "`yamux` negotiated");

        Ok(ConnectionContext {
            io: Box::new(io),
            peer,
        })
    }

    fn close_connection(&mut self, peer: PeerId) -> crate::Result<()> {
        todo!();
    }

    // TODO: return handle + sink for sending/receiving notifications.
    fn open_substream(
        &mut self,
        peer: PeerId,
        protocol: ProtocolId,
        handshake: Option<Vec<u8>>,
    ) -> crate::Result<SubstreamId> {
        todo!();
    }

    fn close_substream(&mut self, substream: SubstreamId) -> crate::Result<()> {
        todo!();
    }

    fn send_request(
        &mut self,
        peer: PeerId,
        protocol: ProtocolType,
        request: Vec<u8>,
    ) -> crate::Result<RequestId> {
        todo!();
    }

    fn send_response(&mut self, request: RequestId, response: Vec<u8>) -> crate::Result<RequestId> {
        todo!();
    }
}

pub struct TcpTransport;

#[async_trait::async_trait]
impl Transport for TcpTransport {
    type Handle = TcpTransportService;

    /// Start the underlying transport listener and return a handle which allows `litep2p` to
    // interact with the transport.
    fn start(config: TransportConfig) -> Self::Handle {
        todo!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn establish_outbound_connection() {
        // TODO: create listener as well
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init()
            .expect("to succeed");

        let mut transport = TcpTransportService::new();
        let keypair = ed25519::Keypair::generate();
        let config = NoiseConfiguration::new(&keypair, crate::config::Role::Dialer);

        transport
            .open_connection(
                "/ip6/::1/tcp/8888".parse().expect("valid multiaddress"),
                config,
            )
            .await
            .unwrap();
    }

    #[test]
    fn parse_multiaddresses() {
        assert!(TcpTransportService::get_socket_address(
            "/ip6/::1/tcp/8888".parse().expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpTransportService::get_socket_address(
            "/ip4/127.0.0.1/tcp/8888"
                .parse()
                .expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpTransportService::get_socket_address(
            "/ip6/::1/tcp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpTransportService::get_socket_address(
            "/ip4/127.0.0.1/tcp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpTransportService::get_socket_address(
            "/ip6/::1/udp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_err());
        assert!(TcpTransportService::get_socket_address(
            "/ip4/127.0.0.1/udp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_err());
    }
}
