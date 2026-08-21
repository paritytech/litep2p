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
    error::{Error, SubstreamError},
    multistream_select::{
        webrtc_listener_negotiate, HandshakeResult, ListenerSelectResult, NegotiationError,
        WebRtcDialerState,
    },
    protocol::{Direction, Permit, ProtocolCommand, ProtocolSet, SubstreamKeepAlive},
    substream::Substream,
    transport::{
        webrtc::{
            schema::webrtc::message::Flag,
            socket::WebRtcSocket,
            substream::{Message, Substream as WebRtcSubstream, SubstreamHandle},
            util::{extract_framed_message, WebRtcMessage},
            AddressPair,
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
use tokio::sync::mpsc::Receiver;

use std::{
    collections::{HashMap, HashSet, VecDeque},
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

            let Some((key, _)) = self.handles.get_index(index) else {
                tracing::debug!(
                    target: LOG_TARGET,
                    index,
                    num_handles = self.handles.len(),
                    "substream handles index out of bounds",
                );
                return Poll::Ready(None);
            };

            if !self.pending.contains(key) {
                let Some((key, stream)) = self.handles.get_index_mut(index) else {
                    tracing::debug!(
                        target: LOG_TARGET,
                        index,
                        num_handles = self.handles.len(),
                        "substream handles index out of bounds",
                    );
                    return Poll::Ready(None);
                };

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

    /// Addresses of the session.
    addrs: AddressPair,

    /// Transport socket.
    socket: Arc<WebRtcSocket>,

    /// RX channel for receiving datagrams from the transport.
    dgram_rx: Receiver<Vec<u8>>,

    /// Pending outbound channels.
    pending_outbound: HashMap<ChannelId, ChannelContext>,

    /// Pending outbound messages,
    /// at most [`MAX_PENDING_PER_CHANNEL`] per channel.
    pending_messages: HashMap<ChannelId, VecDeque<Vec<u8>>>,

    /// Deadlines of the opening phase of channels.
    opening_deadlines: VecDeque<(ChannelId, Instant)>,

    /// Channels closed by time out.
    ///
    /// Need by [`Self::on_channel_closed`], so that
    /// `NegotiationError::Timeout` can be reported as error.
    opening_timed_out: HashSet<ChannelId>,

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
}

impl WebRtcConnection {
    /// Create new [`WebRtcConnection`].
    pub fn new(
        rtc: Rtc,
        peer: PeerId,
        addrs: AddressPair,
        socket: Arc<WebRtcSocket>,
        protocol_set: ProtocolSet,
        endpoint: Endpoint,
        dgram_rx: Receiver<Vec<u8>>,
    ) -> Self {
        Self {
            rtc,
            protocol_set,
            peer,
            addrs,
            socket,
            endpoint,
            dgram_rx,
            pending_outbound: HashMap::new(),
            pending_messages: HashMap::new(),
            opening_deadlines: VecDeque::new(),
            opening_timed_out: HashSet::new(),
            channels: HashMap::new(),
            handles: SubstreamHandleSet::new(),
            recv_buffers: HashMap::new(),
        }
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

            self.add_opening_deadline(channel_id);
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

        let opening_timed_out = self.opening_timed_out.remove(&channel_id);
        let substream_error = || {
            if opening_timed_out {
                SubstreamError::NegotiationError(crate::error::NegotiationError::Timeout)
            } else {
                SubstreamError::ConnectionClosed
            }
        };

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
                    substream_error(),
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
                    substream_error(),
                )
                .await;
        }

        self.pending_messages.remove(&channel_id);
        self.handles.remove(&channel_id);
        self.recv_buffers.remove(&channel_id);

        Ok(())
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
        let WebRtcMessage {
            payload: Some(payload),
            flag: None,
        } = WebRtcMessage::decode(&data)
            .map_err(|err| SubstreamError::NegotiationError(err.into()))?
        else {
            tracing::debug!(
                target: LOG_TARGET,
                peer = ?self.peer,
                ?channel_id,
                "non-payload frame during inbound opening, closing channel"
            );
            return Err(Error::ConnectionClosed);
        };

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
        let keep_alive = protocols
            .get(&protocol)
            .ok_or(Error::ProtocolNotSupported(protocol.to_string()))?;
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

        // Decode errors are not recoverable.
        let WebRtcMessage {
            payload: Some(message),
            flag: None,
        } = WebRtcMessage::decode(&data)
            .map_err(|err| SubstreamError::NegotiationError(err.into()))?
        else {
            tracing::debug!(
                target: LOG_TARGET,
                peer = ?self.peer,
                ?channel_id,
                "non-payload frame during outbound opening, closing channel"
            );
            return Err(SubstreamError::ConnectionClosed);
        };

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

        // Drop data for channels we never opened. Creating a `recv_buffers` entry here would
        // leak it for the life of the connection: `on_channel_closed` only runs for channels
        // that were actually opened.
        if !self.channels.contains_key(&channel_id) {
            tracing::debug!(
                target: LOG_TARGET,
                peer = ?self.peer,
                ?channel_id,
                "dropping data for unknown channel",
            );
            return Ok(());
        }

        self.recv_buffers.entry(channel_id).or_default().extend_from_slice(&data);

        loop {
            let Some(buffer) = self.recv_buffers.get_mut(&channel_id) else {
                return Ok(());
            };

            let Some(body) = (match extract_framed_message(buffer) {
                Ok(value) => value,
                Err(error) => {
                    // Unparseable framing can never become parseable by appending more
                    // bytes. Drop the reassembly buffer and tear the channel down instead
                    // of retaining bytes that would fail again on every append.
                    self.recv_buffers.remove(&channel_id);
                    self.rtc.direct_api().close_data_channel(channel_id);
                    if matches!(
                        self.channels.get(&channel_id),
                        Some(ChannelState::Open { .. })
                    ) {
                        self.handles.remove(&channel_id);
                    }
                    if let Some(state) = self.channels.get_mut(&channel_id) {
                        *state = ChannelState::Closing;
                    }
                    return Err(error.into());
                }
            }) else {
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

    /// Handle one item yielded by the substream handle set.
    ///
    /// `None` means the handle finished, `Some` is a message to forward to the peer. Either
    /// way a failure closes the channel and drops the handle.
    ///
    /// Split out of [`Self::run_event_loop()`] so that `FuzzConnection::poll_handles()` drives
    /// this exact code rather than a copy of it.
    fn on_handle_message(&mut self, channel_id: ChannelId, message: Option<Message>) {
        let failed = match message {
            None => {
                tracing::trace!(
                    target: LOG_TARGET,
                    peer = ?self.peer,
                    ?channel_id,
                    "channel closed",
                );

                true
            }
            Some(Message { payload, flag }) => match self.on_outbound_data(channel_id, payload, flag)
            {
                Ok(()) => false,
                Err(error) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?channel_id,
                        ?flag,
                        ?error,
                        "failed to send data to remote peer",
                    );

                    true
                }
            },
        };

        if failed {
            self.rtc.direct_api().close_data_channel(channel_id);
            self.channels.insert(channel_id, ChannelState::Closing);
            self.handles.remove(&channel_id);
        }
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

        self.add_opening_deadline(channel_id);
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
            let mut timeout = match output {
                Output::Timeout(v) => v,
                Output::Transmit(v) => {
                    tracing::trace!(
                        target: LOG_TARGET,
                        peer = ?self.peer,
                        datagram_len = ?v.contents.len(),
                        "transmit data",
                    );

                    if let Err(error) =
                        self.socket.try_send_to(&v.contents, v.destination, self.addrs.local.ip())
                    {
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
                    Event::Closed => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            peer = ?self.peer,
                            "connection has been closed",
                        );
                        return self.on_connection_closed().await;
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

            // If nothing has expired yet, this is a no-op.
            self.drain_opening_deadlines().await;

            // Update the timeout by comparing it against the next opening-channel deadline.
            // This way, the next iteration will drain the next deadline.
            timeout = self
                .opening_deadlines
                .front()
                .map_or(timeout, |(_, deadline)| std::cmp::min(timeout, *deadline));

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
                                source: self.addrs.remote,
                                destination: self.addrs.local,
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
                    None => {
                        tracing::warn!(
                            target: LOG_TARGET, peer = ?self.peer, "substream handle set unexpectedly terminated"
                        );
                        return self.on_connection_closed().await;
                    },
                    Some((channel_id, message)) => self.on_handle_message(channel_id, message),
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

                            // This substream isn't tracked in `pending_outbound`/`channels` yet, so report
                            // the failure here. Other in-flight substreams are reported during connection close.
                            let _ = self
                                .protocol_set
                                .report_substream_open_failure(
                                    protocol,
                                    substream_id,
                                    SubstreamError::ConnectionClosed,
                                )
                                .await;
                            return self.on_connection_closed().await;
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
                _ = tokio::time::sleep(timeout.saturating_duration_since(Instant::now())) => {
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

    /// Register an opening-phase deadline for `channel_id`.
    fn add_opening_deadline(&mut self, channel_id: ChannelId) {
        self.opening_deadlines
            .push_back((channel_id, Instant::now() + SUBSTREAM_OPEN_TIMEOUT));
    }

    /// Close channels whose opening phase exceeded [`SUBSTREAM_OPEN_TIMEOUT`],
    /// lazily dropping entries for channels that already opened or closed.
    ///
    /// If the channel has an SCTP stream, its state is deliberately left untouched so
    /// the next `on_channel_closed` call will properly handle it.
    async fn drain_opening_deadlines(&mut self) {
        loop {
            let channel_id = match self.opening_deadlines.front() {
                Some((channel_id, deadline)) if Instant::now() >= *deadline => *channel_id,
                _ => break,
            };

            self.opening_deadlines.pop_front();
            if !self.pending_outbound.contains_key(&channel_id)
                && !matches!(
                    self.channels.get(&channel_id),
                    Some(ChannelState::InboundOpening { .. })
                        | Some(ChannelState::OutboundOpening { .. })
                )
            {
                continue;
            };

            tracing::debug!(
                target: LOG_TARGET,
                peer = ?self.peer,
                ?channel_id,
                "opening substream reached deadline, shutting down",
            );

            self.opening_timed_out.insert(channel_id);
            self.rtc.direct_api().close_data_channel(channel_id);

            if let Some(ChannelState::OutboundOpening { context, .. }) =
                self.channels.insert(channel_id, ChannelState::Closing)
            {
                // This requires to be done eagerly becase the state is being update
                // to discard each message that will arrive to this channel between
                // now and its closure, but still higher layers needs to be updated
                // with the closure of the protocol.
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
                        SubstreamError::NegotiationError(crate::error::NegotiationError::Timeout),
                    )
                    .await;
            }
        }
    }
}

/// Test/fuzz scaffolding for [`WebRtcConnection`].
///
/// [`WebRtcConnection::new()`] needs an `Rtc`, a bound socket, a `ProtocolSet` and an
/// `Endpoint`, none of which are constructible from outside this crate — which is why this
/// file had no way to be exercised in isolation. This block builds all of them from
/// deterministic inputs and exposes the inbound entry points, so `fuzz/webrtc-state` can
/// drive the channel state machine directly.
///
/// # What is and is not reachable
///
/// The connection is created without a DTLS handshake, and the data channels are created with
/// `negotiated: None`, so str0m never assigns them an SCTP stream id. `Rtc::channel()`
/// therefore returns `None` and every `write()` returns [`Error::ChannelDoesntExist`].
///
/// The consequence is sharper than "negotiation cannot complete". In
/// [`Self::on_inbound_opening_channel_data()`] the multistream-select response is written
/// *before* the accept/reject/pending branch, so **every** outcome — `Accepted`, `Rejected`
/// and `PendingProtocol` alike — fails at that write and moves the channel to
/// [`ChannelState::Closing`]. A channel opened with [`FuzzConnection::open_channel()`] is
/// effectively a one-frame channel: the first complete frame ends it, and later frames are
/// discarded by the `Closing` arm.
///
/// - **Reachable through [`FuzzConnection::open_channel()`]:** the per-channel `recv_buffers`
///   reassembly loop, frame extraction across interleaved channels, channel-state lookup and
///   removal, close-time cleanup, and one pass through `webrtc_listener_negotiate`. A channel
///   holding a permanently incomplete frame never dispatches, so it never needs a write.
/// - **Reachable through [`FuzzConnection::open_negotiated_channel()`]:** the
///   [`ChannelState::Open`] path, which is what the write failure otherwise blocks. That entry
///   point installs a substream and its handle directly, so inbound frames reach
///   `on_open_channel_data` and `SubstreamHandle::on_message`, and
///   [`FuzzConnection::poll_handles()`] drives the [`SubstreamHandleSet`] round-robin.
/// - **Not reachable:** the back-and-forth negotiation states, because
///   `InboundOpening { header_received: true }` requires the response write to succeed. Any
///   outbound message a handle produces also fails to write and closes its channel, so the
///   `Open` state survives inbound traffic but not outbound.
///
/// Lifting the remaining limits means pairing two `Rtc` instances through a real DTLS/SCTP
/// handshake per iteration; see `fuzz/README.md`.
#[cfg(any(test, feature = "fuzz"))]
pub struct FuzzConnection {
    /// The connection under test.
    ///
    /// Deliberately private: `WebRtcConnection::new` takes `AddressPair` and
    /// `WebRtcSocket`, both crate-internal, so exposing the connection itself would leak
    /// private types into the public API. The harness drives it through the methods below.
    connection: WebRtcConnection,

    /// Data channels opened so far, in creation order, paired with whether this scaffold
    /// still considers them open.
    ///
    /// Entries are never removed, so later indices keep referring to the same channel and a
    /// mutated script stays meaningful. The `ChannelId` outlives the close so a script can
    /// deliver data *after* the close, which is an ordering a peer really can produce.
    channels: Vec<(ChannelId, bool)>,

    /// Local ends of substreams installed by [`Self::open_negotiated_channel()`], indexed in
    /// step with `channels` so one index addresses both. `None` for channels opened through
    /// [`Self::open_channel()`], which have no substream.
    ///
    /// Held so each handle keeps a live peer. Dropping a `Substream` closes its outbound
    /// channel, which makes the handle start half-closing on the next poll and collapses the
    /// `Open` state this exists to reach.
    substreams: Vec<Option<Substream>>,

    /// Held so the connection's datagram receiver does not observe a closed sender.
    _dgram_tx: tokio::sync::mpsc::Sender<Vec<u8>>,

    /// Drained by [`Self::drain_events()`] rather than merely held.
    ///
    /// Both channels have capacity 256 and `ProtocolSet` sends on them with `.await`, so
    /// holding the receivers without reading them would block forever on the 257th event once
    /// any reporting path is reachable — an unattributable fuzzer hang rather than a finding.
    mgr_rx: tokio::sync::mpsc::Receiver<crate::transport::manager::TransportManagerEvent>,
    protocol_rx: tokio::sync::mpsc::Receiver<crate::protocol::InnerTransportEvent>,
}

#[cfg(any(test, feature = "fuzz"))]
impl FuzzConnection {
    /// Build a connection wired to `protocols`, with no DTLS handshake performed.
    pub async fn new(protocols: Vec<ProtocolName>) -> crate::Result<Self> {
        use crate::{
            codec::ProtocolCodec,
            transport::{
                manager::ProtocolContext,
                webrtc::{certificate::DtlsCertificate, socket::WebRtcSocket},
            },
            types::ConnectionId,
        };
        use std::sync::{atomic::AtomicUsize, Arc};
        use str0m::{net::Protocol as Str0mProtocol, Candidate, IceCreds};

        let local = "127.0.0.1:4242".parse().expect("valid socket address");
        let remote = "127.0.0.1:4243".parse().expect("valid socket address");
        let addrs = AddressPair { local, remote };

        // Mirrors `WebRtcTransport::make_rtc`, minus the handshake: ICE-lite, fingerprint
        // verification off (the Noise prologue does the binding), listener role.
        //
        // The certificate is generated once per process rather than per connection.
        // `DtlsCertificate::new` runs a full keypair generation through the crypto provider
        // and dominates the cost of building this scaffold, while the certificate itself is
        // never used, because no DTLS handshake happens. Under a fuzzer's persistent loop
        // that cost is paid on every input. `load` only moves the DER bytes, so reusing them
        // is free and keeps the fingerprint stable across iterations too.
        static DTLS_CERT_DER: std::sync::OnceLock<(Vec<u8>, Vec<u8>)> = std::sync::OnceLock::new();
        let (certificate, private_key) = DTLS_CERT_DER.get_or_init(|| {
            let generated =
                DtlsCertificate::new().expect("DTLS certificate generation to succeed");
            let (certificate, private_key) = generated.as_parts();

            (certificate.clone(), private_key.clone())
        });
        let dtls_cert: str0m::config::DtlsCert =
            DtlsCertificate::load(certificate.clone(), private_key.clone())?.into();
        let mut rtc = Rtc::builder()
            .set_ice_lite(true)
            .set_dtls_cert(dtls_cert)
            .set_fingerprint_verification(false)
            .build(std::time::Instant::now());
        rtc.add_local_candidate(
            Candidate::host(local, Str0mProtocol::Udp).map_err(str0m::RtcError::Ice)?,
        );
        rtc.add_remote_candidate(
            Candidate::host(remote, Str0mProtocol::Udp).map_err(str0m::RtcError::Ice)?,
        );
        let creds = IceCreds {
            ufrag: "fuzzufrag".to_string(),
            pass: "fuzzpassfuzzpass".to_string(),
        };
        rtc.direct_api().set_remote_ice_credentials(creds.clone());
        rtc.direct_api().set_local_ice_credentials(creds);
        rtc.direct_api().set_ice_controlling(false);

        let (protocol_tx, protocol_rx) = tokio::sync::mpsc::channel(256);
        let (mgr_tx, mgr_rx) = tokio::sync::mpsc::channel(256);
        let (_dgram_tx, dgram_rx) = tokio::sync::mpsc::channel(256);

        let protocols = protocols
            .into_iter()
            .map(|protocol| {
                (
                    protocol,
                    ProtocolContext {
                        codec: ProtocolCodec::Identity(0xffff),
                        tx: protocol_tx.clone(),
                        fallback_names: Vec::new(),
                        keep_alive: SubstreamKeepAlive::No,
                    },
                )
            })
            .collect();

        let connection_id = ConnectionId::from(0usize);
        let protocol_set = ProtocolSet::new(
            connection_id,
            mgr_tx,
            Arc::new(AtomicUsize::new(0)),
            protocols,
        );

        // The socket is a placeholder, like the certificate above. The logical `addrs` are
        // hardcoded and no I/O ever runs on it, because there is no DTLS handshake. Binding it per
        // input cost a syscall and a fresh ephemeral port every iteration, so bind once per process
        // and share the `Arc`. That also turns a transient bind failure into a single setup failure
        // rather than one crash per input.
        static SOCKET: std::sync::OnceLock<Arc<WebRtcSocket>> = std::sync::OnceLock::new();
        let socket = match SOCKET.get() {
            Some(socket) => socket.clone(),
            None => {
                let socket =
                    Arc::new(WebRtcSocket::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await?)?);
                let _ = SOCKET.set(socket.clone());
                socket
            }
        };

        let connection = WebRtcConnection::new(
            rtc,
            PeerId::random(),
            addrs,
            socket,
            protocol_set,
            Endpoint::listener(multiaddr::Multiaddr::empty(), connection_id),
            dgram_rx,
        );

        Ok(Self {
            connection,
            channels: Vec::new(),
            substreams: Vec::new(),
            _dgram_tx,
            mgr_rx,
            protocol_rx,
        })
    }

    /// Open an inbound data channel and register it, returning its index.
    ///
    /// The channel lands in [`ChannelState::InboundOpening`], so its first complete frame runs
    /// one pass of `webrtc_listener_negotiate` and then closes it. Use
    /// [`Self::open_negotiated_channel()`] to reach the `Open` state instead.
    pub async fn open_channel(&mut self) -> crate::Result<usize> {
        self.drain_events();

        let label = format!("fuzz-{}", self.channels.len());
        let channel_id = self.connection.rtc.direct_api().create_data_channel(ChannelConfig {
            label: label.clone(),
            ordered: true,
            reliability: str0m::channel::Reliability::Reliable,
            negotiated: None,
            protocol: String::new(),
        });

        self.connection.on_channel_opened(channel_id, label).await?;
        self.channels.push((channel_id, true));
        self.substreams.push(None);

        Ok(self.channels.len() - 1)
    }

    /// Feed inbound bytes to the channel at `index`.
    ///
    /// An index that was never opened is a no-op, since there is no `ChannelId` to address.
    /// Data for a *closed* channel is delivered, which is deliberate: `on_inbound_data` now
    /// drops data for a channel with no state of its own, so the post-close ordering question
    /// this scaffold used to sidestep is answered by the code under test rather than by a
    /// guard here.
    pub async fn inbound(&mut self, index: usize, data: Vec<u8>) -> crate::Result<()> {
        self.drain_events();

        let Some((channel_id, _open)) = self.channels.get(index).copied() else {
            return Ok(());
        };

        self.connection.on_inbound_data(channel_id, data).await
    }

    /// Close the channel at `index`, if this scaffold still considers it open.
    pub async fn close_channel(&mut self, index: usize) -> crate::Result<()> {
        self.drain_events();

        let Some((channel_id, open)) = self.channels.get_mut(index) else {
            return Ok(());
        };
        if !*open {
            return Ok(());
        }

        *open = false;
        let channel_id = *channel_id;

        self.connection.on_channel_closed(channel_id).await
    }

    /// Install an already-negotiated channel in [`ChannelState::Open`], returning its index.
    ///
    /// This is the entry point that makes the `Open` state reachable at all. Multistream-select
    /// cannot complete in this scaffold, because the response write always fails, so the
    /// post-negotiation setup is performed directly instead: allocate a substream id, take a
    /// permit, create the substream pair, install the handle and set the channel state. That is
    /// the same sequence as the tail of [`Self::on_inbound_opening_channel_data()`].
    ///
    /// Two deliberate divergences from production, both of which keep the `Open` state alive
    /// rather than immediately tearing it down:
    ///
    /// - `report_substream_open` is not called, so the protocol never receives the `Substream`.
    ///   The local end is kept in `substreams` instead. Reporting it would hand ownership to a
    ///   receiver this scaffold has to drain, and dropping it there would close the outbound
    ///   channel and start a half-close on the next poll.
    /// - `lifetime_permit` is `None`, matching the `SubstreamKeepAlive::No` that
    ///   [`Self::new()`] registers for every protocol.
    pub fn open_negotiated_channel(&mut self, protocol: ProtocolName) -> crate::Result<usize> {
        self.drain_events();

        let label = format!("fuzz-open-{}", self.channels.len());
        let channel_id = self.connection.rtc.direct_api().create_data_channel(ChannelConfig {
            label,
            ordered: true,
            reliability: str0m::channel::Reliability::Reliable,
            negotiated: None,
            protocol: String::new(),
        });

        let substream_id = self.connection.protocol_set.next_substream_id();
        let codec = self.connection.protocol_set.protocol_codec(&protocol);
        let _permit = self.connection.protocol_set.try_get_permit().ok_or(Error::ConnectionClosed)?;
        let (substream, handle) = WebRtcSubstream::new();
        let substream =
            Substream::new_webrtc(self.connection.peer, substream_id, substream, codec);

        self.connection.handles.insert(channel_id, handle);
        self.connection.channels.insert(
            channel_id,
            ChannelState::Open {
                substream_id,
                channel_id,
                lifetime_permit: None,
            },
        );

        self.substreams.push(Some(substream));
        self.channels.push((channel_id, true));

        Ok(self.channels.len() - 1)
    }

    /// Poll the substream handle set once and hand whatever it yields to the connection.
    ///
    /// Returns whether the set produced an item. This is the round-robin in
    /// [`SubstreamHandleSet::poll_next()`] — the `index` walk, `pending` skipping and
    /// `swap_remove` reordering — which nothing else in this scaffold reaches, and which is
    /// where an index-out-of-bounds or a starved handle would come from.
    ///
    /// Any message a handle produces is forwarded through [`Self::on_handle_message()`], the
    /// same function `run_event_loop` uses. In this scaffold the forwarding write fails, so a
    /// handle that produces anything gets its channel closed; the polling logic still runs.
    pub fn poll_handles(&mut self) -> bool {
        self.drain_events();

        let mut context = Context::from_waker(futures::task::noop_waker_ref());

        match self.connection.handles.poll_next_unpin(&mut context) {
            Poll::Ready(Some((channel_id, message))) => {
                self.connection.on_handle_message(channel_id, message);
                true
            }
            Poll::Ready(None) | Poll::Pending => false,
        }
    }

    /// Read from the local end of the substream at `index`, returning what was available.
    ///
    /// Proves inbound frames actually traverse the whole path: `on_inbound_data` reassembles,
    /// `on_open_channel_data` decodes, `SubstreamHandle::on_message` forwards, and the payload
    /// lands here. Without this the `Open` state would be reachable but unobservable.
    pub fn read_substream(&mut self, index: usize, len: usize) -> Option<Vec<u8>> {
        use futures::FutureExt;
        use tokio::io::AsyncReadExt;

        let substream = self.substreams.get_mut(index)?.as_mut()?;
        let mut buffer = vec![0u8; len];

        match substream.read(&mut buffer).now_or_never() {
            Some(Ok(read)) => {
                buffer.truncate(read);
                Some(buffer)
            }
            Some(Err(_)) | None => None,
        }
    }

    /// Drain the protocol and manager event queues, returning how many events were dropped.
    ///
    /// Called at the start of every operation, so no reporting path can ever fill a
    /// capacity-256 channel and turn a `.await` send into a hang the fuzzer reports as a
    /// timeout.
    pub fn drain_events(&mut self) -> usize {
        let mut drained = 0;

        while self.protocol_rx.try_recv().is_ok() {
            drained += 1;
        }
        while self.mgr_rx.try_recv().is_ok() {
            drained += 1;
        }

        drained
    }

    /// Number of channels this scaffold has opened, however they were opened.
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Total bytes held across every channel's reassembly buffer.
    ///
    /// Each buffer is individually capped at `MAX_FRAME_SIZE`, but nothing caps the sum,
    /// so this is the quantity a fuzzer should watch grow.
    pub fn buffered_bytes(&self) -> usize {
        self.connection.recv_buffers.values().map(|buffer| buffer.len()).sum()
    }

    /// Number of live reassembly buffers.
    pub fn buffer_count(&self) -> usize {
        self.connection.recv_buffers.len()
    }

    /// Size of the largest single reassembly buffer.
    ///
    /// A buffer only retains bytes while a frame is mid-reassembly, so on the success path
    /// it is bounded by `MAX_FRAME_SIZE` plus a 3-byte varint header. The one way it can
    /// grow past that is a permanent parse error: `on_inbound_data` now drops the buffer and
    /// closes the channel in that case, so a buffer growing past the cap is still a genuine
    /// defect a fuzzer can assert on directly.
    pub fn max_buffered_bytes(&self) -> usize {
        self.connection
            .recv_buffers
            .values()
            .map(|buffer| buffer.len())
            .max()
            .unwrap_or(0)
    }
}

#[cfg(all(test, feature = "webrtc"))]
mod tests {
    use super::*;
    use crate::transport::webrtc::util::MAX_FRAME_SIZE;

    fn protocols() -> Vec<ProtocolName> {
        vec![ProtocolName::from("/ipfs/ping/1.0.0")]
    }

    /// A permanent framing error must drop the reassembly buffer and close the channel.
    ///
    /// `extract_framed_message` deliberately does not consume on error, and the event loop only
    /// logs the failure and continues. Without an explicit drop here, the offending bytes stay at
    /// the head of the buffer and every later byte the peer sends is appended to a buffer that can
    /// never be parsed — unbounded, remotely triggered memory growth.
    #[tokio::test]
    async fn permanent_framing_error_drops_buffer_and_closes_channel() {
        let mut connection = FuzzConnection::new(protocols()).await.expect("scaffold to build");
        let index = connection.open_channel().await.expect("channel to open");

        // Non-minimal varint: decodes to zero, but a single `0x00` is the canonical encoding, so
        // no number of following bytes can make this parse.
        assert!(
            connection.inbound(index, vec![0x80, 0x00]).await.is_err(),
            "a permanent framing error must be reported",
        );
        assert_eq!(connection.buffer_count(), 0, "the reassembly buffer must be dropped");

        // Keep writing, as a peer would. Nothing may accumulate.
        for _ in 0..8 {
            let _ = connection.inbound(index, vec![0xaa; MAX_FRAME_SIZE]).await;
            assert_eq!(connection.buffered_bytes(), 0, "no bytes may be retained");
        }
    }

    /// Data for a channel with no state must not create a reassembly buffer. Nothing would ever
    /// reclaim it, because `on_channel_closed` only runs for channels litep2p knows about.
    #[tokio::test]
    async fn post_close_data_creates_no_buffer() {
        let mut connection = FuzzConnection::new(protocols()).await.expect("scaffold to build");
        let index = connection.open_channel().await.expect("channel to open");
        connection.close_channel(index).await.expect("channel to close");

        connection
            .inbound(index, vec![0xac, 0x02, 0xaa, 0xbb])
            .await
            .expect("post-close data is dropped, not an error");

        assert_eq!(connection.buffer_count(), 0, "a closed channel must not regain a buffer");
    }

    /// An inbound frame on an `Open` channel must traverse the whole path: reassembly, protobuf
    /// decode, `SubstreamHandle::on_message`, and out of the local `Substream`.
    ///
    /// This is what makes `open_negotiated_channel` worth having. If the payload does not arrive,
    /// the `Open` state is nominally reachable but exercises nothing.
    #[tokio::test]
    async fn open_channel_delivers_payload_to_substream() {
        let mut connection = FuzzConnection::new(protocols()).await.expect("scaffold to build");
        let index = connection
            .open_negotiated_channel(ProtocolName::from("/ipfs/ping/1.0.0"))
            .expect("negotiated channel to install");

        let frame = WebRtcMessage::encode(b"payload".to_vec(), None);
        connection.inbound(index, frame).await.expect("frame to be handled");

        assert_eq!(
            connection.read_substream(index, 32).as_deref(),
            Some(&b"payload"[..]),
            "the payload must reach the local end of the substream",
        );
    }

    /// Polling the handle set must reach the round-robin without panicking, including after a
    /// `swap_remove` has reordered it. The hard `assert!` in `SubstreamHandleSet::insert` and the
    /// index walk in `poll_next` are the reason this is worth a test.
    #[tokio::test]
    async fn polling_handles_survives_removal() {
        let mut connection = FuzzConnection::new(protocols()).await.expect("scaffold to build");

        for _ in 0..4 {
            connection
                .open_negotiated_channel(ProtocolName::from("/ipfs/ping/1.0.0"))
                .expect("negotiated channel to install");
        }

        for _ in 0..8 {
            connection.poll_handles();
        }

        // Closing reorders the set through `swap_remove`, so keep polling afterwards.
        connection.close_channel(1).await.expect("channel to close");

        for _ in 0..8 {
            connection.poll_handles();
        }
    }
}
