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

//! QUIC connection.

use crate::{
    config::Role,
    error::Error,
    multistream_select::{dialer_select_proto, listener_select_proto, Negotiated, Version},
    protocol::{Direction, Permit, ProtocolCommand, ProtocolSet},
    types::{protocol::ProtocolName, ConnectionId, SubstreamId},
    PeerId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, AsyncRead, AsyncWrite};
use quinn::{AcceptBi, Connection as QuinnConnection, RecvStream, SendStream};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

/// Logging target for the file.
const LOG_TARGET: &str = "quic::connection";

/// QUIC connection error.
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

struct NegotiatedSubstream {
    /// Substream direction.
    direction: Direction,

    /// Protocol name.
    protocol: ProtocolName,

    sender: SendStream,
    receiver: RecvStream,

    /// Permit.
    permit: Permit,
}

/// QUIC connection.
pub struct Connection {
    /// Remote peer ID.
    peer: PeerId,

    /// Connection ID.
    connection_id: ConnectionId,

    /// QUIC connection.
    connection: QuinnConnection,

    /// Protocol set.
    protocol_set: ProtocolSet,

    /// Pending substreams.
    pending_substreams:
        FuturesUnordered<BoxFuture<'static, Result<NegotiatedSubstream, ConnectionError>>>,
}

struct BidirectionalSubstream {
    recv_stream: Compat<RecvStream>,
    send_stream: Compat<SendStream>,
}

impl BidirectionalSubstream {
    fn new(recv_stream: Compat<RecvStream>, send_stream: Compat<SendStream>) -> Self {
        Self {
            send_stream,
            recv_stream,
        }
    }
}

impl AsyncRead for BidirectionalSubstream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.recv_stream).poll_read(cx, buf)
    }

    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [std::io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.recv_stream).poll_read_vectored(cx, bufs)
    }
}

impl AsyncWrite for BidirectionalSubstream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.send_stream).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.send_stream).poll_write_vectored(cx, bufs)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.send_stream).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.send_stream).poll_close(cx)
    }
}

impl Connection {
    /// Create new [`Connection`].
    pub fn new(
        peer: PeerId,
        connection_id: ConnectionId,
        connection: QuinnConnection,
        protocol_set: ProtocolSet,
    ) -> Self {
        Self {
            peer,
            connection,
            protocol_set,
            connection_id,
            pending_substreams: FuturesUnordered::new(),
        }
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

    /// Open substream for `protocol`.
    async fn open_substream(
        handle: QuinnConnection,
        permit: Permit,
        direction: Direction,
        protocol: ProtocolName,
        fallback_names: Vec<ProtocolName>,
    ) -> crate::Result<NegotiatedSubstream> {
        tracing::debug!(target: LOG_TARGET, ?protocol, ?direction, "open substream");

        let stream = match handle.open_bi().await {
            Ok((send_stream, recv_stream)) => {
                let receive = TokioAsyncReadCompatExt::compat(recv_stream);
                let send = TokioAsyncWriteCompatExt::compat_write(send_stream);
                BidirectionalSubstream::new(receive, send)
            }
            Err(error) => return Err(Error::Quinn(error)),
        };

        // TODO: protocols don't change after they've been initialized so this should be done only
        // once
        let protocols = std::iter::once(&*protocol)
            .chain(fallback_names.iter().map(|protocol| &**protocol))
            .collect();

        let (io, protocol) = Self::negotiate_protocol(stream, &Role::Dialer, protocols).await?;

        tracing::trace!(
            target: LOG_TARGET,
            ?protocol,
            ?direction,
            "substream accepted and negotiated"
        );

        let stream = io.inner();
        let sender = stream.send_stream.into_inner();
        let receiver = stream.recv_stream.into_inner();

        Ok(NegotiatedSubstream {
            sender,
            receiver,
            direction,
            permit,
            protocol,
        })
    }

    /// Accept bidirectional substream from rmeote peer.
    async fn accept_substream(
        stream: BidirectionalSubstream,
        protocols: Vec<ProtocolName>,
        substream_id: SubstreamId,
        permit: Permit,
    ) -> crate::Result<NegotiatedSubstream> {
        tracing::trace!(
            target: LOG_TARGET,
            ?substream_id,
            "accept inbound substream"
        );

        let protocols = protocols.iter().map(|protocol| &**protocol).collect::<Vec<&str>>();
        let (io, protocol) = Self::negotiate_protocol(stream, &Role::Listener, protocols).await?;

        tracing::trace!(
            target: LOG_TARGET,
            ?substream_id,
            ?protocol,
            "substream accepted and negotiated"
        );

        let stream = io.inner();
        let sender = stream.send_stream.into_inner();
        let receiver = stream.recv_stream.into_inner();

        Ok(NegotiatedSubstream {
            permit,
            sender,
            receiver,
            protocol,
            direction: Direction::Inbound,
        })
    }

    /// Start event loop for [`Connection`].
    pub async fn start(mut self) -> crate::Result<()> {
        loop {
            tokio::select! {
                event = self.connection.accept_bi() => match event {
                    Ok((send_stream, receive_stream)) => {

                        let substream = self.protocol_set.next_substream_id();
                        let protocols = self.protocol_set.protocols();
                        let permit = self.protocol_set.try_get_permit().ok_or(Error::ConnectionClosed)?;
                        let receive = TokioAsyncReadCompatExt::compat(receive_stream);
                        let send = TokioAsyncWriteCompatExt::compat_write(send_stream);
                        let stream = BidirectionalSubstream::new(receive, send);

                        self.pending_substreams.push(Box::pin(async move {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5), // TODO: make this configurable
                                Self::accept_substream(stream, protocols, substream, permit),
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
                    }
                    Err(error) => {
                        tracing::debug!(target: LOG_TARGET, peer = ?self.peer, ?error, "failed to accept substream");
                        return Ok(());
                    }
                },
                command = self.protocol_set.next_event() => match command {
                    None => return Ok(()),
                    Some(ProtocolCommand::OpenSubstream { protocol, fallback_names, substream_id, permit }) => {
                        let connection = self.connection.clone();

                        tracing::trace!(
                            target: LOG_TARGET,
                            ?protocol,
                            ?fallback_names,
                            ?substream_id,
                            "open substream"
                        );

                        self.pending_substreams.push(Box::pin(async move {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5), // TODO: make this configurable
                                Self::open_substream(connection, permit, Direction::Outbound(substream_id), protocol, fallback_names),
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
                    }
                }
            }
        }
    }
}
