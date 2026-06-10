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
    error::{Error, ParseError, SubstreamError},
    multistream_select::{
        webrtc_listener_negotiate, HandshakeResult, ListenerSelectResult, NegotiationError,
        WebRtcDialerState,
    },
    protocol::{Direction, Permit, ProtocolCommand, ProtocolSet, SubstreamKeepAlive},
    substream::Substream,
    transport::{
        webrtc::{
            schema::webrtc::message::Flag,
            substream::{Message, Substream as WebRtcSubstream, SubstreamHandle},
            util::{extract_framed_message, WebRtcMessage},
        },
        Endpoint, SUBSTREAM_OPEN_TIMEOUT,
    },
    types::{protocol::ProtocolName, SubstreamId},
    PeerId,
};

use bytes::{Bytes, BytesMut};
use futures::{task::AtomicWaker, Stream, StreamExt};
use indexmap::IndexMap;
use str0m::{
    channel::{Channel, ChannelConfig, ChannelId},
    net::{Protocol as Str0mProtocol, Receive},
    Event, IceConnectionState, Input, Output, Rtc,
};
use tokio::{net::UdpSocket, sync::mpsc::Receiver};

use std::{
    collections::{HashMap, HashSet, VecDeque},
    hash::Hash,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::webrtc::connection";

/// Threshold under which str0m emits Event::ChannelBufferedAmountLow.
const BACKPRESSURE_THRESHOLD: usize = 16 * (1 << 10); // 16 KB

/// Maximum number of pending messages supported per channel.
const MAX_PENDING_PER_CHANNEL: usize = 16;

/// Opening channel context.
#[derive(Debug)]
struct ChannelContext {
    /// Protocol name.
    protocol: ProtocolName,

    /// Fallback names.
    fallback_names: Vec<ProtocolName>,

    /// Substream ID.
    substream_id: SubstreamId,

    /// Permit which keeps the connection open while we are opening a substream. Must be returned
    /// to [`TransportService`](crate::protocol::TransportService), where it can be safely dropped
    /// after upgrading the connection.
    opening_permit: Permit,

    /// Whether this substream should keep the connection alive while it exists, i.e., whether it
    /// should store the permit entioned above for the lifetime of the substream.
    keep_alive: SubstreamKeepAlive,
}

/// Tracks deadlines for channels that are still in the opening/negotiation phase.
///
/// A malicious or broken peer can open a data channel (or accept one we opened) and then
/// never complete the `multistream-select` handshake, leaving the channel parked forever
/// with no mechanism to remove it. Recording a deadline per opening channel lets the
/// connection event loop reclaim channels whose negotiation has stalled past
/// [`SUBSTREAM_OPEN_TIMEOUT`] instead of leaking them until the whole connection is torn
/// down.
#[derive(Debug)]
struct OpeningDeadlines<K> {
    /// Per-channel negotiation deadlines.
    deadlines: HashMap<K, Instant>,
}

impl<K: Eq + Hash + Copy> OpeningDeadlines<K> {
    /// Create a new, empty [`OpeningDeadlines`].
    fn new() -> Self {
        Self {
            deadlines: HashMap::new(),
        }
    }

    /// Returns `true` if no channel is currently being tracked.
    fn is_empty(&self) -> bool {
        self.deadlines.is_empty()
    }

    /// Start tracking `key` with the given negotiation `deadline`.
    ///
    /// An existing deadline for `key` is intentionally preserved so that a peer cannot keep
    /// extending the negotiation window: the bound covers the whole opening lifecycle, from
    /// when the channel starts opening until it reaches [`ChannelState::Open`].
    fn insert(&mut self, key: K, deadline: Instant) {
        self.deadlines.entry(key).or_insert(deadline);
    }

    /// Stop tracking `key` (e.g. it reached [`ChannelState::Open`] or was closed).
    fn remove(&mut self, key: &K) {
        self.deadlines.remove(key);
    }

    /// The earliest tracked deadline, if any.
    ///
    /// Used to bound how long the event loop sleeps so a stalled channel is swept promptly.
    fn earliest(&self) -> Option<Instant> {
        self.deadlines.values().copied().min()
    }

    /// Remove and return every channel whose deadline is at or before `now`.
    fn drain_expired(&mut self, now: Instant) -> Vec<K> {
        let expired: Vec<K> = self
            .deadlines
            .iter()
            .filter_map(|(key, deadline)| (*deadline <= now).then_some(*key))
            .collect();

        for key in &expired {
            self.deadlines.remove(key);
        }

        expired
    }
}

/// Set of [`SubstreamHandle`]s.
struct SubstreamHandleSet {
    /// Current index.
    index: usize,

    /// Substream handles.
    handles: IndexMap<ChannelId, SubstreamHandle>,

    /// Substreams that have pending messages.
    pending: HashSet<ChannelId>,

    /// Waker used to drive the stream when no handle can make progress.
    waker: AtomicWaker,
}

impl SubstreamHandleSet {
    /// Create new [`SubstreamHandleSet`].
    pub fn new() -> Self {
        Self {
            index: 0usize,
            handles: IndexMap::new(),
            pending: HashSet::new(),
            waker: AtomicWaker::new(),
        }
    }

    /// Get mutable access to `SubstreamHandle`.
    pub fn get_mut(&mut self, key: &ChannelId) -> Option<&mut SubstreamHandle> {
        self.handles.get_mut(key)
    }

    /// Insert new handle to [`SubstreamHandleSet`].
    pub fn insert(&mut self, key: ChannelId, handle: SubstreamHandle) {
        assert!(self.handles.insert(key, handle).is_none());
        self.waker.wake();
    }

    /// Remove handle from [`SubstreamHandleSet`].
    pub fn remove(&mut self, key: &ChannelId) -> Option<SubstreamHandle> {
        self.pending.remove(key);
        self.handles.swap_remove(key)
    }

    /// Mark channel as having pending messages.
    pub fn add_pending(&mut self, key: ChannelId) {
        self.pending.insert(key);
    }

    /// Unmark channel as having pending messages.
    pub fn clear_pending(&mut self, key: &ChannelId) {
        if self.pending.remove(key) {
            self.waker.wake();
        }
    }
}

impl Stream for SubstreamHandleSet {
    type Item = (ChannelId, Option<Message>);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let len = match self.handles.len() {
            0 => {
                self.waker.register(cx.waker());
                return Poll::Pending;
            }
            len => len,
        };
        let start_index = self.index;

        loop {
            let index = self.index % len;
            self.index += 1;

            let key =
                self.handles.get_index(index).map(|(k, _)| k).cloned().expect("handle to exist");

            if !self.pending.contains(&key) {
                let (key, stream) = self.handles.get_index_mut(index).expect("handle to exist");
                match stream.poll_next_unpin(cx) {
                    Poll::Pending => {}
                    Poll::Ready(event) => return Poll::Ready(Some((*key, event))),
                }
            }

            if self.index == start_index + len {
                self.waker.register(cx.waker());
                break Poll::Pending;
            }
        }
    }
}

/// Channel state.
#[derive(Debug)]
enum ChannelState {
    /// Channel is closing.
    Closing,

    /// Inbound channel is opening.
    InboundOpening {
        /// Whether the multistream-select header has already been received/sent.
        header_received: bool,
    },

    /// Outbound channel is opening.
    OutboundOpening {
        /// Channel context.
        context: ChannelContext,

        /// `multistream-select` dialer state.
        dialer_state: WebRtcDialerState,
    },

    /// Channel is open.
    Open {
        /// Substream ID.
        substream_id: SubstreamId,

        /// Channel ID.
        channel_id: ChannelId,

        /// Connection permit if this substream needs to keep connection open.
        lifetime_permit: Option<Permit>,
    },
}

/// WebRTC connection.
pub struct WebRtcConnection {
    /// `str0m` WebRTC object.
    rtc: Rtc,

    /// Protocol set.
    protocol_set: ProtocolSet,

    /// Remote peer ID.
    peer: PeerId,

    /// Endpoint.
    endpoint: Endpoint,

    /// Peer address
    peer_address: SocketAddr,

    /// Local address.
    local_address: SocketAddr,

    /// Transport socket.
    socket: Arc<UdpSocket>,

    /// RX channel for receiving datagrams from the transport.
    dgram_rx: Receiver<Vec<u8>>,

    /// Pending outbound channels.
    pending_outbound: HashMap<ChannelId, ChannelContext>,

    /// Pending outbound messages,
    /// at most [`MAX_PENDING_PER_CHANNEL`] per channel.
    pending_messages: HashMap<ChannelId, VecDeque<Vec<u8>>>,

    /// Open channels.
    channels: HashMap<ChannelId, ChannelState>,

    /// Substream handles.
    handles: SubstreamHandleSet,

    /// Inbound data channel byte buffer for reassembling full protobuf frames.
    ///
    /// The libp2p-go msgio implementation issues two separate `Write` calls:
    ///  - variant length
    ///  - protobuf body
    ///
    /// These will become two distinct SCTP messages on the data channel.
    ///
    /// Accumulate raw bytes here and only attempt protobuf decode once a
    /// full `varint length ++ body` frame is available.
    recv_buffers: HashMap<ChannelId, BytesMut>,

    /// Negotiation deadlines for channels that are still opening.
    ///
    /// Bounds the `multistream-select` negotiation phase so channels that never finish
    /// handshaking are reclaimed instead of leaking. See [`OpeningDeadlines`].
    opening_deadlines: OpeningDeadlines<ChannelId>,
}

impl WebRtcConnection {
    /// Create new [`WebRtcConnection`].
    pub fn new(
        rtc: Rtc,
        peer: PeerId,
        peer_address: SocketAddr,
        local_address: SocketAddr,
        socket: Arc<UdpSocket>,
        protocol_set: ProtocolSet,
        endpoint: Endpoint,
        dgram_rx: Receiver<Vec<u8>>,
    ) -> Self {
        Self {
            rtc,
            protocol_set,
            peer,
            peer_address,
            local_address,
            socket,
            endpoint,
            dgram_rx,
            pending_outbound: HashMap::new(),
            pending_messages: HashMap::new(),
            channels: HashMap::new(),
            handles: SubstreamHandleSet::new(),
            recv_buffers: HashMap::new(),
            opening_deadlines: OpeningDeadlines::new(),
        }
    }

    /// Deadline after which a channel that is currently opening is considered to have
    /// stalled and is force-closed.
    fn opening_deadline() -> Instant {
        Instant::now() + SUBSTREAM_OPEN_TIMEOUT
    }

    /// Handle opened channel.
    ///
    /// If the channel is inbound, nothing is done because we have to wait for data
    /// `multistream-select` handshake to be received from remote peer before anything
    /// else can be done.
    ///
    /// If the channel is outbound, send `multistream-select` handshake to remote peer.
    async fn on_channel_opened(
        &mut self,
        channel_id: ChannelId,
        channel_name: String,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.peer,
            ?channel_id,
            ?channel_name,
            "channel opened",
        );

        if let Some(mut channel) = self.rtc.channel(channel_id) {
            channel.set_buffered_amount_low_threshold(BACKPRESSURE_THRESHOLD);
        }

        let Some(mut context) = self.pending_outbound.remove(&channel_id) else {
            tracing::trace!(
                target: LOG_TARGET,
                peer = ?self.peer,
                ?channel_id,
                "inbound channel opened, wait for `multistream-select` message",
            );

            self.opening_deadlines.insert(channel_id, Self::opening_deadline());
            self.channels.insert(
                channel_id,
                ChannelState::InboundOpening {
                    header_received: false,
                },
            );
            return Ok(());
        };

        let fallback_names = std::mem::take(&mut context.fallback_names);
        let (dialer_state, message) =
            WebRtcDialerState::propose(context.protocol.clone(), fallback_names)?;
        let message = WebRtcMessage::encode(message, None);

        self.write(channel_id, message)?;

        self.channels.insert(
            channel_id,
            ChannelState::OutboundOpening {
                context,
                dialer_state,
            },
        );

        Ok(())
    }

    // Attempt to write a message over the specified channel,
    // save the message as pending if `WebRtcConnection` didn't have
    // enough space.
    fn write(&mut self, channel_id: ChannelId, message: Vec<u8>) -> Result<(), Error> {
        let Some(mut channel) = self.rtc.channel(channel_id) else {
            tracing::trace!(
                target: LOG_TARGET,
                peer = ?self.peer,
                ?channel_id,
                "protocol rejected received for non-existing channel",
            );
            return Err(Error::ChannelDoesntExist);
        };

        match self.pending_messages.get_mut(&channel_id) {
            Some(messages) if !messages.is_empty() => {
                if messages.len() >= MAX_PENDING_PER_CHANNEL {
                    return Err(Error::ChannelClogged);
                }

                messages.push_back(message);
                return Ok(());
            }
            _ => (),
        }

        let succeeded = Self::channel_write(&mut channel, channel_id, &message, self.peer)?;

        if !succeeded {
            let pending_messages = self.pending_messages.entry(channel_id).or_default();
            if pending_messages.len() >= MAX_PENDING_PER_CHANNEL {
                return Err(Error::ChannelClogged);
            }

            pending_messages.push_back(message);
            self.handles.add_pending(channel_id);
            return Ok(());
        }

        Ok(())
    }

    fn channel_write(
        channel: &mut Channel<'_>,
        channel_id: ChannelId,
        message: &[u8],
        peer: PeerId,
    ) -> Result<bool, Error> {
        match channel.write(true, message) {
            Ok(succeeded) => Ok(succeeded),
            Err(e) => {
                tracing::trace!(
                    target: LOG_TARGET,
                    peer = ?peer,
                    ?channel_id,
                    ?e,
                    "failed to write message to webrtc channel",
                );
                Err(Error::WebRtc(e))
            }
        }
    }

    // Attempt to write all pending messages of the specified ChannelId.
    // Returns whether all messages have been sent or not.
    fn write_pending(&mut self, channel_id: ChannelId) -> Result<bool, Error> {
        let Some(mut channel) = self.rtc.channel(channel_id) else {
            tracing::trace!(
                target: LOG_TARGET,
                peer = ?self.peer,
                ?channel_id,
                "protocol rejected received for non-existing channel",
            );
            return Err(Error::ChannelDoesntExist);
        };

        loop {
            let Some(pending_messages) = self.pending_messages.get_mut(&channel_id) else {
                // This should never happen, `write_pending` should be called
                // for only the channel with pending messages. Treat as a no-op
                // instead of panicking to stay defensive.
                self.handles.clear_pending(&channel_id);
                return Ok(true);
            };

            let Some(message) = pending_messages.front() else {
                self.pending_messages.remove(&channel_id);
                self.handles.clear_pending(&channel_id);
                break Ok(true);
            };

            let succeeded = Self::channel_write(&mut channel, channel_id, message, self.peer)?;
            if succeeded {
                self.pending_messages
                    .get_mut(&channel_id)
                    .and_then(|messages| messages.pop_front());
            } else {
                break Ok(false);
            }
        }
    }

    /// Handle closed channel.
    async fn on_channel_closed(&mut self, channel_id: ChannelId) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.peer,
            ?channel_id,
            "channel closed",
        );

        // If this was a pending outbound channel (waiting for DCEP ACK from remote),
        // report the failure so the protocol handler can retry.
        if let Some(context) = self.pending_outbound.remove(&channel_id) {
            tracing::debug!(
                target: LOG_TARGET,
                peer = ?self.peer,
                ?channel_id,
                protocol = %context.protocol,
                substream_id = ?context.substream_id,
                "outbound channel closed before opening, reporting failure",
            );

            let _ = self
                .protocol_set
                .report_substream_open_failure(
                    context.protocol,
                    context.substream_id,
                    SubstreamError::ConnectionClosed,
                )
                .await;
        }

        if let Some(ChannelState::OutboundOpening { context, .. }) =
            self.channels.remove(&channel_id)
        {
            tracing::debug!(
                target: LOG_TARGET,
                peer = ?self.peer,
                ?channel_id,
                protocol = %context.protocol,
                substream_id = ?context.substream_id,
                "outbound channel closed during negotiation, reporting failure",
            );

            let _ = self
                .protocol_set
                .report_substream_open_failure(
                    context.protocol,
                    context.substream_id,
                    SubstreamError::ConnectionClosed,
                )
                .await;
        }

        self.pending_messages.remove(&channel_id);
        self.handles.remove(&channel_id);
        self.recv_buffers.remove(&channel_id);
        self.opening_deadlines.remove(&channel_id);

        Ok(())
    }

    /// Close any channels whose `multistream-select` negotiation has stalled past
    /// [`SUBSTREAM_OPEN_TIMEOUT`].
    ///
    /// A malicious or broken peer may open a data channel (or accept one we opened) and then
    /// never complete the handshake, leaving the channel parked forever. Bounding the
    /// negotiation phase ensures such channels are reclaimed instead of leaking until the
    /// whole connection is torn down. Outbound opens have a protocol handler waiting for the
    /// result, so they are notified with a [`NegotiationError::Timeout`]; inbound opens have
    /// no waiting handler and are simply closed.
    ///
    /// [`NegotiationError::Timeout`]: crate::error::NegotiationError::Timeout
    async fn sweep_opening_timeouts(&mut self, now: Instant) {
        for channel_id in self.opening_deadlines.drain_expired(now) {
            // Outbound context: `pending_outbound` before the channel acks, `OutboundOpening`
            // during negotiation. Inbound opens have none.
            let outbound_context = match self.pending_outbound.remove(&channel_id) {
                Some(context) => Some(context),
                None => match self.channels.remove(&channel_id) {
                    Some(ChannelState::OutboundOpening { context, .. }) => Some(context),
                    _ => None,
                },
            };

            if let Some(context) = outbound_context {
                tracing::debug!(
                    target: LOG_TARGET,
                    peer = ?self.peer,
                    ?channel_id,
                    protocol = %context.protocol,
                    substream_id = ?context.substream_id,
                    "outbound substream negotiation timed out, closing channel",
                );

                let _ = self
                    .protocol_set
                    .report_substream_open_failure(
                        context.protocol,
                        context.substream_id,
                        SubstreamError::NegotiationError(crate::error::NegotiationError::Timeout),
                    )
                    .await;
            } else {
                tracing::debug!(
                    target: LOG_TARGET,
                    peer = ?self.peer,
                    ?channel_id,
                    "inbound substream negotiation timed out, closing channel",
                );
            }

            self.pending_messages.remove(&channel_id);
            self.recv_buffers.remove(&channel_id);
            self.handles.remove(&channel_id);
            self.channels.insert(channel_id, ChannelState::Closing);
            self.rtc.direct_api().close_data_channel(channel_id);
        }
    }

    /// Handle data received to an opening inbound channel.
    ///
    /// The first message received over an inbound channel is the `multistream-select` handshake.
    /// This handshake contains the protocol the remote peer wants to use for this channel. Parse
    /// the handshake and check whether the proposed protocol is supported by the local node.
    /// If not, send rejection to remote peer and but keep the channel open so that the peer can
    /// propose a fallback. If the local node support the protocol, send confirmation for the
    /// protocol to remote peer and report an opened substream to the selected protocol.
    ///
    /// Returns `Ok(Some(...))` if the protocol was accepted and the substream opened,
    /// `Ok(None)` if the proposed protocol was rejected (the `na` response has been sent
    /// and the channel should remain in [`ChannelState::InboundOpening`] so the dialer can
    /// propose another protocol per back-and-forth multistream-select negotiation),
    /// or `Err(...)` on a fatal error (channel should be closed).
    async fn on_inbound_opening_channel_data(
        &mut self,
        channel_id: ChannelId,
        data: Bytes,
        header_received: bool,
    ) -> crate::Result<Option<(SubstreamId, SubstreamHandle, Option<Permit>)>> {
        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.peer,
            ?channel_id,
            "handle opening inbound substream",
        );

        // Decode errors are not recoverable.
        let payload = WebRtcMessage::decode(&data)?.payload.ok_or(Error::InvalidData)?;
        let protocols = self.protocol_set.protocols_with_keep_alives();
        let protocol_names = protocols.keys().cloned().collect();
        let (response, negotiated) =
            match webrtc_listener_negotiate(protocol_names, payload.into(), header_received)? {
                ListenerSelectResult::Accepted { protocol, message } => (message, Some(protocol)),
                ListenerSelectResult::Rejected { message }
                | ListenerSelectResult::PendingProtocol { message } => (message, None),
            };

        let message = WebRtcMessage::encode(response.to_vec(), None);
        self.write(channel_id, message)?;

        let Some(protocol) = negotiated else {
            tracing::trace!(
                target: LOG_TARGET,
                peer = ?self.peer,
                ?channel_id,
                "inbound protocol rejected, keeping channel open for back-and-forth negotiation",
            );
            return Ok(None);
        };

        let substream_id = self.protocol_set.next_substream_id();
        let codec = self.protocol_set.protocol_codec(&protocol);
        let opening_permit = self.protocol_set.try_get_permit().ok_or(Error::ConnectionClosed)?;
        let (substream, handle) = WebRtcSubstream::new();
        let substream = Substream::new_webrtc(self.peer, substream_id, substream, codec);
        let keep_alive =
            protocols.get(&protocol).expect("negotiated protocol to be one of the keys");
        let lifetime_permit = keep_alive.then(|| opening_permit.clone());

        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.peer,
            ?channel_id,
            ?substream_id,
            ?protocol,
            "inbound substream opened",
        );

        self.protocol_set
            .report_substream_open(
                self.peer,
                protocol.clone(),
                Direction::Inbound,
                substream,
                opening_permit,
            )
            .await
            .map(|_| Some((substream_id, handle, lifetime_permit)))
            .map_err(Into::into)
    }

    /// Handle data received to an opening outbound channel.
    ///
    /// When an outbound channel is opened, the first message the local node sends it the
    /// `multistream-select` handshake which contains the protocol (and any fallbacks for that
    /// protocol) that the local node wants to use to negotiate for the channel. When a message is
    /// received from a remote peer for a channel in state [`ChannelState::OutboundOpening`], parse
    /// the `multistream-select` handshake response. The response either contains a rejection which
    /// causes the substream to be closed, a partial response, or a full response. If a partial
    /// response is heard, e.g., only the header line is received, the handshake cannot be concluded
    /// and the channel is placed back in the [`ChannelState::OutboundOpening`] state to wait for
    /// the rest of the handshake. If a full response is received (or rest of the partial response),
    /// the protocol confirmation is verified and the substream is reported to the protocol.
    ///
    /// If the substream fails to open for whatever reason, since this is an outbound substream,
    /// the protocol is notified of the failure.
    async fn on_outbound_opening_channel_data(
        &mut self,
        channel_id: ChannelId,
        data: Bytes,
        mut dialer_state: WebRtcDialerState,
        context: ChannelContext,
    ) -> Result<Option<(SubstreamId, SubstreamHandle)>, SubstreamError> {
        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.peer,
            ?channel_id,
            data_len = ?data.len(),
            "handle opening outbound substream",
        );

        let rtc_message = WebRtcMessage::decode(&data)
            .map_err(|err| SubstreamError::NegotiationError(err.into()))?;
        let message = rtc_message.payload.ok_or(SubstreamError::NegotiationError(
            ParseError::InvalidData.into(),
        ))?;

        let protocol = match dialer_state.register_response(message)? {
            HandshakeResult::Succeeded(protocol) => protocol,
            HandshakeResult::NotReady => {
                tracing::trace!(
                    target: LOG_TARGET,
                    peer = ?self.peer,
                    ?channel_id,
                    "multistream-select handshake not ready",
                );

                self.channels.insert(
                    channel_id,
                    ChannelState::OutboundOpening {
                        context,
                        dialer_state,
                    },
                );

                return Ok(None);
            }
            HandshakeResult::Rejected => match dialer_state.propose_next_fallback() {
                Ok(Some(message)) => {
                    tracing::trace!(
                        target: LOG_TARGET,
                        peer = ?self.peer,
                        ?channel_id,
                        "protocol rejected, trying next fallback",
                    );

                    let message = WebRtcMessage::encode(message, None);

                    self.write(channel_id, message).map_err(|_| {
                        SubstreamError::NegotiationError(NegotiationError::Failed.into())
                    })?;

                    self.channels.insert(
                        channel_id,
                        ChannelState::OutboundOpening {
                            context,
                            dialer_state,
                        },
                    );

                    return Ok(None);
                }
                Ok(None) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        peer = ?self.peer,
                        ?channel_id,
                        "all protocols rejected by remote peer",
                    );

                    return Err(SubstreamError::NegotiationError(
                        NegotiationError::Failed.into(),
                    ));
                }
                Err(e) => {
                    tracing::trace!(
                        target: LOG_TARGET,
                        peer = ?self.peer,
                        ?channel_id,
                        ?e,
                        "dialer failed proposing next fallback",
                    );

                    return Err(SubstreamError::NegotiationError(
                        NegotiationError::Failed.into(),
                    ));
                }
            },
        };

        let ChannelContext {
            substream_id,
            opening_permit,
            ..
        } = context;
        let codec = self.protocol_set.protocol_codec(&protocol);
        let (substream, handle) = WebRtcSubstream::new();
        let substream = Substream::new_webrtc(self.peer, substream_id, substream, codec);

        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.peer,
            ?channel_id,
            ?substream_id,
            ?protocol,
            "outbound substream opened",
        );

        self.protocol_set
            .report_substream_open(
                self.peer,
                protocol.clone(),
                Direction::Outbound(substream_id),
                substream,
                opening_permit,
            )
            .await
            .map(|_| Some((substream_id, handle)))
    }

    /// Handle data received from an open channel.
    async fn on_open_channel_data(
        &mut self,
        channel_id: ChannelId,
        data: Bytes,
    ) -> crate::Result<()> {
        // Decode errors are not recoverable.
        let message = WebRtcMessage::decode(&data)?;

        tracing::debug!(
            target: LOG_TARGET,
            peer = ?self.peer,
            ?channel_id,
            flag = ?message.flag,
            data_len = message.payload.as_ref().map_or(0usize, |payload| payload.len()),
            "handle inbound message on open channel",
        );

        self.handles
            .get_mut(&channel_id)
            .ok_or_else(|| {
                tracing::warn!(
                    target: LOG_TARGET,
                    peer = ?self.peer,
                    ?channel_id,
                    "data received from an unknown channel",
                );
                debug_assert!(false);
                Error::InvalidState
            })?
            .on_message(message)
            .await
    }

    /// Handle data received from a channel.
    ///
    /// Bytes are accumulated in a per-channel buffer and only handed to the per-state
    /// dispatcher once a complete `varint length ++ protobuf body` frame is available.
    ///
    /// This handles peers (go-libp2p's pbio writer) that split varint and body
    /// across two SCTP messages, while remaining a no-op for peers that send the whole
    /// frame in one message (smoldot).
    async fn on_inbound_data(&mut self, channel_id: ChannelId, data: Vec<u8>) -> crate::Result<()> {
        tracing::debug!(
            target: LOG_TARGET,
            peer = ?self.peer,
            ?channel_id,
            data_len = data.len(),
            channel_state = ?self.channels.get(&channel_id),
            "received channel data",
        );

        self.recv_buffers.entry(channel_id).or_default().extend_from_slice(&data);

        loop {
            let Some(buffer) = self.recv_buffers.get_mut(&channel_id) else {
                return Ok(());
            };

            let Some(body) = extract_framed_message(buffer)? else {
                return Ok(());
            };

            self.dispatch_framed_message(channel_id, body).await?;
            // If the channel was closed/removed during dispatch, stop draining its buffer.
            if !self.channels.contains_key(&channel_id) {
                return Ok(());
            }
        }
    }

    /// Dispatch a single reassembled protobuf body to the per-channel-state handler.
    async fn dispatch_framed_message(
        &mut self,
        channel_id: ChannelId,
        data: Bytes,
    ) -> crate::Result<()> {
        let Some(state) = self.channels.remove(&channel_id) else {
            tracing::warn!(
                target: LOG_TARGET,
                peer = ?self.peer,
                ?channel_id,
                "data received over a channel that doesn't exist",
            );
            debug_assert!(false);
            return Err(Error::InvalidState);
        };

        match state {
            ChannelState::InboundOpening { header_received } => {
                match self.on_inbound_opening_channel_data(channel_id, data, header_received).await
                {
                    Ok(Some((substream_id, handle, lifetime_permit))) => {
                        self.opening_deadlines.remove(&channel_id);
                        self.handles.insert(channel_id, handle);
                        self.channels.insert(
                            channel_id,
                            ChannelState::Open {
                                substream_id,
                                channel_id,
                                lifetime_permit,
                            },
                        );
                    }
                    Ok(None) => {
                        // Header has been exchanged after any successful round.
                        self.channels.insert(
                            channel_id,
                            ChannelState::InboundOpening {
                                header_received: true,
                            },
                        );
                    }
                    Err(error) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            peer = ?self.peer,
                            ?channel_id,
                            ?error,
                            "failed to handle opening inbound substream",
                        );

                        self.opening_deadlines.remove(&channel_id);
                        self.channels.insert(channel_id, ChannelState::Closing);
                        self.rtc.direct_api().close_data_channel(channel_id);
                    }
                }
            }
            ChannelState::OutboundOpening {
                context,
                dialer_state,
            } => {
                let protocol = context.protocol.clone();
                let substream_id = context.substream_id;
                let lifetime_permit = context.keep_alive.then(|| context.opening_permit.clone());

                match self
                    .on_outbound_opening_channel_data(channel_id, data, dialer_state, context)
                    .await
                {
                    Ok(Some((substream_id, handle))) => {
                        self.opening_deadlines.remove(&channel_id);
                        self.handles.insert(channel_id, handle);
                        self.channels.insert(
                            channel_id,
                            ChannelState::Open {
                                substream_id,
                                channel_id,
                                lifetime_permit,
                            },
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            peer = ?self.peer,
                            ?channel_id,
                            ?error,
                            "failed to handle opening outbound substream",
                        );

                        self.opening_deadlines.remove(&channel_id);
                        let _ = self
                            .protocol_set
                            .report_substream_open_failure(protocol, substream_id, error)
                            .await;

                        self.rtc.direct_api().close_data_channel(channel_id);
                        self.channels.insert(channel_id, ChannelState::Closing);
                    }
                }
            }
            ChannelState::Open {
                substream_id,
                channel_id,
                lifetime_permit,
            } => match self.on_open_channel_data(channel_id, data).await {
                Ok(()) => {
                    self.channels.insert(
                        channel_id,
                        ChannelState::Open {
                            substream_id,
                            channel_id,
                            lifetime_permit,
                        },
                    );
                }
                Err(error) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        peer = ?self.peer,
                        ?channel_id,
                        ?error,
                        "failed to handle data for an open channel",
                    );

                    self.rtc.direct_api().close_data_channel(channel_id);
                    self.channels.insert(channel_id, ChannelState::Closing);
                    self.handles.remove(&channel_id);
                }
            },
            ChannelState::Closing => {
                tracing::debug!(
                    target: LOG_TARGET,
                    peer = ?self.peer,
                    ?channel_id,
                    "channel closing, discarding received data",
                );
                self.channels.insert(channel_id, ChannelState::Closing);
            }
        }

        Ok(())
    }

    /// Handle outbound data with optional flag.
    fn on_outbound_data(
        &mut self,
        channel_id: ChannelId,
        data: Vec<u8>,
        flag: Option<Flag>,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.peer,
            ?channel_id,
            data_len = ?data.len(),
            ?flag,
            "send data",
        );

        let message = WebRtcMessage::encode(data, flag);
        self.write(channel_id, message)
    }

    /// Open outbound substream.
    fn on_open_substream(
        &mut self,
        protocol: ProtocolName,
        fallback_names: Vec<ProtocolName>,
        substream_id: SubstreamId,
        opening_permit: Permit,
        keep_alive: SubstreamKeepAlive,
    ) {
        let channel_id = self.rtc.direct_api().create_data_channel(ChannelConfig {
            label: "".to_string(),
            ordered: false,
            reliability: Default::default(),
            negotiated: None,
            protocol: protocol.to_string(),
        });

        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.peer,
            ?channel_id,
            ?substream_id,
            ?protocol,
            ?fallback_names,
            "open data channel",
        );

        // Start the negotiation deadline now; it is carried through DCEP and
        // `multistream-select`, across the `pending_outbound` -> `OutboundOpening` transition.
        self.opening_deadlines.insert(channel_id, Self::opening_deadline());
        self.pending_outbound.insert(
            channel_id,
            ChannelContext {
                protocol,
                fallback_names,
                substream_id,
                opening_permit,
                keep_alive,
            },
        );
    }

    /// Connection to peer has been closed.
    async fn on_connection_closed(&mut self) {
        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.peer,
            "connection closed",
        );

        let mut report_failure = async |context: &ChannelContext| {
            let _ = self
                .protocol_set
                .report_substream_open_failure(
                    context.protocol.clone(),
                    context.substream_id,
                    SubstreamError::ConnectionClosed,
                )
                .await;
        };

        // Drain pending outbound opens (data channel not yet acked).
        for (_, context) in self.pending_outbound.drain() {
            report_failure(&context).await;
        }

        // Drain channels still in OutboundOpening (multistream-select in flight).
        for (_, state) in self.channels.drain() {
            if let ChannelState::OutboundOpening { context, .. } = state {
                report_failure(&context).await;
            }
        }

        let _ = self
            .protocol_set
            .report_connection_closed(self.peer, self.endpoint.connection_id())
            .await;
    }

    /// Start the connection event loop without notifying protocols.
    pub async fn run_event_loop(mut self) {
        loop {
            // poll output until we get a timeout
            let output = match self.rtc.poll_output() {
                Ok(output) => output,
                Err(error) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        peer = ?self.peer,
                        ?error,
                        "poll_output failed, closing connection",
                    );
                    return self.on_connection_closed().await;
                }
            };
            let str0m_timeout = match output {
                Output::Timeout(v) => v,
                Output::Transmit(v) => {
                    tracing::trace!(
                        target: LOG_TARGET,
                        peer = ?self.peer,
                        datagram_len = ?v.contents.len(),
                        "transmit data",
                    );

                    if let Err(error) = self.socket.try_send_to(&v.contents, v.destination) {
                        if error.kind() == std::io::ErrorKind::WouldBlock {
                            tracing::trace!(
                                target: LOG_TARGET,
                                peer = ?self.peer,
                                destination = ?v.destination,
                                "UDP send buffer full, dropping datagram (str0m will retransmit)",
                            );
                        } else {
                            tracing::debug!(
                                target: LOG_TARGET,
                                peer = ?self.peer,
                                destination = ?v.destination,
                                ?error,
                                "failed to send datagram, closing connection",
                            );
                            return self.on_connection_closed().await;
                        }
                    }

                    continue;
                }
                Output::Event(v) => match v {
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        tracing::trace!(
                            target: LOG_TARGET,
                            peer = ?self.peer,
                            "ice connection state changed to closed",
                        );
                        return self.on_connection_closed().await;
                    }
                    Event::ChannelOpen(channel_id, name) => {
                        if let Err(error) = self.on_channel_opened(channel_id, name).await {
                            tracing::debug!(
                                target: LOG_TARGET,
                                peer = ?self.peer,
                                ?channel_id,
                                ?error,
                                "failed to handle opened channel",
                            );
                        }

                        continue;
                    }
                    Event::ChannelClose(channel_id) => {
                        // This event is emitted once the rtc instance
                        // completes the call to `close_data_channel(channel_id)`.
                        if let Err(error) = self.on_channel_closed(channel_id).await {
                            tracing::debug!(
                                target: LOG_TARGET,
                                peer = ?self.peer,
                                ?channel_id,
                                ?error,
                                "failed to handle closed channel",
                            );
                        }

                        continue;
                    }
                    Event::ChannelData(info) => {
                        if let Err(error) = self.on_inbound_data(info.id, info.data).await {
                            tracing::debug!(
                                target: LOG_TARGET,
                                peer = ?self.peer,
                                channel_id = ?info.id,
                                ?error,
                                "failed to handle channel data",
                            );
                        }

                        continue;
                    }
                    Event::ChannelBufferedAmountLow(_channel_id) => {
                        let channel_ids: Vec<_> = self.pending_messages.keys().cloned().collect();
                        for channel_id in channel_ids {
                            let _ = self.write_pending(channel_id);
                        }
                        continue;
                    }
                    event => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            peer = ?self.peer,
                            ?event,
                            "unhandled event",
                        );
                        continue;
                    }
                },
            };

            // About to block: sweep stalled opening channels and cap the sleep at the next
            // negotiation deadline so the following one is swept promptly even when idle.
            let deadline = if self.opening_deadlines.is_empty() {
                str0m_timeout
            } else {
                self.sweep_opening_timeouts(Instant::now()).await;
                self.opening_deadlines.earliest().map_or(str0m_timeout, |open_deadline| {
                    str0m_timeout.min(open_deadline)
                })
            };

            tokio::select! {
                biased;
                datagram = self.dgram_rx.recv() => match datagram {
                    Some(datagram) => {
                        let contents = match datagram.as_slice().try_into() {
                            Ok(contents) => contents,
                            Err(error) => {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    peer = ?self.peer,
                                    ?error,
                                    datagram_len = datagram.len(),
                                    "failed to parse inbound datagram, closing connection",
                                );

                                return self.on_connection_closed().await;
                            }
                        };

                        let input = Input::Receive(
                            Instant::now(),
                            Receive {
                                proto: Str0mProtocol::Udp,
                                source: self.peer_address,
                                destination: self.local_address,
                                contents,
                            },
                        );

                        if let Err(error) = self.rtc.handle_input(input) {
                            tracing::debug!(
                                target: LOG_TARGET,
                                peer = ?self.peer,
                                ?error,
                                "str0m rejected inbound datagram, closing connection",
                            );
                            return self.on_connection_closed().await;
                        }
                    }
                    None => {
                        tracing::trace!(
                            target: LOG_TARGET,
                            peer = ?self.peer,
                            "read `None` from `dgram_rx`",
                        );
                        return self.on_connection_closed().await;
                    }
                },
                event = self.handles.next() => match event {
                    None => unreachable!(),
                    Some((channel_id, None)) => {
                        tracing::trace!(
                            target: LOG_TARGET,
                            peer = ?self.peer,
                            ?channel_id,
                            "channel closed",
                        );

                        self.rtc.direct_api().close_data_channel(channel_id);
                        self.channels.insert(channel_id, ChannelState::Closing);
                        self.handles.remove(&channel_id);
                    }
                    Some((channel_id, Some(Message { payload, flag }))) => {
                        if let Err(error) = self.on_outbound_data(channel_id, payload, flag) {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?channel_id,
                                ?flag,
                                ?error,
                                "failed to send data to remote peer",
                            );

                            self.rtc.direct_api().close_data_channel(channel_id);
                            self.channels.insert(channel_id, ChannelState::Closing);
                            self.handles.remove(&channel_id);
                        }
                    }
                },
                command = self.protocol_set.next() => match command {
                    None | Some(ProtocolCommand::ForceClose) => {
                        tracing::trace!(
                            target: LOG_TARGET,
                            peer = ?self.peer,
                            ?command,
                            "`ProtocolSet` instructed to close connection",
                        );
                        return self.on_connection_closed().await;
                    }
                    Some(ProtocolCommand::OpenSubstream {
                        protocol,
                        fallback_names,
                        substream_id,
                        permit,
                        keep_alive,
                        connection_id: _,
                    }) => {
                        // Check if the connection is still healthy before opening new substreams.
                        // This prevents panics when trying to open channels on a shutting-down
                        // SCTP association.
                        if !self.rtc.is_alive() || !self.rtc.is_connected() {
                            tracing::debug!(
                                target: LOG_TARGET,
                                peer = ?self.peer,
                                ?protocol,
                                is_alive = self.rtc.is_alive(),
                                is_connected = self.rtc.is_connected(),
                                "rejecting substream open: connection not healthy",
                            );
                            continue;
                        }
                        self.on_open_substream(
                            protocol,
                            fallback_names,
                            substream_id,
                            permit,
                            keep_alive,
                        );
                    }
                },
                _ = tokio::time::sleep(deadline.saturating_duration_since(Instant::now())) => {
                    if let Err(error) = self.rtc.handle_input(Input::Timeout(Instant::now())) {
                        tracing::debug!(
                            target: LOG_TARGET,
                            peer = ?self.peer,
                            ?error,
                            "str0m rejected timeout input, closing connection",
                        );

                        return self.on_connection_closed().await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn earliest_returns_the_minimum_deadline() {
        let mut deadlines = OpeningDeadlines::new();
        assert!(deadlines.is_empty());
        assert_eq!(deadlines.earliest(), None);

        let now = Instant::now();
        deadlines.insert(1usize, now + Duration::from_secs(5));
        deadlines.insert(2usize, now + Duration::from_secs(1));
        deadlines.insert(3usize, now + Duration::from_secs(3));

        assert!(!deadlines.is_empty());
        assert_eq!(deadlines.earliest(), Some(now + Duration::from_secs(1)));
    }

    #[test]
    fn insert_preserves_the_original_deadline() {
        // First deadline wins: a peer can't extend its window by re-inserting.
        let mut deadlines = OpeningDeadlines::new();
        let now = Instant::now();

        deadlines.insert(1usize, now + Duration::from_secs(1));
        deadlines.insert(1usize, now + Duration::from_secs(100));

        assert_eq!(deadlines.earliest(), Some(now + Duration::from_secs(1)));
    }

    #[test]
    fn drain_expired_only_removes_passed_deadlines() {
        let mut deadlines = OpeningDeadlines::new();
        let now = Instant::now();

        deadlines.insert(1usize, now - Duration::from_secs(1)); // expired
        deadlines.insert(2usize, now); // expired (deadline == now)
        deadlines.insert(3usize, now + Duration::from_secs(5)); // still pending

        let mut expired = deadlines.drain_expired(now);
        expired.sort_unstable();
        assert_eq!(expired, vec![1usize, 2usize]);

        // The pending channel is retained and remains the earliest deadline.
        assert_eq!(deadlines.earliest(), Some(now + Duration::from_secs(5)));

        // Draining again before the remaining deadline yields nothing.
        assert!(deadlines.drain_expired(now).is_empty());
    }

    #[test]
    fn remove_stops_tracking() {
        let mut deadlines = OpeningDeadlines::new();
        let now = Instant::now();

        deadlines.insert(1usize, now + Duration::from_secs(1));
        deadlines.remove(&1usize);

        assert!(deadlines.is_empty());
        assert_eq!(deadlines.earliest(), None);
        assert!(deadlines.drain_expired(now + Duration::from_secs(10)).is_empty());
    }
}
