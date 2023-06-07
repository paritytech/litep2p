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
    new_config::Litep2pConfig,
    peer_id::PeerId,
    transport::{
        tcp_new::{
            config::TransportConfig, Connection, ConnectionNew, Direction, Transport, LOG_TARGET,
        },
        TransportEvent, TransportNew, TransportService,
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
    fmt, io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
};

/// TCP connection.
pub struct TcpConnection {
    /// Configuration.
    config: Litep2pConfig,

    /// Yamux connection.
    connection: yamux::ControlledConnection<Encrypted<Compat<TcpStream>>>,

    /// Yamux control.
    control: yamux::Control,

    /// Remote peer ID.
    peer: PeerId,

    /// Next substream ID.
    next_substream_id: usize,

    /// Pending substreams.
    pending_substreams: FuturesUnordered<BoxFuture<'static, crate::Result<Substream>>>,
}

impl fmt::Debug for TcpConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpConnection")
            .field("config", &self.config)
            .field("peer", &self.peer)
            .field("next_substream_id", &self.next_substream_id)
            .finish()
    }
}

impl TcpConnection {
    /// Open connection to remote peer at `address`.
    pub async fn open_connection(
        config: Litep2pConfig,
        address: SocketAddr,
        peer: Option<PeerId>,
    ) -> crate::Result<Self> {
        tracing::debug!(
            target: LOG_TARGET,
            ?address,
            ?peer,
            "open connection to remote peer",
        );

        let noise_config = NoiseConfiguration::new(config.keypair(), Role::Dialer);
        let stream = TcpStream::connect(address).await?;
        Self::negotiate_connection(stream, config, noise_config).await
    }

    /// Open substream for `protocol`.
    pub async fn open_substream(
        mut control: yamux::Control,
        substream: usize,
        protocol: ProtocolName,
    ) -> crate::Result<Substream> {
        tracing::debug!(target: LOG_TARGET, ?substream, ?protocol, "open substream");

        let stream = match control.open_stream().await {
            Ok(stream) => {
                tracing::trace!(target: LOG_TARGET, ?substream, "substream opened");
                stream
            }
            Err(error) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?substream,
                    ?error,
                    "failed to open substream"
                );
                return Err(Error::YamuxError(substream, error));
            }
        };

        let io = Self::negotiate_protocol(stream, &Role::Dialer, vec![&protocol])
            .await?
            .inner();

        Ok(Substream { io, substream })
    }

    /// Accept a new connection.
    pub async fn accept_connection(
        stream: TcpStream,
        config: Litep2pConfig,
        address: SocketAddr,
    ) -> crate::Result<Self> {
        tracing::debug!(target: LOG_TARGET, ?address, "accept connection");

        let noise_config = NoiseConfiguration::new(config.keypair(), Role::Listener);
        Self::negotiate_connection(stream, config, noise_config).await
    }

    /// Accept substream.
    pub async fn accept_substream(
        mut stream: yamux::Stream,
        substream: usize,
        protocols: Vec<ProtocolName>,
    ) -> crate::Result<Substream> {
        tracing::trace!(target: LOG_TARGET, ?substream, "accept inbound substream");

        let protocols = protocols
            .iter()
            .map(|protocol| &**protocol)
            .collect::<Vec<&str>>();
        let io = Self::negotiate_protocol(stream, &Role::Listener, protocols)
            .await?
            .inner();

        tracing::trace!(
            target: LOG_TARGET,
            ?substream,
            "substream accepted and negotiated"
        );

        Ok(Substream { io, substream })
    }

    /// Negotiate protocol.
    async fn negotiate_protocol<S: Connection>(
        stream: S,
        role: &Role,
        protocols: Vec<&str>,
    ) -> crate::Result<Negotiated<S>> {
        tracing::trace!(target: LOG_TARGET, ?protocols, "negotiating protocols");

        let (protocol, mut socket) = match role {
            Role::Dialer => dialer_select_proto(stream, protocols, Version::V1).await?,
            Role::Listener => listener_select_proto(stream, protocols).await?,
        };

        tracing::trace!(target: LOG_TARGET, ?protocol, "protocol negotiated");

        Ok(socket)
    }

    /// Negotiate noise + yamux for the connection.
    async fn negotiate_connection(
        stream: TcpStream,
        config: Litep2pConfig,
        noise_config: NoiseConfiguration,
    ) -> crate::Result<Self> {
        tracing::trace!(
            target: LOG_TARGET,
            role = ?noise_config.role,
            "negotiate connection",
        );

        let stream = TokioAsyncReadCompatExt::compat(stream).into_inner();
        let stream = TokioAsyncWriteCompatExt::compat_write(stream);
        let role = noise_config.role;

        // negotiate `noise`
        let stream = Self::negotiate_protocol(stream, &noise_config.role, vec!["/noise"])
            .await?
            .inner();

        tracing::trace!(
            target: LOG_TARGET,
            "`multistream-select` and `noise` negotiated"
        );

        // perform noise handshake
        let (stream, peer) = noise::handshake_new(stream, noise_config).await?;
        tracing::trace!(target: LOG_TARGET, "noise handshake done");
        let stream: Encrypted<Compat<TcpStream>> = stream;

        // negotiate `yamux`
        let stream = Self::negotiate_protocol(stream, &role, vec!["/yamux/1.0.0"])
            .await?
            .inner();
        tracing::trace!(target: LOG_TARGET, "`yamux` negotiated");

        let connection = yamux::Connection::new(stream, yamux::Config::default(), role.into());
        let (control, mut connection) = yamux::Control::new(connection);

        Ok(Self {
            config,
            connection,
            control,
            peer,
            next_substream_id: 0usize,
            pending_substreams: FuturesUnordered::new(),
        })
    }

    /// Get next substream ID.
    fn next_substream_id(&mut self) -> usize {
        let substream = self.next_substream_id;
        self.next_substream_id += 1;
        substream
    }
}

#[derive(Debug)]
pub struct Substream {
    /// Substream ID.
    substream: usize,

    /// Yamux substream.
    io: yamux::Stream,
}

impl AsyncRead for Substream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let mut inner = Pin::into_inner(self);
        Pin::new(&mut inner.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Substream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let mut inner = Pin::into_inner(self);
        Pin::new(&mut inner.io).poll_write(cx, &buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let mut inner = Pin::into_inner(self);
        Pin::new(&mut inner.io).poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let mut inner = Pin::into_inner(self);
        Pin::new(&mut inner.io).poll_close(cx)
    }
}

#[async_trait::async_trait]
impl ConnectionNew for TcpConnection {
    type Substream = Substream;

    /// Get remote peer ID.
    fn peer_id(&self) -> &PeerId {
        &self.peer
    }

    /// Open substream for `protocol`.
    fn open_substream(&mut self, protocol: ProtocolName) -> usize {
        let mut control = self.control.clone();
        let substream = self.next_substream_id();

        self.pending_substreams.push(Box::pin(async move {
            Self::open_substream(control, substream, protocol).await
        }));

        substream
    }

    /// Poll next substream.
    async fn next_substream(&mut self) -> crate::Result<Self::Substream> {
        loop {
            tokio::select! {
                substream = self.connection.next() => match substream {
                    Some(Ok(stream)) => {
                        let substream = self.next_substream_id();
                        let protocols = self.config.protocols().clone();

                        self.pending_substreams.push(Box::pin(async move {
                            Self::accept_substream(stream, substream, protocols).await
                        }));
                    },
                    Some(Err(error)) => {
                        tracing::error!(target: LOG_TARGET, ?error, "failed to poll inbound substream");
                        return Err(Error::SubstreamError(SubstreamError::YamuxError(error)));
                    }
                    None => return Err(Error::SubstreamError(SubstreamError::ConnectionClosed)),
                },
                substream = self.pending_substreams.select_next_some(), if !self.pending_substreams.is_empty() => {
                    return substream
                }
            }
        }
    }
}
