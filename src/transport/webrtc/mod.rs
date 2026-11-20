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

//! WebRTC transport.

use crate::{
    error::Error,
    transport::{
        common::{opening_deadline, InboundOpening},
        manager::TransportHandle,
        webrtc::{
            config::Config,
            connection::WebRtcConnection,
            listener::WebRtcListener,
            opening::OpeningWebRtcConnection,
            rtc::{guard, Rtc},
            socket::WebRtcSocket,
        },
        Endpoint, Transport, TransportBuilder, TransportEvent,
    },
    types::ConnectionId,
    PeerId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, Future, Stream, StreamExt};
use futures_timer::Delay;
use hickory_resolver::TokioResolver;
use multiaddr::Multiaddr;
use str0m::{
    channel::{ChannelConfig, ChannelId},
    config::DtlsCert,
    ice::IceCreds,
    net::{DatagramRecv, Protocol as Str0mProtocol},
    Candidate, RtcError,
};

use tokio::sync::mpsc::{channel, error::TrySendError, Sender};

use std::{
    collections::{hash_map::Entry, HashMap, VecDeque},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

pub(crate) use substream::Substream;

mod certificate;
mod connection;
mod listener;
mod opening;
mod rtc;
mod socket;

// `substream` and `util` (the substream state machine and wire codec) are the primary fuzz
// targets; the `fuzz` feature makes them reachable from the harnesses under `fuzz/`.
#[cfg(not(feature = "fuzz"))]
mod substream;
#[cfg(feature = "fuzz")]
pub mod substream;

#[cfg(not(feature = "fuzz"))]
mod util;
#[cfg(feature = "fuzz")]
pub mod util;

pub mod config;
pub use certificate::DtlsCertificate;

// Scaffolding that lets `fuzz/webrtc-state` drive the channel state machine. Re-exported
// rather than making `connection` public, so only the harness surface is widened.
#[cfg(feature = "fuzz")]
pub use connection::fuzz::FuzzConnection;

// Generated `webrtc::Message` / `Flag`: part of the fuzzable codec surface, so public under `fuzz`.
#[cfg(not(feature = "fuzz"))]
pub(super) mod schema {
    pub(super) mod webrtc {
        include!(concat!(env!("OUT_DIR"), "/webrtc.rs"));
    }

    pub(super) mod noise {
        include!(concat!(env!("OUT_DIR"), "/noise.rs"));
    }
}

#[cfg(feature = "fuzz")]
pub mod schema {
    pub mod webrtc {
        include!(concat!(env!("OUT_DIR"), "/webrtc.rs"));
    }

    pub mod noise {
        include!(concat!(env!("OUT_DIR"), "/noise.rs"));
    }
}

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::webrtc";

/// Hardcoded remote fingerprint.
const REMOTE_FINGERPRINT: &str =
    "sha-256 FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF";

/// A maximum single UDP payload size that we forward to `str0m`.
const MAX_UDP_PAYLOAD_SIZE: usize = 2000;

/// Bound receive work so that timers run under sustained traffic.
const MAX_SOCKET_READS_PER_POLL: usize = 64;

/// Connection context.
struct ConnectionContext {
    /// Remote peer ID.
    peer: PeerId,

    /// Connection ID.
    connection_id: ConnectionId,

    /// TX channel for sending datagrams to the connection event loop.
    tx: Sender<Vec<u8>>,
}

/// Events received from opening connections that are handled
/// by the [`WebRtcTransport`] event loop.
enum ConnectionEvent {
    /// Connection established.
    ConnectionEstablished {
        /// Remote peer ID.
        peer: PeerId,

        /// Endpoint.
        endpoint: Endpoint,
    },

    /// Connection to peer closed.
    ConnectionClosed,

    /// Timeout.
    Timeout {
        /// Absolute timeout.
        timeout: Instant,
    },
}

/// Endpoints of a received UDP datagram: its local destination address,
/// and the remote socket that sent it.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
struct AddressPair {
    /// Local destination address of the datagram;
    /// a concrete IP even for wildcard listening sockets.
    local: SocketAddr,
    /// Address of the remote peer that sent the datagram.
    remote: SocketAddr,
}

/// WebRTC transport.
pub(crate) struct WebRtcTransport {
    /// Transport context.
    context: TransportHandle,

    /// DTLS certificate.
    dtls_cert: DtlsCert,

    /// WebRtc Listener.
    listener: WebRtcListener,

    /// Datagram buffer size.
    datagram_buffer_size: usize,

    /// Absolute connection opening timeout.
    connection_open_timeout: Duration,

    /// Connected peers.
    open: HashMap<AddressPair, ConnectionContext>,

    /// Watches for closed connections.
    closed_connections: FuturesUnordered<BoxFuture<'static, (AddressPair, ConnectionId)>>,

    /// OpeningWebRtc connections.
    opening: HashMap<AddressPair, OpeningWebRtcConnection>,

    /// `ConnectionId -> (peer id, address_pair, endpoint)` mappings.
    connections: HashMap<ConnectionId, (PeerId, AddressPair, Endpoint)>,

    /// Pending timeouts.
    timeouts: HashMap<AddressPair, Delay>,

    /// Pending events.
    pending_events: VecDeque<TransportEvent>,

    /// Read buffer.
    read_buffer: Vec<u8>,
}

impl WebRtcTransport {
    /// Create [`Rtc`] object for a new session and open a channel for Noise handshake.
    fn make_rtc(
        &self,
        ufrag: &str,
        pass: &str,
        addrs: &AddressPair,
    ) -> crate::Result<(Rtc, ChannelId)> {
        let (rtc, noise_channel_id) = guard(|| self.build_rtc(ufrag, pass, addrs))??;

        Ok((Rtc::new(rtc), noise_channel_id))
    }

    /// Unguarded part of [`Self::make_rtc`].
    fn build_rtc(
        &self,
        ufrag: &str,
        pass: &str,
        addrs: &AddressPair,
    ) -> crate::Result<(str0m::Rtc, ChannelId)> {
        let mut rtc = str0m::Rtc::builder()
            .set_ice_lite(true)
            .set_dtls_cert(self.dtls_cert.clone())
            .set_fingerprint_verification(false)
            .build(std::time::Instant::now());
        rtc.add_local_candidate(
            Candidate::host(addrs.local, Str0mProtocol::Udp).map_err(RtcError::Ice)?,
        );
        rtc.add_remote_candidate(
            Candidate::host(addrs.remote, Str0mProtocol::Udp).map_err(RtcError::Ice)?,
        );
        rtc.direct_api().set_remote_fingerprint(
            REMOTE_FINGERPRINT
                .parse()
                .map_err(|_| Error::Other("remote fingerprint generation failed".to_string()))?,
        );
        rtc.direct_api().set_remote_ice_credentials(IceCreds {
            ufrag: ufrag.to_owned(),
            pass: pass.to_owned(),
        });
        rtc.direct_api().set_local_ice_credentials(IceCreds {
            ufrag: ufrag.to_owned(),
            pass: pass.to_owned(),
        });
        rtc.direct_api().set_ice_controlling(false);
        rtc.direct_api().start_dtls(false)?;
        rtc.direct_api().start_sctp(false);

        let noise_channel_id = rtc.direct_api().create_data_channel(ChannelConfig {
            label: "noise".to_string(),
            ordered: true,
            reliability: str0m::channel::Reliability::Reliable,
            negotiated: Some(0),
            protocol: "".to_string(),
        });

        Ok((rtc, noise_channel_id))
    }

    /// Poll opening connection.
    fn poll_connection(&mut self, addrs: &AddressPair) -> ConnectionEvent {
        let Some(connection) = self.opening.get_mut(addrs) else {
            tracing::warn!(
                target: LOG_TARGET,
                ?addrs,
                "connection doesn't exist",
            );
            return ConnectionEvent::ConnectionClosed;
        };

        loop {
            match connection.poll_process() {
                opening::WebRtcEvent::Timeout { timeout } => {
                    if timeout > Instant::now() {
                        return ConnectionEvent::Timeout { timeout };
                    }

                    if let Err(error) = connection.on_timeout() {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?addrs,
                            ?error,
                            "failed to handle timeout",
                        );

                        return ConnectionEvent::ConnectionClosed;
                    }
                }
                opening::WebRtcEvent::Transmit {
                    destination,
                    datagram,
                } =>
                    if let Err(error) =
                        connection.socket().try_send_to(&datagram, destination, addrs.local.ip())
                    {
                        if error.kind() == std::io::ErrorKind::WouldBlock {
                            tracing::trace!(
                                target: LOG_TARGET,
                                ?addrs,
                                "UDP send buffer full, dropping datagram (str0m will retransmit)",
                            );
                        } else {
                            tracing::warn!(
                                target: LOG_TARGET,
                                ?addrs,
                                ?error,
                                "failed to send datagram",
                            );
                        }
                    },
                opening::WebRtcEvent::ConnectionClosed => return ConnectionEvent::ConnectionClosed,
                opening::WebRtcEvent::ConnectionOpened { peer, endpoint } => {
                    return ConnectionEvent::ConnectionEstablished { peer, endpoint };
                }
            }
        }
    }

    /// Remove all state for an opening connection.
    fn remove_opening(&mut self, addrs: &AddressPair) -> Option<OpeningWebRtcConnection> {
        self.timeouts.remove(addrs);
        let connection = self.opening.remove(addrs)?;
        let connection_id = connection.connection_id();
        self.connections.remove(connection_id);
        self.pending_events.retain(|event| {
            !matches!(event, TransportEvent::ConnectionEstablished { endpoint, .. }
                if endpoint.connection_id() == *connection_id)
        });
        Some(connection)
    }

    /// Pop the next event, dropping establishments of expired openings.
    ///
    /// Other expired openings are removed by their timers, capped to the deadline.
    fn pop_event(&mut self) -> Option<TransportEvent> {
        while let Some(event) = self.pending_events.pop_front() {
            if let TransportEvent::ConnectionEstablished { endpoint, .. } = &event {
                let expired = self
                    .connections
                    .get(&endpoint.connection_id())
                    .map(|(_, addrs, _)| *addrs)
                    .filter(|addrs| self.opening.get(addrs).is_some_and(|c| c.is_expired()));

                if let Some(addrs) = expired {
                    self.remove_opening(&addrs);
                    continue;
                }
            }

            return Some(event);
        }

        None
    }

    /// Drain opening output and register its next wakeup.
    fn process_connection(&mut self, addrs: &AddressPair, cx: &mut Context<'_>) {
        loop {
            match self.poll_connection(addrs) {
                ConnectionEvent::ConnectionEstablished { peer, endpoint } => {
                    self.connections
                        .insert(endpoint.connection_id(), (peer, *addrs, endpoint.clone()));
                    self.pending_events
                        .push_back(TransportEvent::ConnectionEstablished { peer, endpoint });
                }
                ConnectionEvent::ConnectionClosed => {
                    self.remove_opening(addrs);
                    return;
                }
                ConnectionEvent::Timeout { timeout } => {
                    let mut delay = Delay::new(timeout.saturating_duration_since(Instant::now()));
                    if Pin::new(&mut delay).poll(cx).is_ready() {
                        cx.waker().wake_by_ref();
                    }
                    self.timeouts.insert(*addrs, delay);
                    return;
                }
            }
        }
    }

    /// Handle socket input.
    ///
    /// If the datagram was received from an active client, it's dispatched to the connection
    /// handler, if there is space in the queue. If the datagram opened a new connection or it
    /// belonged to a client who is opening, the event loop is instructed to poll the client
    /// until it timeouts.
    ///
    /// Returns `true` if the client should be polled.
    fn on_socket_input(
        &mut self,
        addrs: AddressPair,
        socket: &Arc<WebRtcSocket>,
        buffer: Vec<u8>,
    ) -> crate::Result<bool> {
        // Drop anything that is not STUN or DTLS before it reaches str0m.
        // No media is ever negotiated, so RTP/RTCP is always rejected.
        //
        // Drop silently rather than erroring,
        // an invalid datagram must not tear down a live peer.
        if !is_forwardable(&buffer) {
            tracing::trace!(
                target: LOG_TARGET,
                ?addrs,
                first_byte = buffer.first().copied(),
                len = buffer.len(),
                "dropping non-STUN/DTLS datagram",
            );

            return Ok(false);
        }

        if let Entry::Occupied(mut entry) = self.open.entry(addrs) {
            let ConnectionContext {
                peer,
                connection_id,
                tx,
            } = entry.get_mut();

            match tx.try_send(buffer) {
                Ok(_) => return Ok(false),
                Err(TrySendError::Full(_)) => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        ?addrs,
                        ?peer,
                        ?connection_id,
                        "channel full, dropping datagram",
                    );

                    return Ok(false);
                }
                Err(TrySendError::Closed(_)) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?addrs,
                        ?peer,
                        ?connection_id,
                        "connection closed, removing stale entry",
                    );

                    entry.remove();
                    return Ok(false);
                }
            }
        }

        if self.opening.get(&addrs).is_some_and(|connection| connection.is_expired()) {
            self.remove_opening(&addrs);
            return Ok(false);
        }

        // If an opening connection already exists for this source, route all packets to it
        if let Some(opening_conn) = self.opening.get_mut(&addrs) {
            let contents = guard(|| DatagramRecv::try_from(buffer.as_slice()))?
                .map_err(|_| Error::InvalidData)?;

            tracing::trace!(
                target: LOG_TARGET,
                ?addrs,
                is_stun = is_stun_packet(&buffer),
                "routing packet to existing opening connection"
            );

            if let Err(error) = opening_conn.on_input(contents) {
                tracing::error!(
                    target: LOG_TARGET,
                    ?error,
                    ?addrs,
                    "failed to handle inbound datagram"
                );
            }
            return Ok(true);
        }

        // No existing connection - this should be a STUN packet to create a new connection
        if !is_stun_packet(&buffer) {
            tracing::warn!(
                target: LOG_TARGET,
                ?addrs,
                "received non-stun packet without existing connection, ignoring"
            );
            return Ok(false);
        }

        // Reserve capacity before decoding, so that datagrams dropped at capacity stay cheap.
        let Some(permit) = self.context.try_acquire_inbound_permit() else {
            if let Some(dropped) = self.context.dropped_openings.record() {
                tracing::debug!(
                    target: LOG_TARGET,
                    dropped,
                    "inbound connections dropped: `max_pending_incoming_connections` reached",
                );
            }
            return Ok(false);
        };
        let deadline = opening_deadline(Instant::now(), self.connection_open_timeout);

        let contents =
            guard(|| DatagramRecv::try_from(buffer.as_slice()))?.map_err(|_| Error::InvalidData)?;
        let stun_message =
            guard(|| str0m::ice::StunMessage::parse(&buffer))?.map_err(|_| Error::InvalidData)?;
        let Some((ufrag, pass)) = stun_message.split_username() else {
            tracing::warn!(
                target: LOG_TARGET,
                ?addrs,
                "failed to split username/password",
            );
            return Err(Error::InvalidData);
        };

        tracing::debug!(
            target: LOG_TARGET,
            ?addrs,
            ?ufrag,
            "received stun message"
        );

        // create new `Rtc` object for the peer and give it the received STUN message
        let (rtc, noise_channel_id) = self.make_rtc(ufrag, pass, &addrs)?;

        let connection_id = self.context.next_connection_id();
        let mut connection = OpeningWebRtcConnection::new(
            rtc,
            connection_id,
            noise_channel_id,
            self.context.keypair.clone(),
            addrs,
            socket.clone(),
            InboundOpening::new(permit, deadline),
        );
        connection.on_input(contents)?;
        self.opening.insert(addrs, connection);

        Ok(true)
    }
}

impl TransportBuilder for WebRtcTransport {
    type Config = Config;
    type Transport = WebRtcTransport;

    /// Create new [`Transport`] object.
    fn new(
        context: TransportHandle,
        config: Self::Config,
        _resolver: Arc<TokioResolver>,
    ) -> crate::Result<(Self, Vec<Multiaddr>)>
    where
        Self: Sized,
    {
        if config.listen_addresses.is_empty() {
            return Err(Error::Other(
                "WebRTC transport requires at least one listen address but none were configured"
                    .to_string(),
            ));
        }

        tracing::debug!(
            target: LOG_TARGET,
            listen_addresses = ?config.listen_addresses,
            "start webrtc transport",
        );

        let dtls_cert = match config.certificate {
            Some(certificate) => certificate,
            None => {
                tracing::debug!(target: LOG_TARGET, "generating temporary WebRTC certificate");
                DtlsCertificate::new()?
            }
        };

        let (listener, listen_multi_addresses) =
            WebRtcListener::new(config.listen_addresses, dtls_cert.certhash())?;
        let read_buffer = vec![0; listener.max_read_size()];

        Ok((
            Self {
                context,
                dtls_cert: dtls_cert.into(),
                listener,
                open: HashMap::new(),
                closed_connections: FuturesUnordered::new(),
                opening: HashMap::new(),
                connections: HashMap::new(),
                timeouts: HashMap::new(),
                pending_events: VecDeque::new(),
                datagram_buffer_size: config.datagram_buffer_size,
                connection_open_timeout: config.connection_open_timeout,
                read_buffer,
            },
            listen_multi_addresses,
        ))
    }
}

impl Transport for WebRtcTransport {
    fn dial(&mut self, connection_id: ConnectionId, address: Multiaddr) -> crate::Result<()> {
        tracing::warn!(
            target: LOG_TARGET,
            ?connection_id,
            ?address,
            "webrtc cannot dial",
        );

        debug_assert!(false);
        Err(Error::NotSupported("webrtc cannot dial peers".to_string()))
    }

    fn accept_pending(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?connection_id,
            "webrtc cannot accept pending connections",
        );

        debug_assert!(false);
        Err(Error::NotSupported(
            "webrtc cannot accept pending connections".to_string(),
        ))
    }

    fn reject_pending(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?connection_id,
            "webrtc cannot reject pending connections",
        );

        debug_assert!(false);
        Err(Error::NotSupported(
            "webrtc cannot reject pending connections".to_string(),
        ))
    }

    fn accept(
        &mut self,
        connection_id: ConnectionId,
    ) -> crate::Result<BoxFuture<'static, crate::Result<()>>> {
        tracing::trace!(
            target: LOG_TARGET,
            ?connection_id,
            "inbound connection accepted",
        );

        let (peer, addrs, endpoint) = self.connections.remove(&connection_id).ok_or_else(|| {
            tracing::warn!(
                target: LOG_TARGET,
                ?connection_id,
                "pending connection doens't exist",
            );

            Error::InvalidState
        })?;

        let connection = self.remove_opening(&addrs).ok_or_else(|| {
            tracing::warn!(
                target: LOG_TARGET,
                ?connection_id,
                "pending connection doens't exist",
            );

            Error::InvalidState
        })?;

        let socket = connection.socket().clone();
        let rtc = connection.on_accept()?;
        let (tx, rx) = channel(self.datagram_buffer_size);
        let mut protocol_set = self.context.protocol_set(connection_id);
        let connection_id = endpoint.connection_id();
        let endpoint_clone = endpoint.clone();
        let executor = self.context.executor.clone();

        let watcher_tx = tx.clone();
        self.closed_connections.push(Box::pin(async move {
            // Resolves when the `WebRtcConnection` is dropped.
            watcher_tx.closed().await;
            (addrs, connection_id)
        }));

        self.open.insert(
            addrs,
            ConnectionContext {
                tx,
                peer,
                connection_id,
            },
        );

        Ok(Box::pin(async move {
            // First, notify all protocols about the connection establishment
            protocol_set.report_connection_established(peer, endpoint_clone).await;

            // After protocols are notified, create connection and spawn event loop
            let connection =
                WebRtcConnection::new(rtc, peer, addrs, socket, protocol_set, endpoint, rx);

            executor.run(Box::pin(async move {
                connection.run_event_loop().await;
            }));

            Ok(())
        }))
    }

    fn reject(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?connection_id,
            "inbound connection rejected",
        );

        let (_, addrs, _) = self.connections.remove(&connection_id).ok_or_else(|| {
            tracing::warn!(
                target: LOG_TARGET,
                ?connection_id,
                "pending connection doens't exist",
            );

            Error::InvalidState
        })?;

        self.remove_opening(&addrs)
            .ok_or_else(|| {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?connection_id,
                    "pending connection doens't exist",
                );

                Error::InvalidState
            })
            .map(|_| ())
    }

    fn open(
        &mut self,
        _connection_id: ConnectionId,
        _addresses: Vec<Multiaddr>,
    ) -> crate::Result<()> {
        Ok(())
    }

    fn negotiate(&mut self, _connection_id: ConnectionId) -> crate::Result<()> {
        Ok(())
    }

    fn cancel(&mut self, _connection_id: ConnectionId) {}
}

impl Stream for WebRtcTransport {
    type Item = TransportEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);

        if let Some(event) = this.pop_event() {
            return Poll::Ready(Some(event));
        }

        while let Poll::Ready(Some((source, connection_id))) =
            this.closed_connections.poll_next_unpin(cx)
        {
            if let Entry::Occupied(entry) = this.open.entry(source) {
                if entry.get().connection_id == connection_id {
                    entry.remove();
                }
            }
        }

        for read in 0..MAX_SOCKET_READS_PER_POLL {
            let (addrs, meta, socket) =
                match this.listener.poll_recv_from(cx, &mut this.read_buffer) {
                    // No error is expected to be returned by the listener.
                    Poll::Ready(Err(error)) => {
                        tracing::info!(
                            target: LOG_TARGET,
                            ?error,
                            "webrtc udp socket closed",
                        );
                        return Poll::Ready(None);
                    }
                    Poll::Pending => break,
                    Poll::Ready(Ok(res)) => res,
                };

            if read + 1 == MAX_SOCKET_READS_PER_POLL {
                cx.waker().wake_by_ref();
            }

            // `stride == 0` is only possible with `len == 0`
            // (`quinn-udp` sets `stride = len` when GRO info is absent),
            // normalize it to keep the loop below finite.
            let stride = if meta.stride == 0 {
                meta.len
            } else {
                meta.stride
            };

            // Reassembled oversized datagrams crash `str0m`. Drop them.
            if stride > MAX_UDP_PAYLOAD_SIZE {
                tracing::trace!(target: LOG_TARGET, ?addrs, "dropping oversized datagram(s)");
                continue;
            };

            // The read may contain multiple GRO-coalesced datagrams,
            // feed them to the connection one by one.
            let mut should_poll = false;
            let mut offset = 0;
            while offset < meta.len {
                let len = stride.min(meta.len - offset);
                let datagram = this.read_buffer[offset..offset + len].to_vec();
                offset += len;

                match this.on_socket_input(addrs, &socket, datagram) {
                    Ok(poll) => should_poll |= poll,
                    Err(error) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?addrs,
                            ?error,
                            "failed to handle datagram",
                        );
                    }
                }
            }

            if should_poll {
                this.process_connection(&addrs, cx);
            }
        }

        // go over all pending timeouts to see if any of them have expired
        // and if any of them have, poll the connection until it registers another timeout
        let filtered_timeouts: Vec<_> = this
            .timeouts
            .iter_mut()
            .filter_map(|(source, mut delay)| match Pin::new(&mut delay).poll(cx) {
                Poll::Pending => None,
                Poll::Ready(_) => Some(*source),
            })
            .collect();

        for addrs in filtered_timeouts {
            this.process_connection(&addrs, cx);
        }

        this.pop_event().map_or(Poll::Pending, |event| Poll::Ready(Some(event)))
    }
}

/// Check if the packet received is STUN.
///
/// Extracted from the STUN RFC 5389 (<https://datatracker.ietf.org/doc/html/rfc5389#page-10>):
///  All STUN messages MUST start with a 20-byte header followed by zero
///  or more Attributes.  The STUN header contains a STUN message type,
///  magic cookie, transaction ID, and message length.
///
/// ```ignore
///      0                   1                   2                   3
///      0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
///     +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///     |0 0|     STUN Message Type     |         Message Length        |
///     +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///     |                         Magic Cookie                          |
///     +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///     |                                                               |
///     |                     Transaction ID (96 bits)                  |
///     |                                                               |
///     +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
fn is_stun_packet(bytes: &[u8]) -> bool {
    const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
    // 20 bytes for the header, then follows attributes.
    bytes.len() >= 20 && bytes[0] < 2 && bytes[4..8] == STUN_MAGIC_COOKIE
}

/// Check whether a datagram may be forwarded to str0m.
///
/// Datagrams are demultiplexed by their first byte.
/// The only kinds a libp2p WebRTC transport legitimately receives
/// are STUN and DTLS, SCTP is not demultiplexed on its own
/// but carried inside DTLS.
/// RTP/RTCP never arrive, since no media is negotiated.
///
/// The accepted set mirrors str0m's own classifier (`str0m::io::MultiplexKind`):
/// STUN is `byte0 < 2` with a full 20-byte header,
/// DTLS is `byte0` in `20..=63`.
///
/// A datagram that passes this filter but that str0m then rejects
/// closes the connection which would tear down a live peer.
fn is_forwardable(bytes: &[u8]) -> bool {
    match bytes.first().copied() {
        // STUN messages always carry a 20-byte header,
        // shorter ones are rejected by str0m.
        Some(0..=1) => bytes.len() >= 20,
        Some(20..=63) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        executor::DefaultExecutor,
        transport::{
            manager::{limits::DEFAULT_MAX_PENDING_INCOMING_CONNECTIONS, TransportManagerBuilder},
            ConnectionLimitsConfig, CONNECTION_OPEN_TIMEOUT,
        },
    };
    use futures::{future::poll_fn, task::noop_waker};
    use multiaddr::Protocol;
    use str0m::ice::{StunMessage, TransId};
    use tokio::net::UdpSocket;

    async fn transport() -> (WebRtcTransport, SocketAddr) {
        transport_with_limit(DEFAULT_MAX_PENDING_INCOMING_CONNECTIONS).await
    }

    async fn transport_with_limit(limit: usize) -> (WebRtcTransport, SocketAddr) {
        let manager = TransportManagerBuilder::new()
            .with_connection_limits_config(
                ConnectionLimitsConfig::default().max_pending_incoming_connections(Some(limit)),
            )
            .build();
        let (transport, addresses) = WebRtcTransport::new(
            manager.transport_handle(Arc::new(DefaultExecutor)),
            Config {
                listen_addresses: vec!["/ip4/127.0.0.1/udp/0/webrtc-direct".parse().unwrap()],
                ..Default::default()
            },
            Arc::new(TokioResolver::builder_tokio().unwrap().build().unwrap()),
        )
        .unwrap();
        let port = addresses[0]
            .iter()
            .find_map(|protocol| match protocol {
                Protocol::Udp(port) => Some(port),
                _ => None,
            })
            .unwrap();
        (transport, SocketAddr::from(([127, 0, 0, 1], port)))
    }

    fn stun_request(username: &str) -> Vec<u8> {
        let message = StunMessage::binding_request(username, TransId::new(), true, 1, 1, false);
        let mut buffer = vec![0; 512];
        // Invalid integrity still reaches RTC allocation, before ICE authentication.
        let len = message.to_bytes(Some(b"password"), &mut buffer, |_, _| [0; 20]).unwrap();
        buffer.truncate(len);
        buffer
    }

    async fn receive_stun(
        transport: &mut WebRtcTransport,
        address: SocketAddr,
    ) -> (AddressPair, Arc<WebRtcSocket>, Vec<u8>) {
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.send_to(&stun_request("user:password"), address).await.unwrap();
        let (addrs, meta, socket) =
            poll_fn(|cx| transport.listener.poll_recv_from(cx, &mut transport.read_buffer))
                .await
                .unwrap();
        (addrs, socket, transport.read_buffer[..meta.len].to_vec())
    }

    fn insert_opening(
        transport: &mut WebRtcTransport,
        addrs: AddressPair,
        socket: Arc<WebRtcSocket>,
        deadline: Instant,
    ) -> ConnectionId {
        let available = transport.context.pending_incoming_connections.available_permits();
        let permit = transport.context.try_acquire_inbound_permit().unwrap();
        let (rtc, channel_id) = transport.make_rtc("user", "password", &addrs).unwrap();
        let connection_id = transport.context.next_connection_id();
        transport.opening.insert(
            addrs,
            OpeningWebRtcConnection::new(
                rtc,
                connection_id,
                channel_id,
                transport.context.keypair.clone(),
                addrs,
                socket,
                InboundOpening::new(permit, deadline),
            ),
        );
        // The opening holds the capacity.
        assert_eq!(
            transport.context.pending_incoming_connections.available_permits(),
            available - 1
        );
        connection_id
    }

    fn queue_established(
        transport: &mut WebRtcTransport,
        addrs: AddressPair,
        connection_id: ConnectionId,
    ) {
        let peer = PeerId::random();
        let endpoint = Endpoint::listener(Multiaddr::empty(), connection_id);
        transport.connections.insert(connection_id, (peer, addrs, endpoint.clone()));
        transport
            .pending_events
            .push_back(TransportEvent::ConnectionEstablished { peer, endpoint });
    }

    fn assert_no_openings(transport: &WebRtcTransport) {
        assert!(transport.opening.is_empty());
        assert!(transport.timeouts.is_empty());
        assert!(transport.connections.is_empty());
        assert!(transport.pending_events.is_empty());
        assert_eq!(
            transport.context.pending_incoming_connections.available_permits(),
            DEFAULT_MAX_PENDING_INCOMING_CONNECTIONS
        );
    }

    #[tokio::test]
    async fn repeated_stun_does_not_extend_opening_deadline() {
        let (mut transport, address) = transport().await;
        let (addrs, socket, buffer) = receive_stun(&mut transport, address).await;
        let before = Instant::now();
        assert!(transport.on_socket_input(addrs, &socket, buffer).unwrap());
        let after = Instant::now();
        let connection = &transport.opening[&addrs];
        let deadline = connection.deadline();
        let connection_id = *connection.connection_id();
        assert!(deadline >= before + CONNECTION_OPEN_TIMEOUT);
        assert!(deadline <= after + CONNECTION_OPEN_TIMEOUT);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        transport.process_connection(&addrs, &mut cx);
        for username in ["user:password", "other:credentials", "user:password"] {
            assert!(transport.on_socket_input(addrs, &socket, stun_request(username)).unwrap());
            transport.process_connection(&addrs, &mut cx);
            let connection = &transport.opening[&addrs];
            assert_eq!(connection.deadline(), deadline);
            assert_eq!(*connection.connection_id(), connection_id);
            assert_eq!(transport.opening.len(), 1);
        }
    }

    #[tokio::test]
    async fn idle_opening_expires_after_timer_replacement() {
        let (mut transport, address) = transport().await;
        let (addrs, socket, _) = receive_stun(&mut transport, address).await;
        insert_opening(
            &mut transport,
            addrs,
            socket,
            Instant::now() + Duration::from_millis(200),
        );
        let waker = noop_waker();
        transport.process_connection(&addrs, &mut Context::from_waker(&waker));

        // Force a protocol wakeup, so poll_next must register the replacement timer.
        transport.timeouts.insert(addrs, Delay::new(Duration::ZERO));
        let task = tokio::spawn(async move {
            poll_fn(|cx| {
                assert!(Pin::new(&mut transport).poll_next(cx).is_pending());
                if transport.opening.is_empty() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            assert_no_openings(&transport);
        });

        // The guard wakes the join handle, never the transport task.
        tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn expiry_removes_queued_establishment_and_connection_state() {
        let (mut transport, address) = transport().await;
        let (addrs, socket, _) = receive_stun(&mut transport, address).await;
        let connection_id = insert_opening(&mut transport, addrs, socket, Instant::now());
        queue_established(&mut transport, addrs, connection_id);
        transport.timeouts.insert(addrs, Delay::new(Duration::from_secs(60)));

        let buffer = stun_request("user:password");
        let connection = transport.opening.get_mut(&addrs).unwrap();
        assert!(matches!(
            connection.on_input(buffer.as_slice().try_into().unwrap()),
            Err(Error::Timeout)
        ));
        assert!(matches!(connection.on_timeout(), Err(Error::Timeout)));
        assert!(matches!(
            connection.poll_process(),
            opening::WebRtcEvent::ConnectionClosed
        ));

        let waker = noop_waker();
        assert!(Pin::new(&mut transport)
            .poll_next(&mut Context::from_waker(&waker))
            .is_pending());
        assert_no_openings(&transport);
    }

    #[tokio::test]
    async fn panicked_opening_is_removed() {
        let (mut transport, address) = transport().await;
        let (addrs, socket, buffer) = receive_stun(&mut transport, address).await;
        insert_opening(
            &mut transport,
            addrs,
            socket.clone(),
            Instant::now() + CONNECTION_OPEN_TIMEOUT,
        );
        transport.opening.get_mut(&addrs).unwrap().rtc_mut().poison();

        // The datagram is rejected, but the connection must still be polled to be removed.
        assert!(transport.on_socket_input(addrs, &socket, buffer).unwrap());
        let waker = noop_waker();
        transport.process_connection(&addrs, &mut Context::from_waker(&waker));

        assert_no_openings(&transport);
    }

    #[tokio::test]
    async fn acceptance_after_deadline_fails_and_cleans_up() {
        let (mut transport, address) = transport().await;
        let (addrs, socket, _) = receive_stun(&mut transport, address).await;
        let connection_id = insert_opening(&mut transport, addrs, socket, Instant::now());
        queue_established(&mut transport, addrs, connection_id);
        transport.timeouts.insert(addrs, Delay::new(Duration::from_secs(60)));

        assert!(matches!(
            transport.accept(connection_id),
            Err(Error::Timeout)
        ));
        assert_no_openings(&transport);
        assert!(transport.open.is_empty());
    }

    #[tokio::test]
    async fn rejection_removes_timer_and_queued_establishment() {
        let (mut transport, address) = transport().await;
        let (addrs, socket, _) = receive_stun(&mut transport, address).await;
        let connection_id = insert_opening(
            &mut transport,
            addrs,
            socket,
            Instant::now() + CONNECTION_OPEN_TIMEOUT,
        );
        queue_established(&mut transport, addrs, connection_id);
        transport.timeouts.insert(addrs, Delay::new(CONNECTION_OPEN_TIMEOUT));

        transport.reject(connection_id).unwrap();
        assert_no_openings(&transport);
    }

    #[tokio::test]
    async fn receive_budget_preserves_timer_progress_with_queued_datagrams() {
        use futures::task::{waker, ArcWake};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct WakeCounter(AtomicUsize);
        impl ArcWake for WakeCounter {
            fn wake_by_ref(this: &Arc<Self>) {
                this.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let (mut transport, address) = transport().await;
        let (addrs, socket, _) = receive_stun(&mut transport, address).await;
        // Only the fired timer can remove this expired opening.
        insert_opening(&mut transport, addrs, socket, Instant::now());
        transport.timeouts.insert(addrs, Delay::new(Duration::ZERO));
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        for _ in 0..=MAX_SOCKET_READS_PER_POLL {
            sender.send_to(&[0xff; 20], address).unwrap();
        }

        let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let waker = waker(counter.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut transport).poll_next(&mut cx).is_pending());
        assert!(transport.opening.is_empty());
        assert!(counter.0.load(Ordering::Relaxed) > 0);
        assert!(matches!(
            transport.listener.poll_recv_from(&mut cx, &mut transport.read_buffer),
            Poll::Ready(Ok(_))
        ));
    }

    async fn opening_transport(limit: usize) -> (WebRtcTransport, Arc<WebRtcSocket>, AddressPair) {
        let (transport, _) = transport_with_limit(limit).await;
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addrs = AddressPair {
            local: socket.local_addr().unwrap(),
            remote: "127.0.0.1:40000".parse().unwrap(),
        };
        (
            transport,
            Arc::new(WebRtcSocket::new(socket).unwrap()),
            addrs,
        )
    }

    #[tokio::test]
    async fn opening_admission_and_existing_session_traffic() {
        let (mut transport, socket, addrs) = opening_transport(1).await;
        let other = transport.context.try_acquire_inbound_permit().unwrap();
        assert!(!transport
            .on_socket_input(addrs, &socket, stun_request("user:password"))
            .unwrap());
        assert!(transport.opening.is_empty());

        // Admission is checked before decoding: at capacity a malformed datagram is dropped,
        // otherwise it is an error and its capacity is released.
        let malformed = stun_datagram(20);
        assert!(!transport.on_socket_input(addrs, &socket, malformed.clone()).unwrap());
        drop(other);
        assert!(transport.on_socket_input(addrs, &socket, malformed).is_err());
        assert_eq!(
            transport.context.pending_incoming_connections.available_permits(),
            1
        );
        assert!(transport.opening.is_empty());

        assert!(transport
            .on_socket_input(addrs, &socket, stun_request("user:password"))
            .unwrap());

        let another = AddressPair {
            remote: "127.0.0.1:40001".parse().unwrap(),
            ..addrs
        };
        assert!(!transport
            .on_socket_input(another, &socket, stun_request("user:password"))
            .unwrap());
        assert!(transport
            .on_socket_input(addrs, &socket, stun_request("user:password"))
            .unwrap());
        assert_eq!(transport.opening.len(), 1);
    }

    #[tokio::test]
    async fn openings_share_a_transport_budget_without_a_per_ip_limit() {
        let (mut transport, socket, addrs) = opening_transport(2).await;
        assert!(transport
            .on_socket_input(addrs, &socket, stun_request("user:password"))
            .unwrap());
        let another = AddressPair {
            remote: "127.0.0.1:40001".parse().unwrap(),
            ..addrs
        };
        assert!(transport
            .on_socket_input(another, &socket, stun_request("user:password"))
            .unwrap());
        let third = AddressPair {
            remote: "127.0.0.2:40000".parse().unwrap(),
            ..addrs
        };
        assert!(!transport
            .on_socket_input(third, &socket, stun_request("user:password"))
            .unwrap());
        let capacity = transport.context.pending_incoming_connections.clone();
        assert_eq!(capacity.available_permits(), 0);
        drop(transport);
        assert_eq!(capacity.available_permits(), 2);
    }

    /// A STUN-shaped datagram of `len` bytes, carrying the magic cookie.
    fn stun_datagram(len: usize) -> Vec<u8> {
        let mut datagram = vec![0x00; len];
        if len >= 8 {
            datagram[4..8].copy_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
        }
        datagram
    }

    #[test]
    fn stun_and_dtls_are_forwardable() {
        // STUN, which always carries a 20-byte header.
        assert!(is_forwardable(&stun_datagram(20)));
        assert!(is_forwardable(&stun_datagram(64)));
        assert!(is_forwardable(&[0x01; 20]));

        // DTLS, at any length.
        for byte in 20..=63 {
            assert!(is_forwardable(&[byte]), "{byte:#04x} should be forwardable");
            assert!(
                is_forwardable(&[byte; 20]),
                "{byte:#04x} should be forwardable"
            );
        }
    }

    #[test]
    fn truncated_stun_datagrams_are_not_forwardable() {
        // str0m only classifies `byte0 < 2` as STUN once the 20-byte header is complete, and
        // errors otherwise. Forwarding these would let a spoofed datagram close a live
        // connection, so they must be dropped here.
        for len in 0..20 {
            assert!(
                !is_forwardable(&stun_datagram(len)),
                "{len}-byte stun datagram should not be forwardable"
            );
            assert!(
                !is_forwardable(&vec![0x01; len]),
                "{len}-byte stun datagram should not be forwardable"
            );
        }
    }

    #[test]
    fn rtp_rtcp_and_unknown_datagrams_are_not_forwardable() {
        // RTP/RTCP, and the gaps between the STUN, DTLS and RTP/RTCP ranges. Length never
        // rescues any of them.
        let rejected = (2..=19).chain(64..=255);
        for byte in rejected {
            assert!(
                !is_forwardable(&[byte]),
                "{byte:#04x} should not be forwardable"
            );
            assert!(
                !is_forwardable(&[byte; 20]),
                "{byte:#04x} should not be forwardable"
            );
        }

        // Empty datagram.
        assert!(!is_forwardable(&[]));
    }
}
