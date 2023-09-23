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
    substream,
    transport::quic::substream::{NegotiatingSubstream, Substream},
    types::{protocol::ProtocolName, ConnectionId, SubstreamId},
    PeerId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, AsyncRead, AsyncWrite, StreamExt};
use quinn::{Connection as QuinnConnection, RecvStream, SendStream};

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

    /// Substream used to send data.
    sender: SendStream,

    /// Substream used to receive data.
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
            Ok((send_stream, recv_stream)) => NegotiatingSubstream::new(send_stream, recv_stream),
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
        let (sender, receiver) = stream.into_parts();

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
        stream: NegotiatingSubstream,
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
        let (sender, receiver) = stream.into_parts();

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
                        let stream = NegotiatingSubstream::new(send_stream, receive_stream);

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
                        return self.protocol_set.report_connection_closed(self.peer, self.connection_id).await;
                    }
                },
                substream = self.pending_substreams.select_next_some(), if !self.pending_substreams.is_empty() => {
                    match substream {
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

                            if let (Some(protocol), Some(substream_id)) = (protocol, substream_id) {
                                if let Err(error) = self.protocol_set
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
                        }
                        Ok(substream) => {
                            let protocol = substream.protocol.clone();
                            let direction = substream.direction;
                            let substream = substream::Substream::new_quic(
                                self.peer,
                                Substream::new(substream.permit, substream.sender, substream.receiver),
                                self.protocol_set.protocol_codec(&protocol)
                            );

                            if let Err(error) = self.protocol_set
                                .report_substream_open(self.peer, protocol, direction, substream)
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
                command = self.protocol_set.next_event() => match command {
                    None => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            peer = ?self.peer,
                            connection_id = ?self.connection_id,
                            "protocols have dropped connection"
                        );
                        return self.protocol_set.report_connection_closed(self.peer, self.connection_id).await;
                    }
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
