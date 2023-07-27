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
    crypto::noise::{self, Encrypted, NoiseConfiguration},
    error::{Error, SubstreamError},
    multistream_select::{dialer_select_proto, listener_select_proto, Negotiated, Version},
    peer_id::PeerId,
    protocol::{Direction, ProtocolEvent, ProtocolSet},
    substream::SubstreamType,
    transport::{websocket::stream::BufferedStream, TransportContext},
    types::{protocol::ProtocolName, SubstreamId},
};

use futures::{future::BoxFuture, stream::FuturesUnordered, AsyncRead, AsyncWrite, StreamExt};
use multiaddr::{multihash::Multihash, Multiaddr, Protocol};
use tokio::net::TcpStream;
use tokio_tungstenite::MaybeTlsStream;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use url::Url;

use std::{io, net::SocketAddr, pin::Pin};

mod schema {
    pub(super) mod noise {
        include!(concat!(env!("OUT_DIR"), "/noise.rs"));
    }
}

/// Logging target for the file.
const LOG_TARGET: &str = "websocket::connection";

/// Convert `Multiaddr` into `url::Url`
fn multiaddr_into_url(address: Multiaddr) -> crate::Result<Url> {
    let mut protocol_stack = address.iter();

    let ip4 = match protocol_stack
        .next()
        .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
    {
        Protocol::Ip4(address) => address.to_string(),
        _ => return Err(Error::TransportNotSupported(address)),
    };

    match protocol_stack
        .next()
        .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
    {
        Protocol::Tcp(port) => match protocol_stack.next() {
            Some(Protocol::Ws(_)) => {
                url::Url::parse(&format!("ws://{ip4}:{port}/")).map_err(|_| Error::InvalidData)
            }
            _ => return Err(Error::TransportNotSupported(address.clone())),
        },
        _ => Err(Error::TransportNotSupported(address)),
    }
}

/// WebSocket connection error.
#[derive(Debug)]
enum ConnectionError {
    /// Timeout
    Timeout {
        /// Protocol.
        protocol: Option<ProtocolName>,

        /// Substream ID.
        substream_id: Option<SubstreamId>,
    },

    /// Failed to negotiate connection/substream.
    FailedToNegotiate {
        /// Protocol.
        protocol: Option<ProtocolName>,

        /// Substream ID.
        substream_id: Option<SubstreamId>,

        /// Error.
        error: Error,
    },
}

/// WebSocket connection.
pub(crate) struct WebSocketConnection {
    /// Protocol context.
    context: ProtocolSet,

    /// Yamux connection.
    connection: yamux::ControlledConnection<Encrypted<BufferedStream<MaybeTlsStream<TcpStream>>>>,

    /// Yamux control.
    control: yamux::Control,

    /// Remote peer ID.
    peer: PeerId,

    /// Next substream ID.
    next_substream_id: SubstreamId,

    /// Remote address.
    address: Multiaddr,

    /// Pending substreams.
    pending_substreams: FuturesUnordered<BoxFuture<'static, Result<Substream, ConnectionError>>>,
}

impl WebSocketConnection {
    /// Get remote peer ID.
    pub(crate) fn peer(&self) -> &PeerId {
        &self.peer
    }

    /// Get remote address.
    pub(crate) fn address(&self) -> &Multiaddr {
        &self.address
    }

    /// Negotiate protocol.
    async fn negotiate_protocol<S: AsyncRead + AsyncWrite + Unpin>(
        stream: S,
        role: &Role,
        protocols: Vec<&str>,
    ) -> crate::Result<(Negotiated<S>, ProtocolName)> {
        tracing::trace!(target: LOG_TARGET, ?protocols, "negotiating protocols");

        let (protocol, socket) = match role {
            Role::Dialer => dialer_select_proto(stream, protocols, Version::V1).await?,
            Role::Listener => listener_select_proto(stream, protocols).await?,
        };

        tracing::trace!(target: LOG_TARGET, ?protocol, "protocol negotiated");

        Ok((socket, ProtocolName::from(protocol.to_string())))
    }

    /// Open WebSocket connection.
    pub(crate) async fn open_connection(
        address: Multiaddr,
        context: TransportContext,
    ) -> crate::Result<Self> {
        let (stream, _) =
            tokio_tungstenite::connect_async(multiaddr_into_url(address.clone())?).await?;
        let stream = BufferedStream::new(stream);

        tracing::error!(
            target: LOG_TARGET,
            ?address,
            "connection established, negotiate protocols"
        );

        // negotiate `noise`
        let noise_config = NoiseConfiguration::new(&context.keypair, Role::Dialer);
        let (stream, _) =
            Self::negotiate_protocol(stream, &noise_config.role, vec!["/noise"]).await?;

        tracing::trace!(
            target: LOG_TARGET,
            "`multistream-select` and `noise` negotiated"
        );

        // perform noise handshake
        let (stream, peer) = noise::handshake(stream.inner(), noise_config).await?;
        tracing::trace!(target: LOG_TARGET, "noise handshake done");
        let stream: Encrypted<BufferedStream<_>> = stream;

        // negotiate `yamux`
        let (stream, _) =
            Self::negotiate_protocol(stream, &Role::Dialer, vec!["/yamux/1.0.0"]).await?;
        tracing::trace!(target: LOG_TARGET, "`yamux` negotiated");

        let connection = yamux::Connection::new(
            stream.inner(),
            yamux::Config::default(),
            Role::Dialer.into(),
        );
        let (control, connection) = yamux::Control::new(connection);
        let context = ProtocolSet::from_transport_context(peer, context).await?;

        Ok(Self {
            peer,
            control,
            connection,
            context,
            next_substream_id: SubstreamId::new(),
            pending_substreams: FuturesUnordered::new(),
            address,
        })
    }

    /// Accept WebSocket connection.
    pub(crate) async fn accept_connection(
        stream: TcpStream,
        address: SocketAddr,
        context: TransportContext,
    ) -> crate::Result<Self> {
        let stream = MaybeTlsStream::Plain(stream);
        let stream = tokio_tungstenite::accept_async(stream).await?;
        let stream = BufferedStream::new(stream);

        tracing::error!(
            target: LOG_TARGET,
            ?address,
            "connection received, negotiate protocols"
        );

        // negotiate `noise`
        let noise_config = NoiseConfiguration::new(&context.keypair, Role::Listener);
        let (stream, _) =
            Self::negotiate_protocol(stream, &noise_config.role, vec!["/noise"]).await?;

        tracing::trace!(
            target: LOG_TARGET,
            "`multistream-select` and `noise` negotiated"
        );

        // perform noise handshake
        let (stream, peer) = noise::handshake(stream.inner(), noise_config).await?;
        tracing::trace!(target: LOG_TARGET, "noise handshake done");
        let stream: Encrypted<BufferedStream<_>> = stream;

        // negotiate `yamux`
        let (stream, _) =
            Self::negotiate_protocol(stream, &Role::Listener, vec!["/yamux/1.0.0"]).await?;
        tracing::trace!(target: LOG_TARGET, "`yamux` negotiated");

        let connection = yamux::Connection::new(
            stream.inner(),
            yamux::Config::default(),
            Role::Listener.into(),
        );
        let (control, connection) = yamux::Control::new(connection);
        let context = ProtocolSet::from_transport_context(peer, context).await?;

        Ok(Self {
            peer,
            control,
            connection,
            context,
            next_substream_id: SubstreamId::new(),
            pending_substreams: FuturesUnordered::new(),
            address: Multiaddr::empty()
                .with(Protocol::from(address.ip()))
                .with(Protocol::Tcp(address.port()))
                .with(Protocol::Ws(std::borrow::Cow::Owned("".to_string())))
                .with(Protocol::P2p(Multihash::from(peer))),
        })
    }

    /// Accept substream.
    pub async fn accept_substream(
        stream: yamux::Stream,
        substream_id: SubstreamId,
        protocols: Vec<ProtocolName>,
    ) -> crate::Result<Substream> {
        tracing::trace!(
            target: LOG_TARGET,
            ?substream_id,
            "accept inbound substream"
        );

        let protocols = protocols
            .iter()
            .map(|protocol| &**protocol)
            .collect::<Vec<&str>>();
        let (io, protocol) = Self::negotiate_protocol(stream, &Role::Listener, protocols).await?;

        tracing::trace!(
            target: LOG_TARGET,
            ?substream_id,
            "substream accepted and negotiated"
        );

        Ok(Substream {
            io: io.inner(),
            direction: Direction::Inbound,
            protocol,
        })
    }

    /// Open substream for `protocol`.
    pub async fn open_substream(
        mut control: yamux::Control,
        direction: Direction,
        protocol: ProtocolName,
    ) -> crate::Result<Substream> {
        tracing::debug!(target: LOG_TARGET, ?protocol, ?direction, "open substream");

        let stream = match control.open_stream().await {
            Ok(stream) => {
                tracing::trace!(target: LOG_TARGET, ?direction, "substream opened");
                stream
            }
            Err(error) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?direction,
                    ?error,
                    "failed to open substream"
                );
                return Err(Error::YamuxError(direction, error));
            }
        };

        let (io, protocol) =
            Self::negotiate_protocol(stream, &Role::Dialer, vec![&protocol]).await?;

        Ok(Substream {
            io: io.inner(),
            direction,
            protocol,
        })
    }

    /// Start connection event loop.
    pub(crate) async fn start(mut self) -> crate::Result<()> {
        loop {
            tokio::select! {
                substream = self.connection.next() => match substream {
                    Some(Ok(stream)) => {
                        let substream = self.next_substream_id.next();
                        let protocols = self.context.protocols.keys().cloned().collect();

                        self.pending_substreams.push(Box::pin(async move {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                Self::accept_substream(stream, substream, protocols),
                            )
                            .await
                            {
                                Ok(Ok(substream)) => Ok(substream),
                                Ok(Err(error)) => Err(ConnectionError::FailedToNegotiate {
                                    protocol: None,
                                    substream_id: None,
                                    error,
                                }),
                                Err(_) => Err(ConnectionError::Timeout {
                                    protocol: None,
                                    substream_id: None
                                }),
                            }
                        }));
                    },
                    Some(Err(error)) => {
                        tracing::error!(target: LOG_TARGET, ?error, "failed to poll inbound substream");
                        // TODO: this is probably not correct
                        return Err(Error::SubstreamError(SubstreamError::YamuxError(error)));
                    }
                    // TODO: this is probably not correct
                    None => return Err(Error::SubstreamError(SubstreamError::ConnectionClosed)),
                },
                // TODO: move this to a function
                substream = self.pending_substreams.select_next_some(), if !self.pending_substreams.is_empty() => {
                    match substream {
                        // TODO: return error to protocol
                        Err(error) => {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?error,
                                "failed to accept/open substream",
                            );

                            let (protocol, substream_id, error) = match error {
                                ConnectionError::Timeout { protocol, substream_id } => {
                                    (protocol, substream_id, Error::Timeout)
                                }
                                ConnectionError::FailedToNegotiate { protocol, substream_id, error } => {
                                    (protocol, substream_id, error)
                                }
                            };

                            match (protocol, substream_id) {
                                (Some(protocol), Some(substream_id)) => {
                                    if let Err(error) = self.context
                                        .report_substream_open_failure(protocol, substream_id, error)
                                        .await
                                    {
                                        tracing::error!(
                                            target: LOG_TARGET,
                                            ?error,
                                            "failed to register opened substream to protocol"
                                        );
                                    }
                                }
                                _ => {}
                            }
                        }
                        Ok(substream) => {
                            let protocol = substream.protocol.clone();
                            let direction = substream.direction;
                            let substream = FuturesAsyncReadCompatExt::compat(substream);

                            if let Err(error) = self.context
                                .report_substream_open(self.peer, protocol, direction, SubstreamType::Raw(substream))
                                .await
                            {
                                tracing::error!(
                                    target: LOG_TARGET,
                                    ?error,
                                    "failed to register opened substream to protocol"
                                );
                            }
                        }
                    }
                }
                protocol = self.context.next_event() => match protocol {
                    Some(ProtocolEvent::OpenSubstream { protocol, substream_id }) => {
                        let control = self.control.clone();

                        tracing::trace!(
                            target: LOG_TARGET,
                            ?protocol,
                            ?substream_id,
                            "open substream"
                        );

                        self.pending_substreams.push(Box::pin(async move {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                Self::open_substream(control, Direction::Outbound(substream_id), protocol.clone()),
                            )
                            .await
                            {
                                Ok(Ok(substream)) => Ok(substream),
                                Ok(Err(error)) => Err(ConnectionError::FailedToNegotiate {
                                    protocol: Some(protocol),
                                    substream_id: Some(substream_id),
                                    error,
                                }),
                                Err(_) => Err(ConnectionError::Timeout {
                                    protocol: Some(protocol),
                                    substream_id: Some(substream_id)
                                }),
                            }
                        }));
                    }
                    None => {
                        tracing::error!(target: LOG_TARGET, "protocols have exited, shutting down connection");
                        return Ok(())
                    }
                }
            }
        }
    }
}

// TODO: this is not needed anymore
#[derive(Debug)]
pub struct Substream {
    /// Substream direction.
    direction: Direction,

    /// Protocol name.
    protocol: ProtocolName,

    /// Yamux substream.
    io: yamux::Stream,
}

impl AsyncRead for Substream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let inner = Pin::into_inner(self);
        Pin::new(&mut inner.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Substream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let inner = Pin::into_inner(self);
        Pin::new(&mut inner.io).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let inner = Pin::into_inner(self);
        Pin::new(&mut inner.io).poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let inner = Pin::into_inner(self);
        Pin::new(&mut inner.io).poll_close(cx)
    }
}
