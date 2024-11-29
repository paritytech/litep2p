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
    error::{AddressError, Error, NegotiationError},
    metrics::{MetricGauge, ScopeGaugeMetric},
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
    utils::futures_stream::FuturesStream,
    DialError, PeerId,
};

use connection::WebSocketConnectionMetrics;
use futures::{
    future::BoxFuture,
    stream::{AbortHandle, FuturesUnordered},
    Stream, StreamExt, TryFutureExt,
};
use multiaddr::{Multiaddr, Protocol};
use socket2::{Domain, Socket, Type};
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use url::Url;

use std::{
    collections::HashMap,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

pub(crate) use substream::Substream;

mod connection;
mod stream;
mod substream;

pub mod config;

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::websocket";

/// Pending inbound connection.
struct PendingInboundConnection {
    /// Socket address of the remote peer.
    connection: TcpStream,
    /// Address of the remote peer.
    address: SocketAddr,
}

#[derive(Debug)]
enum RawConnectionResult {
    /// The first successful connection.
    Connected {
        connection_id: ConnectionId,
        address: Multiaddr,
        stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    },

    /// All connection attempts failed.
    Failed {
        connection_id: ConnectionId,
        errors: Vec<(Multiaddr, DialError)>,
    },

    /// Future was canceled.
    Canceled { connection_id: ConnectionId },
}

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

    /// Pending inbound connections.
    pending_inbound_connections: HashMap<ConnectionId, PendingInboundConnection>,

    /// Pending connections.
    pending_connections:
        FuturesStream<BoxFuture<'static, Result<NegotiatedConnection, (ConnectionId, DialError)>>>,

    /// Pending raw, unnegotiated connections.
    pending_raw_connections: FuturesStream<BoxFuture<'static, RawConnectionResult>>,

    /// Opened raw connection, waiting for approval/rejection from `TransportManager`.
    opened_raw: HashMap<ConnectionId, (WebSocketStream<MaybeTlsStream<TcpStream>>, Multiaddr)>,

    /// Cancel raw connections futures.
    ///
    /// This is cancelling `Self::pending_raw_connections`.
    cancel_futures: HashMap<ConnectionId, AbortHandle>,

    /// Negotiated connections waiting validation.
    pending_open: HashMap<ConnectionId, NegotiatedConnection>,

    /// Websocket metrics.
    metrics: Option<WebSocketMetrics>,
}

/// Websocket specific metrics.
struct WebSocketMetrics {
    /// Interval for collecting metrics.
    ///
    /// This is a tradeoff we make in favor of simplicity and correctness.
    /// An alternative to this would be to complicate the code by collecting
    /// individual metrics in each method. This is error prone, as names are
    /// easily mismatched, and it's hard to keep track of all the metrics.
    interval: tokio::time::Interval,

    /// The following metrics are used for the transport itself.
    pending_dials_num: MetricGauge,
    pending_inbound_connections_num: MetricGauge,
    pending_connections_num: MetricGauge,
    pending_raw_connections_num: MetricGauge,
    open_raw_connections_num: MetricGauge,
    cancel_futures_num: MetricGauge,
    pending_open_num: MetricGauge,

    /// The following metrics are shared with all TCP connections.
    active_connections_num: MetricGauge,
    pending_substreams_num: MetricGauge,
}

impl WebSocketMetrics {
    fn to_connection_metrics(&self) -> WebSocketConnectionMetrics {
        WebSocketConnectionMetrics {
            _active_connections_num: ScopeGaugeMetric::new(self.active_connections_num.clone()),
            pending_substreams_num: self.pending_substreams_num.clone(),
        }
    }
}

impl WebSocketTransport {
    /// Handle inbound connection.
    fn on_inbound_connection(
        &mut self,
        connection_id: ConnectionId,
        connection: TcpStream,
        address: SocketAddr,
    ) {
        let keypair = self.context.keypair.clone();
        let yamux_config = self.config.yamux_config.clone();
        let connection_open_timeout = self.config.connection_open_timeout;
        let max_read_ahead_factor = self.config.noise_read_ahead_frame_count;
        let max_write_buffer_size = self.config.noise_write_buffer_size;
        let address = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::Ws(std::borrow::Cow::Borrowed("/")));

        self.pending_connections.push(Box::pin(async move {
            match tokio::time::timeout(connection_open_timeout, async move {
                WebSocketConnection::accept_connection(
                    connection,
                    connection_id,
                    keypair,
                    address,
                    yamux_config,
                    max_read_ahead_factor,
                    max_write_buffer_size,
                )
                .await
                .map_err(|error| (connection_id, error.into()))
            })
            .await
            {
                Err(_) => Err((connection_id, DialError::Timeout)),
                Ok(Err(error)) => Err(error),
                Ok(Ok(result)) => Ok(result),
            }
        }));
    }

    /// Convert `Multiaddr` into `url::Url`
    fn multiaddr_into_url(address: Multiaddr) -> Result<(Url, PeerId), AddressError> {
        let mut protocol_stack = address.iter();

        let dial_address = match protocol_stack.next().ok_or(AddressError::InvalidProtocol)? {
            Protocol::Ip4(address) => address.to_string(),
            Protocol::Ip6(address) => format!("[{address}]"),
            Protocol::Dns(address) | Protocol::Dns4(address) | Protocol::Dns6(address) =>
                address.to_string(),

            _ => return Err(AddressError::InvalidProtocol),
        };

        let url = match protocol_stack.next().ok_or(AddressError::InvalidProtocol)? {
            Protocol::Tcp(port) => match protocol_stack.next() {
                Some(Protocol::Ws(_)) => format!("ws://{dial_address}:{port}/"),
                Some(Protocol::Wss(_)) => format!("wss://{dial_address}:{port}/"),
                _ => return Err(AddressError::InvalidProtocol),
            },
            _ => return Err(AddressError::InvalidProtocol),
        };

        let peer = match protocol_stack.next() {
            Some(Protocol::P2p(multihash)) => PeerId::from_multihash(multihash)?,
            protocol => {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `Protocol::Ws`/`Protocol::Wss`",
                );
                return Err(AddressError::PeerIdMissing);
            }
        };

        tracing::trace!(target: LOG_TARGET, ?url, "parse address");

        url::Url::parse(&url)
            .map(|url| (url, peer))
            .map_err(|_| AddressError::InvalidUrl)
    }

    /// Dial remote peer over `address`.
    async fn dial_peer(
        address: Multiaddr,
        dial_addresses: DialAddresses,
        connection_open_timeout: Duration,
        nodelay: bool,
    ) -> Result<(Multiaddr, WebSocketStream<MaybeTlsStream<TcpStream>>), DialError> {
        let (url, _) = Self::multiaddr_into_url(address.clone())?;

        let (socket_address, _) = WebSocketAddress::multiaddr_to_socket_address(&address)?;
        let remote_address =
            match tokio::time::timeout(connection_open_timeout, socket_address.lookup_ip()).await {
                Err(_) => return Err(DialError::Timeout),
                Ok(Err(error)) => return Err(error.into()),
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
                Err(err) => return Err(DialError::from(err)),
            }

            let stream = TcpStream::try_from(Into::<std::net::TcpStream>::into(socket))?;
            stream.writable().await?;
            if let Some(e) = stream.take_error()? {
                return Err(DialError::from(e));
            }

            Ok((
                address,
                tokio_tungstenite::client_async_tls(url, stream)
                    .await
                    .map_err(NegotiationError::WebSocket)?
                    .0,
            ))
        };

        match tokio::time::timeout(connection_open_timeout, future).await {
            Err(_) => Err(DialError::Timeout),
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
        registry: Option<crate::metrics::MetricsRegistry>,
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

        let metrics = if let Some(registry) = registry {
            Some(WebSocketMetrics {
                interval: tokio::time::interval(Duration::from_secs(15)),

                pending_dials_num: registry.register_gauge(
                    "litep2p_websocket_pending_dials".into(),
                    "Litep2p number of pending dials".into(),
                )?,
                pending_inbound_connections_num: registry.register_gauge(
                    "litep2p_websocket_pending_inbound_connections".into(),
                    "Litep2p number of pending inbound connections".into(),
                )?,
                pending_connections_num: registry.register_gauge(
                    "litep2p_websocket_pending_connections".into(),
                    "Litep2p number of pending connections".into(),
                )?,
                pending_raw_connections_num: registry.register_gauge(
                    "litep2p_websocket_pending_raw_connections".into(),
                    "Litep2p number of pending raw connections".into(),
                )?,
                open_raw_connections_num: registry.register_gauge(
                    "litep2p_websocket_open_raw_connections".into(),
                    "Litep2p number of open raw connections".into(),
                )?,
                cancel_futures_num: registry.register_gauge(
                    "litep2p_websocket_cancel_futures".into(),
                    "Litep2p number of cancel futures".into(),
                )?,
                pending_open_num: registry.register_gauge(
                    "litep2p_websocket_pending_open".into(),
                    "Litep2p number of pending open connections".into(),
                )?,

                active_connections_num: registry.register_gauge(
                    "litep2p_websocket_active_connections".into(),
                    "Litep2p number of active connections".into(),
                )?,

                pending_substreams_num: registry.register_gauge(
                    "litep2p_websocket_pending_substreams".into(),
                    "Litep2p number of pending substreams".into(),
                )?,
            })
        } else {
            None
        };

        Ok((
            Self {
                listener,
                config,
                context,
                dial_addresses,
                opened_raw: HashMap::new(),
                pending_open: HashMap::new(),
                pending_dials: HashMap::new(),
                pending_inbound_connections: HashMap::new(),
                pending_connections: FuturesStream::new(),
                pending_raw_connections: FuturesStream::new(),
                cancel_futures: HashMap::new(),
                metrics,
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
            .map_err(|error| (connection_id, error))?;

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
            .map_err(|error| (connection_id, error.into()))
        };

        self.pending_connections.push(Box::pin(async move {
            match tokio::time::timeout(connection_open_timeout, future).await {
                Err(_) => Err((connection_id, DialError::Timeout)),
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

        let metrics = self.metrics.as_ref().map(|metrics| metrics.to_connection_metrics());
        self.context.executor.run(Box::pin(async move {
            if let Err(error) = WebSocketConnection::new(
                context,
                protocol_set,
                bandwidth_sink,
                substream_open_timeout,
                metrics,
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
        self.pending_open
            .remove(&connection_id)
            .map_or(Err(Error::ConnectionDoesntExist(connection_id)), |_| Ok(()))
    }

    fn accept_pending(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        let pending = self.pending_inbound_connections.remove(&connection_id).ok_or_else(|| {
            tracing::error!(
                target: LOG_TARGET,
                ?connection_id,
                "Cannot accept non existent pending connection",
            );

            Error::ConnectionDoesntExist(connection_id)
        })?;

        self.on_inbound_connection(connection_id, pending.connection, pending.address);

        Ok(())
    }

    fn reject_pending(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        self.pending_inbound_connections.remove(&connection_id).map_or_else(
            || {
                tracing::error!(
                    target: LOG_TARGET,
                    ?connection_id,
                    "Cannot reject non existent pending connection",
                );

                Err(Error::ConnectionDoesntExist(connection_id))
            },
            |_| Ok(()),
        )
    }

    fn open(
        &mut self,
        connection_id: ConnectionId,
        addresses: Vec<Multiaddr>,
    ) -> crate::Result<()> {
        let num_addresses = addresses.len();
        let mut futures: FuturesUnordered<_> = addresses
            .into_iter()
            .map(|address| {
                let connection_open_timeout = self.config.connection_open_timeout;
                let dial_addresses = self.dial_addresses.clone();
                let nodelay = self.config.nodelay;

                async move {
                    WebSocketTransport::dial_peer(
                        address.clone(),
                        dial_addresses,
                        connection_open_timeout,
                        nodelay,
                    )
                    .await
                    .map_err(|error| (address, error))
                }
            })
            .collect();

        // Future that will resolve to the first successful connection.
        let future = async move {
            let mut errors = Vec::with_capacity(num_addresses);
            while let Some(result) = futures.next().await {
                match result {
                    Ok((address, stream)) =>
                        return RawConnectionResult::Connected {
                            connection_id,
                            address,
                            stream,
                        },
                    Err(error) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?connection_id,
                            ?error,
                            "failed to open connection",
                        );
                        errors.push(error)
                    }
                }
            }

            RawConnectionResult::Failed {
                connection_id,
                errors,
            }
        };

        let (fut, handle) = futures::future::abortable(future);
        let fut = fut.unwrap_or_else(move |_| RawConnectionResult::Canceled { connection_id });
        self.pending_raw_connections.push(Box::pin(fut));
        self.cancel_futures.insert(connection_id, handle);

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
                .map_err(|error| (connection_id, error.into()))
            })
            .await
            {
                Err(_) => Err((connection_id, DialError::Timeout)),
                Ok(Err(error)) => Err(error),
                Ok(Ok(connection)) => Ok(connection),
            }
        }));

        Ok(())
    }

    fn cancel(&mut self, connection_id: ConnectionId) {
        // Cancel the future if it exists.
        // State clean-up happens inside the `poll_next`.
        if let Some(handle) = self.cancel_futures.get(&connection_id) {
            handle.abort();
        }
    }
}

impl Stream for WebSocketTransport {
    type Item = TransportEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Take the metrics to only poll the tick in case they are enabled.
        if let Some(mut metrics) = self.metrics.take() {
            if let Poll::Ready(_) = metrics.interval.poll_tick(cx) {
                metrics.pending_dials_num.set(self.pending_dials.len() as u64);
                metrics
                    .pending_inbound_connections_num
                    .set(self.pending_inbound_connections.len() as u64);
                metrics.pending_connections_num.set(self.pending_connections.len() as u64);
                metrics
                    .pending_raw_connections_num
                    .set(self.pending_raw_connections.len() as u64);
                metrics.open_raw_connections_num.set(self.opened_raw.len() as u64);
                metrics.cancel_futures_num.set(self.cancel_futures.len() as u64);
                metrics.pending_open_num.set(self.pending_open.len() as u64);
            }
        }

        if let Poll::Ready(event) = self.listener.poll_next_unpin(cx) {
            return match event {
                None => {
                    tracing::error!(
                        target: LOG_TARGET,
                        "Websocket listener terminated, ignore if the node is stopping",
                    );

                    Poll::Ready(None)
                }
                Some(Err(error)) => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?error,
                        "Websocket listener terminated with error",
                    );

                    Poll::Ready(None)
                }
                Some(Ok((connection, address))) => {
                    let connection_id = self.context.next_connection_id();
                    tracing::trace!(
                        target: LOG_TARGET,
                        ?connection_id,
                        ?address,
                        "pending inbound Websocket connection",
                    );

                    self.pending_inbound_connections.insert(
                        connection_id,
                        PendingInboundConnection {
                            connection,
                            address,
                        },
                    );

                    Poll::Ready(Some(TransportEvent::PendingInboundConnection {
                        connection_id,
                    }))
                }
            };
        }

        while let Poll::Ready(Some(result)) = self.pending_raw_connections.poll_next_unpin(cx) {
            tracing::trace!(target: LOG_TARGET, ?result, "raw connection result");

            match result {
                RawConnectionResult::Connected {
                    connection_id,
                    address,
                    stream,
                } => {
                    let Some(handle) = self.cancel_futures.remove(&connection_id) else {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?connection_id,
                            ?address,
                            "raw connection without a cancel handle",
                        );
                        continue;
                    };

                    if !handle.is_aborted() {
                        self.opened_raw.insert(connection_id, (stream, address.clone()));

                        return Poll::Ready(Some(TransportEvent::ConnectionOpened {
                            connection_id,
                            address,
                        }));
                    }
                }

                RawConnectionResult::Failed {
                    connection_id,
                    errors,
                } => {
                    let Some(handle) = self.cancel_futures.remove(&connection_id) else {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?connection_id,
                            ?errors,
                            "raw connection without a cancel handle",
                        );
                        continue;
                    };

                    if !handle.is_aborted() {
                        return Poll::Ready(Some(TransportEvent::OpenFailure {
                            connection_id,
                            errors,
                        }));
                    }
                }
                RawConnectionResult::Canceled { connection_id } => {
                    if self.cancel_futures.remove(&connection_id).is_none() {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?connection_id,
                            "raw cancelled connection without a cancel handle",
                        );
                    }
                }
            }
        }

        while let Poll::Ready(Some(connection)) = self.pending_connections.poll_next_unpin(cx) {
            match connection {
                Ok(connection) => {
                    let peer = connection.peer();
                    let endpoint = connection.endpoint();
                    self.pending_dials.remove(&connection.connection_id());
                    self.pending_open.insert(connection.connection_id(), connection);

                    return Poll::Ready(Some(TransportEvent::ConnectionEstablished {
                        peer,
                        endpoint,
                    }));
                }
                Err((connection_id, error)) => {
                    if let Some(address) = self.pending_dials.remove(&connection_id) {
                        return Poll::Ready(Some(TransportEvent::DialFailure {
                            connection_id,
                            address,
                            error,
                        }));
                    } else {
                        tracing::debug!(target: LOG_TARGET, ?error, ?connection_id, "Pending inbound connection failed");
                    }
                }
            }
        }

        Poll::Pending
    }
}
