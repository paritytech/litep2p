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
        noise::{self, NoiseSocket},
    },
    error::{Error, NegotiationError, SubstreamError},
    multistream_select::{dialer_select_proto, listener_select_proto, Negotiated, Version},
    protocol::{Direction, Permit, ProtocolCommand, ProtocolSet},
    substream,
    transport::{
        websocket::{stream::BufferedStream, substream::Substream},
        Endpoint,
    },
    types::{protocol::ProtocolName, ConnectionId, SubstreamId},
    BandwidthSink, PeerId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, AsyncRead, AsyncWrite, StreamExt};
use multiaddr::{multihash::Multihash, Multiaddr, Protocol};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::compat::FuturesAsyncReadCompatExt;
use url::Url;

use std::time::Duration;

mod schema {
    pub(super) mod noise {
        include!(concat!(env!("OUT_DIR"), "/noise.rs"));
    }
}

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::websocket::connection";

/// Negotiated substream and its context.
pub struct NegotiatedSubstream {
    /// Substream direction.
    direction: Direction,

    /// Substream ID.
    substream_id: SubstreamId,

    /// Protocol name.
    protocol: ProtocolName,

    /// Yamux substream.
    io: crate::yamux::Stream,

    /// Permit.
    permit: Permit,
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
        error: SubstreamError,
    },
}

/// Negotiated connection.
pub(super) struct NegotiatedConnection {
    /// Remote peer ID.
    peer: PeerId,

    /// Endpoint.
    endpoint: Endpoint,

    /// Yamux connection.
    connection:
        crate::yamux::ControlledConnection<NoiseSocket<BufferedStream<MaybeTlsStream<TcpStream>>>>,

    /// Yamux control.
    control: crate::yamux::Control,
}

impl NegotiatedConnection {
    /// Get `ConnectionId` of the negotiated connection.
    pub fn connection_id(&self) -> ConnectionId {
        self.endpoint.connection_id()
    }

    /// Get `PeerId` of the negotiated connection.
    pub fn peer(&self) -> PeerId {
        self.peer
    }

    /// Get `Endpoint` of the negotiated connection.
    pub fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }
}

/// WebSocket connection.
pub(crate) struct WebSocketConnection {
    /// Protocol context.
    protocol_set: ProtocolSet,

    /// Yamux connection.
    connection:
        crate::yamux::ControlledConnection<NoiseSocket<BufferedStream<MaybeTlsStream<TcpStream>>>>,

    /// Yamux control.
    control: crate::yamux::Control,

    /// Remote peer ID.
    peer: PeerId,

    /// Endpoint.
    endpoint: Endpoint,

    /// Substream open timeout.
    substream_open_timeout: Duration,

    /// Connection ID.
    connection_id: ConnectionId,

    /// Bandwidth sink.
    bandwidth_sink: BandwidthSink,

    /// Pending substreams.
    pending_substreams:
        FuturesUnordered<BoxFuture<'static, Result<NegotiatedSubstream, ConnectionError>>>,
}

impl WebSocketConnection {
    /// Create new [`WebSocketConnection`].
    pub(super) fn new(
        connection: NegotiatedConnection,
        protocol_set: ProtocolSet,
        bandwidth_sink: BandwidthSink,
        substream_open_timeout: Duration,
    ) -> Self {
        let NegotiatedConnection {
            peer,
            endpoint,
            connection,
            control,
        } = connection;

        Self {
            connection_id: endpoint.connection_id(),
            protocol_set,
            connection,
            control,
            peer,
            endpoint,
            bandwidth_sink,
            substream_open_timeout,
            pending_substreams: FuturesUnordered::new(),
        }
    }

    /// Negotiate protocol.
    async fn negotiate_protocol<S: AsyncRead + AsyncWrite + Unpin>(
        stream: S,
        role: &Role,
        protocols: Vec<&str>,
    ) -> Result<(Negotiated<S>, ProtocolName), NegotiationError> {
        tracing::trace!(target: LOG_TARGET, ?protocols, "negotiating protocols");

        let (protocol, socket) = match role {
            Role::Dialer => dialer_select_proto(stream, protocols, Version::V1).await,
            Role::Listener => listener_select_proto(stream, protocols).await,
        }
        .map_err(NegotiationError::MultistreamSelectError)?;

        tracing::trace!(target: LOG_TARGET, ?protocol, "protocol negotiated");

        Ok((socket, ProtocolName::from(protocol.to_string())))
    }

    /// Open WebSocket connection.
    pub(super) async fn open_connection(
        connection_id: ConnectionId,
        keypair: Keypair,
        stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
        address: Multiaddr,
        dialed_peer: PeerId,
        ws_address: Url,
        yamux_config: crate::yamux::Config,
        max_read_ahead_factor: usize,
        max_write_buffer_size: usize,
    ) -> Result<NegotiatedConnection, NegotiationError> {
        tracing::trace!(
            target: LOG_TARGET,
            ?address,
            ?ws_address,
            ?connection_id,
            "open connection to remote peer",
        );

        Self::negotiate_connection(
            stream,
            Some(dialed_peer),
            Role::Dialer,
            address,
            connection_id,
            keypair,
            yamux_config,
            max_read_ahead_factor,
            max_write_buffer_size,
        )
        .await
    }

    /// Accept WebSocket connection.
    pub(super) async fn accept_connection(
        stream: TcpStream,
        connection_id: ConnectionId,
        keypair: Keypair,
        address: Multiaddr,
        yamux_config: crate::yamux::Config,
        max_read_ahead_factor: usize,
        max_write_buffer_size: usize,
    ) -> Result<NegotiatedConnection, NegotiationError> {
        let stream = MaybeTlsStream::Plain(stream);

        Self::negotiate_connection(
            tokio_tungstenite::accept_async(stream)
                .await
                .map_err(NegotiationError::WebSocket)?,
            None,
            Role::Listener,
            address,
            connection_id,
            keypair,
            yamux_config,
            max_read_ahead_factor,
            max_write_buffer_size,
        )
        .await
    }

    /// Negotiate WebSocket connection.
    pub(super) async fn negotiate_connection(
        stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
        dialed_peer: Option<PeerId>,
        role: Role,
        address: Multiaddr,
        connection_id: ConnectionId,
        keypair: Keypair,
        yamux_config: crate::yamux::Config,
        max_read_ahead_factor: usize,
        max_write_buffer_size: usize,
    ) -> Result<NegotiatedConnection, NegotiationError> {
        tracing::trace!(
            target: LOG_TARGET,
            ?connection_id,
            ?address,
            ?role,
            ?dialed_peer,
            "negotiate connection"
        );
        let stream = BufferedStream::new(stream);

        // negotiate `noise`
        let (stream, _) = Self::negotiate_protocol(stream, &role, vec!["/noise"]).await?;

        tracing::trace!(
            target: LOG_TARGET,
            "`multistream-select` and `noise` negotiated"
        );

        // perform noise handshake
        let (stream, peer) = noise::handshake(
            stream.inner(),
            &keypair,
            role,
            max_read_ahead_factor,
            max_write_buffer_size,
        )
        .await?;

        if let Some(dialed_peer) = dialed_peer {
            if peer != dialed_peer {
                return Err(NegotiationError::PeerIdMismatch(dialed_peer, peer));
            }
        }

        let stream: NoiseSocket<BufferedStream<_>> = stream;
        tracing::trace!(target: LOG_TARGET, "noise handshake done");

        // negotiate `yamux`
        let (stream, _) = Self::negotiate_protocol(stream, &role, vec!["/yamux/1.0.0"]).await?;
        tracing::trace!(target: LOG_TARGET, "`yamux` negotiated");

        let connection = crate::yamux::Connection::new(stream.inner(), yamux_config, role.into());
        let (control, connection) = crate::yamux::Control::new(connection);

        let address = match role {
            Role::Dialer => address,
            Role::Listener => address.with(Protocol::P2p(Multihash::from(peer))),
        };

        Ok(NegotiatedConnection {
            peer,
            control,
            connection,
            endpoint: match role {
                Role::Dialer => Endpoint::dialer(address, connection_id),
                Role::Listener => Endpoint::listener(address, connection_id),
            },
        })
    }

    /// Accept substream.
    pub async fn accept_substream(
        stream: crate::yamux::Stream,
        permit: Permit,
        substream_id: SubstreamId,
        protocols: Vec<ProtocolName>,
    ) -> Result<NegotiatedSubstream, NegotiationError> {
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
            "substream accepted and negotiated"
        );

        Ok(NegotiatedSubstream {
            io: io.inner(),
            direction: Direction::Inbound,
            substream_id,
            protocol,
            permit,
        })
    }

    /// Open substream for `protocol`.
    pub async fn open_substream(
        mut control: crate::yamux::Control,
        permit: Permit,
        substream_id: SubstreamId,
        protocol: ProtocolName,
        fallback_names: Vec<ProtocolName>,
    ) -> Result<NegotiatedSubstream, SubstreamError> {
        tracing::debug!(target: LOG_TARGET, ?protocol, ?substream_id, "open substream");

        let stream = match control.open_stream().await {
            Ok(stream) => {
                tracing::trace!(target: LOG_TARGET, ?substream_id, "substream opened");
                stream
            }
            Err(error) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?substream_id,
                    ?error,
                    "failed to open substream"
                );
                return Err(SubstreamError::YamuxError(
                    error,
                    Direction::Outbound(substream_id),
                ));
            }
        };

        // TODO: protocols don't change after they've been initialized so this should be done only
        // once
        let protocols = std::iter::once(&*protocol)
            .chain(fallback_names.iter().map(|protocol| &**protocol))
            .collect();

        let (io, protocol) = Self::negotiate_protocol(stream, &Role::Dialer, protocols).await?;

        Ok(NegotiatedSubstream {
            io: io.inner(),
            substream_id,
            direction: Direction::Outbound(substream_id),
            protocol,
            permit,
        })
    }

    /// Start connection event loop.
    pub(crate) async fn start(mut self) -> crate::Result<()> {
        self.protocol_set
            .report_connection_established(self.peer, self.endpoint)
            .await?;

        loop {
            tokio::select! {
                substream = self.connection.next() => match substream {
                    Some(Ok(stream)) => {
                        let substream = self.protocol_set.next_substream_id();
                        let protocols = self.protocol_set.protocols();
                        let permit = self.protocol_set.try_get_permit().ok_or(Error::ConnectionClosed)?;
                        let substream_open_timeout = self.substream_open_timeout;

                        self.pending_substreams.push(Box::pin(async move {
                            match tokio::time::timeout(
                                substream_open_timeout,
                                Self::accept_substream(stream, permit, substream, protocols),
                            )
                            .await
                            {
                                Ok(Ok(substream)) => Ok(substream),
                                Ok(Err(error)) => Err(ConnectionError::FailedToNegotiate {
                                    protocol: None,
                                    substream_id: None,
                                    error: SubstreamError::NegotiationError(error),
                                }),
                                Err(_) => Err(ConnectionError::Timeout {
                                    protocol: None,
                                    substream_id: None
                                }),
                            }
                        }));
                    },
                    Some(Err(error)) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            peer = ?self.peer,
                            ?error,
                            "connection closed with error"
                        );
                        self.protocol_set.report_connection_closed(self.peer, self.connection_id).await?;

                        return Ok(())
                    }
                    None => {
                        tracing::debug!(target: LOG_TARGET, peer = ?self.peer, "connection closed");
                        self.protocol_set.report_connection_closed(self.peer, self.connection_id).await?;

                        return Ok(())
                    }
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
                                    (protocol, substream_id, SubstreamError::NegotiationError(NegotiationError::Timeout))
                                }
                                ConnectionError::FailedToNegotiate { protocol, substream_id, error } => {
                                    (protocol, substream_id, error)
                                }
                            };

                            if let (Some(protocol), Some(substream_id)) = (protocol, substream_id) {
                                self.protocol_set
                                    .report_substream_open_failure(protocol, substream_id, error)
                                    .await?;
                            }
                        }
                        Ok(substream) => {
                            let protocol = substream.protocol.clone();
                            let direction = substream.direction;
                            let substream_id = substream.substream_id;
                            let socket = FuturesAsyncReadCompatExt::compat(substream.io);
                            let bandwidth_sink = self.bandwidth_sink.clone();

                            let substream = substream::Substream::new_websocket(
                                self.peer,
                                substream_id,
                                Substream::new(socket, bandwidth_sink, substream.permit),
                                self.protocol_set.protocol_codec(&protocol)
                            );

                            self.protocol_set
                                .report_substream_open(self.peer, protocol, direction, substream)
                                .await?;
                        }
                    }
                }
                protocol = self.protocol_set.next() => match protocol {
                    Some(ProtocolCommand::OpenSubstream { protocol, fallback_names, substream_id, permit, .. }) => {
                        let control = self.control.clone();
                        let substream_open_timeout = self.substream_open_timeout;

                        tracing::trace!(
                            target: LOG_TARGET,
                            ?protocol,
                            ?substream_id,
                            "open substream"
                        );

                        self.pending_substreams.push(Box::pin(async move {
                            match tokio::time::timeout(
                                substream_open_timeout,
                                Self::open_substream(
                                    control,
                                    permit,
                                    substream_id,
                                    protocol.clone(),
                                    fallback_names
                                ),
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
                    Some(ProtocolCommand::ForceClose) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            peer = ?self.peer,
                            connection_id = ?self.connection_id,
                            "force closing connection",
                        );

                        return self.protocol_set.report_connection_closed(self.peer, self.connection_id).await
                    }
                    None => {
                        tracing::debug!(target: LOG_TARGET, "protocols have exited, shutting down connection");
                        return self.protocol_set.report_connection_closed(self.peer, self.connection_id).await
                    }
                }
            }
        }
    }
}
