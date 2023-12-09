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
    crypto::noise::{self, NoiseSocket},
    error::Error,
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
use tokio_tungstenite::MaybeTlsStream;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use url::Url;

use std::net::SocketAddr;

mod schema {
    pub(super) mod noise {
        include!(concat!(env!("OUT_DIR"), "/noise.rs"));
    }
}

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::websocket::connection";

#[derive(Debug)]
pub struct NegotiatedSubstream {
    /// Substream direction.
    direction: Direction,

    /// Protocol name.
    protocol: ProtocolName,

    /// Yamux substream.
    io: yamux::Stream,

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
        error: Error,
    },
}

/// WebSocket connection.
pub(crate) struct WebSocketConnection {
    /// Protocol context.
    protocol_set: ProtocolSet,

    /// Yamux connection.
    connection: yamux::ControlledConnection<NoiseSocket<BufferedStream<MaybeTlsStream<TcpStream>>>>,

    /// Yamux control.
    control: yamux::Control,

    /// Remote peer ID.
    peer: PeerId,

    /// Connection ID.
    connection_id: ConnectionId,

    /// Bandwidth sink.
    bandwidth_sink: BandwidthSink,

    /// Pending substreams.
    pending_substreams:
        FuturesUnordered<BoxFuture<'static, Result<NegotiatedSubstream, ConnectionError>>>,
}

impl WebSocketConnection {
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
        dialed_peer: Option<PeerId>,
        ws_address: Url,
        connection_id: ConnectionId,
        yamux_config: yamux::Config,
        bandwidth_sink: BandwidthSink,
        mut protocol_set: ProtocolSet,
    ) -> crate::Result<Self> {
        tracing::trace!(target: LOG_TARGET, ?address, ?ws_address, ?connection_id, "open connection to remote peer");

        let (stream, _) = tokio_tungstenite::connect_async(ws_address).await?;
        let stream = BufferedStream::new(stream);

        tracing::trace!(
            target: LOG_TARGET,
            ?address,
            "connection established, negotiate protocols"
        );

        // negotiate `noise`
        let (stream, _) = Self::negotiate_protocol(stream, &Role::Dialer, vec!["/noise"]).await?;

        tracing::trace!(
            target: LOG_TARGET,
            "`multistream-select` and `noise` negotiated"
        );

        // perform noise handshake
        let (stream, peer) =
            noise::handshake(stream.inner(), &protocol_set.keypair, Role::Dialer, 5, 2).await?;
        let stream: NoiseSocket<BufferedStream<_>> = stream;

        // if the local node dialed a remote node, verify that received peer ID matches the one that
        // was transported as part of the noise handshake
        if let Some(dialed_peer) = dialed_peer {
            if dialed_peer != peer {
                tracing::debug!(target: LOG_TARGET, ?dialed_peer, ?peer, "peer id mismatch");
                return Err(Error::PeerIdMismatch(dialed_peer, peer));
            }
        }

        tracing::trace!(target: LOG_TARGET, "noise handshake done");

        // negotiate `yamux`
        let (stream, _) =
            Self::negotiate_protocol(stream, &Role::Dialer, vec!["/yamux/1.0.0"]).await?;
        tracing::trace!(target: LOG_TARGET, "`yamux` negotiated");

        let connection = yamux::Connection::new(stream.inner(), yamux_config, Role::Dialer.into());
        let (control, connection) = yamux::Control::new(connection);

        protocol_set
            .report_connection_established(
                connection_id,
                peer,
                Endpoint::dialer(address.clone(), connection_id),
            )
            .await?;

        Ok(Self {
            peer,
            control,
            connection,
            connection_id,
            protocol_set,
            bandwidth_sink,
            pending_substreams: FuturesUnordered::new(),
        })
    }

    /// Accept WebSocket connection.
    pub(crate) async fn accept_connection(
        stream: TcpStream,
        address: SocketAddr,
        connection_id: ConnectionId,
        yamux_config: yamux::Config,
        bandwidth_sink: BandwidthSink,
        mut protocol_set: ProtocolSet,
    ) -> crate::Result<Self> {
        let stream = MaybeTlsStream::Plain(stream);
        let stream = tokio_tungstenite::accept_async(stream).await?;
        let stream = BufferedStream::new(stream);

        tracing::trace!(
            target: LOG_TARGET,
            ?address,
            "connection received, negotiate protocols"
        );

        // negotiate `noise`
        let (stream, _) = Self::negotiate_protocol(stream, &Role::Dialer, vec!["/noise"]).await?;

        tracing::trace!(
            target: LOG_TARGET,
            "`multistream-select` and `noise` negotiated"
        );

        // perform noise handshake
        let (stream, peer) =
            noise::handshake(stream.inner(), &protocol_set.keypair, Role::Listener, 5, 2).await?;
        let stream: NoiseSocket<BufferedStream<_>> = stream;

        tracing::trace!(target: LOG_TARGET, "noise handshake done");

        // negotiate `yamux`
        let (stream, _) =
            Self::negotiate_protocol(stream, &Role::Listener, vec!["/yamux/1.0.0"]).await?;
        tracing::trace!(target: LOG_TARGET, "`yamux` negotiated");

        let connection =
            yamux::Connection::new(stream.inner(), yamux_config, Role::Listener.into());
        let (control, connection) = yamux::Control::new(connection);

        // create `Multiaddr` from `SocketAddr` and report to protocols
        // and `TransportManager` that a new connection was established
        let address = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::Ws(std::borrow::Cow::Owned("".to_string())))
            .with(Protocol::P2p(Multihash::from(peer)));

        protocol_set
            .report_connection_established(
                connection_id,
                peer,
                Endpoint::listener(address.clone(), connection_id),
            )
            .await?;

        Ok(Self {
            peer,
            control,
            connection,
            connection_id,
            protocol_set,
            bandwidth_sink,
            pending_substreams: FuturesUnordered::new(),
        })
    }

    /// Accept substream.
    pub async fn accept_substream(
        stream: yamux::Stream,
        permit: Permit,
        substream_id: SubstreamId,
        protocols: Vec<ProtocolName>,
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
            "substream accepted and negotiated"
        );

        Ok(NegotiatedSubstream {
            io: io.inner(),
            direction: Direction::Inbound,
            protocol,
            permit,
        })
    }

    /// Open substream for `protocol`.
    pub async fn open_substream(
        mut control: yamux::Control,
        permit: Permit,
        direction: Direction,
        protocol: ProtocolName,
        fallback_names: Vec<ProtocolName>,
    ) -> crate::Result<NegotiatedSubstream> {
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

        // TODO: protocols don't change after they've been initialized so this should be done only
        // once
        let protocols = std::iter::once(&*protocol)
            .chain(fallback_names.iter().map(|protocol| &**protocol))
            .collect();

        let (io, protocol) = Self::negotiate_protocol(stream, &Role::Dialer, protocols).await?;

        Ok(NegotiatedSubstream {
            io: io.inner(),
            direction,
            protocol,
            permit,
        })
    }

    /// Start connection event loop.
    pub(crate) async fn start(mut self) -> crate::Result<()> {
        loop {
            tokio::select! {
                substream = self.connection.next() => match substream {
                    Some(Ok(stream)) => {
                        let substream = self.protocol_set.next_substream_id();
                        let protocols = self.protocol_set.protocols();
                        let permit = self.protocol_set.try_get_permit().ok_or(Error::ConnectionClosed)?;

                        self.pending_substreams.push(Box::pin(async move {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5), // TODO: make this configurable
                                Self::accept_substream(stream, permit, substream, protocols),
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
                                    tracing::warn!(
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
                            let socket = FuturesAsyncReadCompatExt::compat(substream.io);
                            let bandwidth_sink = self.bandwidth_sink.clone();
                            let substream = substream::Substream::new_websocket(
                                self.peer,
                                Substream::new(socket, bandwidth_sink, substream.permit),
                                self.protocol_set.protocol_codec(&protocol)
                            );

                            if let Err(error) = self.protocol_set
                                .report_substream_open(self.peer, protocol, direction, substream)
                                .await
                            {
                                tracing::warn!(
                                    target: LOG_TARGET,
                                    ?error,
                                    "failed to register opened substream to protocol"
                                );
                            }
                        }
                    }
                }
                protocol = self.protocol_set.next_event() => match protocol {
                    Some(ProtocolCommand::OpenSubstream { protocol, fallback_names, substream_id, permit }) => {
                        let control = self.control.clone();

                        tracing::trace!(
                            target: LOG_TARGET,
                            ?protocol,
                            ?substream_id,
                            "open substream"
                        );

                        self.pending_substreams.push(Box::pin(async move {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5), // TODO: make this configurable
                                Self::open_substream(
                                    control,
                                    permit,
                                    Direction::Outbound(substream_id),
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
                    None => {
                        tracing::debug!(target: LOG_TARGET, "protocols have exited, shutting down connection");
                        return self.protocol_set.report_connection_closed(self.peer, self.connection_id).await
                    }
                }
            }
        }
    }
}
