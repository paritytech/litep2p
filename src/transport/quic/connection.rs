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
    error::Error,
    multistream_select::{dialer_select_proto, listener_select_proto, Negotiated, Version},
    peer_id::PeerId,
    protocol::{Direction, ProtocolEvent, ProtocolSet},
    substream::SubstreamType,
    transport::TransportContext,
    types::{protocol::ProtocolName, ConnectionId, SubstreamId},
};

use futures::{future::BoxFuture, stream::FuturesUnordered, AsyncRead, AsyncWrite, StreamExt};
use s2n_quic::{
    connection::{Connection, Handle},
    stream::BidirectionalStream,
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

/// QUIC connection.
pub(crate) struct QuicConnection {
    /// Inner QUIC connection.
    connection: Connection,

    /// Remote peer ID.
    peer: PeerId,

    /// Connection ID.
    _connection_id: ConnectionId,

    /// Transport context.
    context: ProtocolSet,

    /// Next substream ID.
    next_substream_id: SubstreamId,

    /// Pending substreams.
    pending_substreams: FuturesUnordered<BoxFuture<'static, Result<Substream, ConnectionError>>>,
}

// TODO: this is not needed anymore
#[derive(Debug)]
pub struct Substream {
    /// Substream direction.
    direction: Direction,

    /// Protocol name.
    protocol: ProtocolName,

    /// `s2n-quic` stream.
    io: BidirectionalStream,
}

impl QuicConnection {
    /// Create new [`QuiConnection`].
    pub(crate) async fn new(
        peer: PeerId,
        context: TransportContext,
        connection: Connection,
        _connection_id: ConnectionId,
    ) -> crate::Result<Self> {
        Ok(Self {
            peer,
            connection,
            _connection_id,
            next_substream_id: 0usize,
            pending_substreams: FuturesUnordered::new(),
            context: ProtocolSet::from_transport_context(peer, context).await?,
        })
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

    /// Get next substream ID.
    fn next_substream_id(&mut self) -> usize {
        let substream = self.next_substream_id;
        self.next_substream_id += 1;
        substream
    }

    /// Open substream for `protocol`.
    pub async fn open_substream(
        mut handle: Handle,
        direction: Direction,
        protocol: ProtocolName,
    ) -> crate::Result<Substream> {
        tracing::debug!(target: LOG_TARGET, ?protocol, ?direction, "open substream");

        let stream = match handle.open_bidirectional_stream().await {
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
                return Err(Error::Unknown);
                // return Err(Error::YamuxError(direction, error));
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

    /// Accept substream.
    pub async fn accept_substream(
        stream: BidirectionalStream,
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

    /// Start [`QuicConnection`] event loop.
    pub(crate) async fn start(mut self) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, "starting quic connection handler");

        loop {
            tokio::select! {
                substream = self.connection.accept_bidirectional_stream() => match substream {
                    Ok(Some(stream)) => {
                        let substream = self.next_substream_id();
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
                    }
                    // TODO: shut down the connection?
                    Ok(None) => todo!(),
                    Err(_error) => todo!(),
                },
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
                            let substream = substream.io;

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
                        let handle = self.connection.handle();

                        tracing::trace!(
                            target: LOG_TARGET,
                            ?protocol,
                            ?substream_id,
                            "open substream"
                        );

                        self.pending_substreams.push(Box::pin(async move {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                Self::open_substream(handle, Direction::Outbound(substream_id), protocol.clone()),
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
