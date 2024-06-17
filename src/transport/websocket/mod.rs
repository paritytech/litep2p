// Copyright 2023 litep2p developers
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rigts to use, copy, modify, merge, publish, distribute, sublicense,
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

//! WebSocket transport.

use crate::{
    config::Role,
    error::{AddressError, Error},
    transport::{
        common::listener::{DialAddresses, GetSocketAddr, SocketListener, WebSocketAddress},
        manager::TransportHandle,
        websocket::{
            config::Config,
            connection::{NegotiatedConnection, WebSocketConnection},
        },
        Transport, TransportBuilder, TransportEvent,
    },
    types::ConnectionId,
    PeerId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, Stream, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use socket2::{Domain, Socket, Type};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use url::Url;

use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

pub(crate) use substream::Substream;

mod connection;
mod stream;
mod substream;

pub mod config;

#[derive(Debug)]
pub(super) struct WebSocketError {
    /// Error.
    error: Error,

    /// Connection ID.
    connection_id: Option<ConnectionId>,
}

impl WebSocketError {
    pub fn new(error: Error, connection_id: Option<ConnectionId>) -> Self {
        Self {
            error,
            connection_id,
        }
    }
}

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::websocket";

/// WebSocket transport.
pub(crate) struct WebSocketTransport {
    /// Transport context.
    context: TransportHandle,

    /// Transport configuration.
    config: Config,

    /// WebSocket listener.
    listener: SocketListener,

    /// Dial addresses.
    dial_addresses: DialAddresses,

    /// Pending dials.
    pending_dials: HashMap<ConnectionId, Multiaddr>,

    /// Pending connections.
    pending_connections:
        FuturesUnordered<BoxFuture<'static, Result<NegotiatedConnection, WebSocketError>>>,

    /// Pending raw, unnegotiated connections.
    pending_raw_connections: FuturesUnordered<
        BoxFuture<
            'static,
            Result<
                (
                    ConnectionId,
                    Multiaddr,
                    WebSocketStream<MaybeTlsStream<TcpStream>>,
                ),
                ConnectionId,
            >,
        >,
    >,

    /// Opened raw connection, waiting for approval/rejection from `TransportManager`.
    opened_raw: HashMap<ConnectionId, (WebSocketStream<MaybeTlsStream<TcpStream>>, Multiaddr)>,

    /// Canceled raw connections.
    canceled: HashSet<ConnectionId>,

    /// Negotiated connections waiting validation.
    pending_open: HashMap<ConnectionId, NegotiatedConnection>,
}

impl WebSocketTransport {
    /// Convert `Multiaddr` into `url::Url`
    fn multiaddr_into_url(address: Multiaddr) -> crate::Result<(Url, PeerId)> {
        let mut protocol_stack = address.iter();

        let dial_address = match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
        {
            Protocol::Ip4(address) => address.to_string(),
            Protocol::Ip6(address) => format!("[{address}]"),
            Protocol::Dns(address) | Protocol::Dns4(address) | Protocol::Dns6(address) =>
                address.to_string(),

            _ => return Err(Error::TransportNotSupported(address)),
        };

        let url = match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
        {
            Protocol::Tcp(port) => match protocol_stack.next() {
                Some(Protocol::Ws(_)) => format!("ws://{dial_address}:{port}/"),
                Some(Protocol::Wss(_)) => format!("wss://{dial_address}:{port}/"),
                _ => return Err(Error::TransportNotSupported(address.clone())),
            },
            _ => return Err(Error::TransportNotSupported(address)),
        };

        let peer = match protocol_stack.next() {
            Some(Protocol::P2p(multihash)) => PeerId::from_multihash(multihash)?,
            protocol => {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `Protocol::Ws`/`Protocol::Wss`",
                );
                return Err(Error::AddressError(AddressError::PeerIdMissing));
            }
        };

        tracing::trace!(target: LOG_TARGET, ?url, "parse address");

        url::Url::parse(&url).map(|url| (url, peer)).map_err(|_| Error::InvalidData)
    }

    /// Dial remote peer over `address`.
    async fn dial_peer(
        address: Multiaddr,
        dial_addresses: DialAddresses,
        connection_open_timeout: Duration,
        nodelay: bool,
    ) -> crate::Result<(Multiaddr, WebSocketStream<MaybeTlsStream<TcpStream>>)> {
        let (url, _) = Self::multiaddr_into_url(address.clone())?;

        let (socket_address, _) = WebSocketAddress::multiaddr_to_socket_address(&address)?;
        let remote_address =
            match tokio::time::timeout(connection_open_timeout, socket_address.lookup_ip()).await {
                Err(_) => return Err(Error::Timeout),
                Ok(Err(error)) => return Err(error),
                Ok(Ok(address)) => address,
            };

        let domain = match remote_address.is_ipv4() {
            true => Domain::IPV4,
            false => Domain::IPV6,
        };
        let socket = Socket::new(domain, Type::STREAM, Some(socket2::Protocol::TCP))?;
        if remote_address.is_ipv6() {
            socket.set_only_v6(true)?;
        }
        socket.set_nonblocking(true)?;
        socket.set_nodelay(nodelay)?;

        match dial_addresses.local_dial_address(&remote_address.ip()) {
            Ok(Some(dial_address)) => {
                socket.set_reuse_address(true)?;
                #[cfg(unix)]
                socket.set_reuse_port(true)?;
                socket.bind(&dial_address.into())?;
            }
            Ok(None) => {}
            Err(()) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?remote_address,
                    "tcp listener not enabled for remote address, using ephemeral port",
                );
            }
        }

        let future = async move {
            match socket.connect(&remote_address.into()) {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(libc::EINPROGRESS) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(Error::Other(error.to_string())),
            }

            let stream = TcpStream::try_from(Into::<std::net::TcpStream>::into(socket))
                .map_err(|error| Error::Other(error.to_string()))?;
            stream.writable().await.map_err(|error| Error::Other(error.to_string()))?;

            if let Some(error) =
                stream.take_error().map_err(|error| Error::Other(error.to_string()))?
            {
                return Err(Error::Other(error.to_string()));
            }

            Ok((
                address,
                tokio_tungstenite::client_async_tls(url, stream).await?.0,
            ))
        };

        match tokio::time::timeout(connection_open_timeout, future).await {
            Err(_) => Err(Error::Timeout),
            Ok(Err(error)) => Err(error),
            Ok(Ok((address, stream))) => Ok((address, stream)),
        }
    }
}

impl TransportBuilder for WebSocketTransport {
    type Config = Config;
    type Transport = WebSocketTransport;

    /// Create new [`Transport`] object.
    fn new(
        context: TransportHandle,
        mut config: Self::Config,
    ) -> crate::Result<(Self, Vec<Multiaddr>)>
    where
        Self: Sized,
    {
        tracing::debug!(
            target: LOG_TARGET,
            listen_addresses = ?config.listen_addresses,
            "start websocket transport",
        );
        let (listener, listen_addresses, dial_addresses) = SocketListener::new::<WebSocketAddress>(
            std::mem::take(&mut config.listen_addresses),
            config.reuse_port,
            config.nodelay,
        );

        Ok((
            Self {
                listener,
                config,
                context,
                dial_addresses,
                canceled: HashSet::new(),
                opened_raw: HashMap::new(),
                pending_open: HashMap::new(),
                pending_dials: HashMap::new(),
                pending_connections: FuturesUnordered::new(),
                pending_raw_connections: FuturesUnordered::new(),
            },
            listen_addresses,
        ))
    }
}

impl Transport for WebSocketTransport {
    fn dial(&mut self, connection_id: ConnectionId, address: Multiaddr) -> crate::Result<()> {
        let yamux_config = self.config.yamux_config.clone();
        let keypair = self.context.keypair.clone();
        let (ws_address, peer) = Self::multiaddr_into_url(address.clone())?;
        let connection_open_timeout = self.config.connection_open_timeout;
        let max_read_ahead_factor = self.config.noise_read_ahead_frame_count;
        let max_write_buffer_size = self.config.noise_write_buffer_size;
        let dial_addresses = self.dial_addresses.clone();
        let nodelay = self.config.nodelay;

        self.pending_dials.insert(connection_id, address.clone());

        tracing::debug!(target: LOG_TARGET, ?connection_id, ?address, "open connection");

        let future = async move {
            let (_, stream) = WebSocketTransport::dial_peer(
                address.clone(),
                dial_addresses,
                connection_open_timeout,
                nodelay,
            )
            .await
            .map_err(|error| WebSocketError::new(error, Some(connection_id)))?;

            WebSocketConnection::open_connection(
                connection_id,
                keypair,
                stream,
                address,
                peer,
                ws_address,
                yamux_config,
                max_read_ahead_factor,
                max_write_buffer_size,
            )
            .await
            .map_err(|error| WebSocketError::new(error, Some(connection_id)))
        };

        self.pending_connections.push(Box::pin(async move {
            match tokio::time::timeout(connection_open_timeout, future).await {
                Err(_) => Err(WebSocketError::new(Error::Timeout, Some(connection_id))),
                Ok(Err(error)) => Err(error),
                Ok(Ok(result)) => Ok(result),
            }
        }));

        Ok(())
    }

    fn accept(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        let context = self
            .pending_open
            .remove(&connection_id)
            .ok_or(Error::ConnectionDoesntExist(connection_id))?;
        let protocol_set = self.context.protocol_set(connection_id);
        let bandwidth_sink = self.context.bandwidth_sink.clone();
        let substream_open_timeout = self.config.substream_open_timeout;

        tracing::trace!(
            target: LOG_TARGET,
            ?connection_id,
            "start connection",
        );

        self.context.executor.run(Box::pin(async move {
            if let Err(error) = WebSocketConnection::new(
                context,
                protocol_set,
                bandwidth_sink,
                substream_open_timeout,
            )
            .start()
            .await
            {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?connection_id,
                    ?error,
                    "connection exited with error",
                );
            }
        }));

        Ok(())
    }

    fn reject(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        self.canceled.insert(connection_id);
        self.pending_open
            .remove(&connection_id)
            .map_or(Err(Error::ConnectionDoesntExist(connection_id)), |_| Ok(()))
    }

    fn open(
        &mut self,
        connection_id: ConnectionId,
        addresses: Vec<Multiaddr>,
    ) -> crate::Result<()> {
        let mut futures: FuturesUnordered<_> = addresses
            .into_iter()
            .map(|address| {
                let connection_open_timeout = self.config.connection_open_timeout;
                let dial_addresses = self.dial_addresses.clone();
                let nodelay = self.config.nodelay;

                async move {
                    WebSocketTransport::dial_peer(
                        address,
                        dial_addresses,
                        connection_open_timeout,
                        nodelay,
                    )
                    .await
                }
            })
            .collect();

        self.pending_raw_connections.push(Box::pin(async move {
            while let Some(result) = futures.next().await {
                match result {
                    Ok((address, stream)) => return Ok((connection_id, address, stream)),
                    Err(error) => tracing::debug!(
                        target: LOG_TARGET,
                        ?connection_id,
                        ?error,
                        "failed to open connection",
                    ),
                }
            }

            Err(connection_id)
        }));

        Ok(())
    }

    fn negotiate(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        let (stream, address) = self
            .opened_raw
            .remove(&connection_id)
            .ok_or(Error::ConnectionDoesntExist(connection_id))?;

        let peer = match address.iter().find(|protocol| std::matches!(protocol, Protocol::P2p(_))) {
            Some(Protocol::P2p(multihash)) => PeerId::from_multihash(multihash)?,
            _ => return Err(Error::InvalidState),
        };
        let yamux_config = self.config.yamux_config.clone();
        let max_read_ahead_factor = self.config.noise_read_ahead_frame_count;
        let max_write_buffer_size = self.config.noise_write_buffer_size;
        let connection_open_timeout = self.config.connection_open_timeout;
        let keypair = self.context.keypair.clone();

        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?connection_id,
            ?address,
            "negotiate connection",
        );

        self.pending_dials.insert(connection_id, address.clone());
        self.pending_connections.push(Box::pin(async move {
            match tokio::time::timeout(connection_open_timeout, async move {
                WebSocketConnection::negotiate_connection(
                    stream,
                    Some(peer),
                    Role::Dialer,
                    address,
                    connection_id,
                    keypair,
                    yamux_config,
                    max_read_ahead_factor,
                    max_write_buffer_size,
                )
                .await
                .map_err(|error| WebSocketError::new(error, Some(connection_id)))
            })
            .await
            {
                Err(_) => Err(WebSocketError::new(Error::Timeout, Some(connection_id))),
                Ok(Err(error)) => Err(error),
                Ok(Ok(connection)) => Ok(connection),
            }
        }));

        Ok(())
    }

    fn cancel(&mut self, connection_id: ConnectionId) {
        self.canceled.insert(connection_id);
    }
}

impl Stream for WebSocketTransport {
    type Item = TransportEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while let Poll::Ready(Some(connection)) = self.listener.poll_next_unpin(cx) {
            match connection {
                Err(_) => return Poll::Ready(None),
                Ok((stream, address)) => {
                    let connection_id = self.context.next_connection_id();
                    let keypair = self.context.keypair.clone();
                    let yamux_config = self.config.yamux_config.clone();
                    let connection_open_timeout = self.config.connection_open_timeout;
                    let max_read_ahead_factor = self.config.noise_read_ahead_frame_count;
                    let max_write_buffer_size = self.config.noise_write_buffer_size;
                    let address = Multiaddr::empty()
                        .with(Protocol::from(address.ip()))
                        .with(Protocol::Tcp(address.port()))
                        .with(Protocol::Ws(std::borrow::Cow::Owned("/".to_string())));

                    self.pending_connections.push(Box::pin(async move {
                        match tokio::time::timeout(connection_open_timeout, async move {
                            WebSocketConnection::accept_connection(
                                stream,
                                connection_id,
                                keypair,
                                address,
                                yamux_config,
                                max_read_ahead_factor,
                                max_write_buffer_size,
                            )
                            .await
                            .map_err(|error| WebSocketError::new(error, None))
                        })
                        .await
                        {
                            Err(_) => Err(WebSocketError::new(Error::Timeout, None)),
                            Ok(Err(error)) => Err(error),
                            Ok(Ok(result)) => Ok(result),
                        }
                    }));
                }
            }
        }

        while let Poll::Ready(Some(result)) = self.pending_raw_connections.poll_next_unpin(cx) {
            match result {
                Ok((connection_id, address, stream)) => {
                    tracing::trace!(
                        target: LOG_TARGET,
                        ?connection_id,
                        ?address,
                        canceled = self.canceled.contains(&connection_id),
                        "connection opened",
                    );

                    if !self.canceled.remove(&connection_id) {
                        self.opened_raw.insert(connection_id, (stream, address.clone()));

                        return Poll::Ready(Some(TransportEvent::ConnectionOpened {
                            connection_id,
                            address,
                        }));
                    }
                }
                Err(connection_id) =>
                    if !self.canceled.remove(&connection_id) {
                        return Poll::Ready(Some(TransportEvent::OpenFailure { connection_id }));
                    },
            }
        }

        while let Poll::Ready(Some(connection)) = self.pending_connections.poll_next_unpin(cx) {
            match connection {
                Ok(connection) => {
                    let peer = connection.peer();
                    let endpoint = connection.endpoint();
                    self.pending_open.insert(connection.connection_id(), connection);

                    return Poll::Ready(Some(TransportEvent::ConnectionEstablished {
                        peer,
                        endpoint,
                    }));
                }
                Err(error) => match error.connection_id {
                    Some(connection_id) => match self.pending_dials.remove(&connection_id) {
                        Some(address) =>
                            return Poll::Ready(Some(TransportEvent::DialFailure {
                                connection_id,
                                address,
                                error: error.error,
                            })),
                        None => {
                            tracing::debug!(target: LOG_TARGET, ?error, "failed to establish connection")
                        }
                    },
                    None => {
                        tracing::debug!(target: LOG_TARGET, ?error, "failed to establish connection")
                    }
                },
            }
        }

        Poll::Pending
    }
}
