// Copyright 2024 litep2p developers
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
    transport::webrtc::{
        schema::webrtc::message::Flag,
        util::{WebRtcMessage, MAX_FRAME_SIZE},
        LOG_TARGET,
    },
    Error,
};

use bytes::{Buf, BufMut, BytesMut};
use futures::{task::AtomicWaker, Future, Stream};
use tokio::sync::mpsc::{channel, error::TrySendError, Receiver, Sender};
use tokio_util::sync::PollSender;

use std::{
    marker::PhantomData,
    pin::Pin,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};

/// Timeout for waiting on FIN_ACK after sending FIN.
/// Matches go-libp2p and js-libp2p's 10-second stream close timeout.
const FIN_ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum number of in flight messages between
/// [`Substream`] and [`SubstreamHandle`], in both directions.
const MAX_INFLIGHT_MESSAGES: usize = 256;

/// Substream Message.
#[derive(PartialEq, Eq, Debug)]
pub struct Message {
    pub payload: Vec<u8>,
    pub flag: Option<Flag>,
}

trait AtomicState {
    fn from_u8(raw_state: u8) -> Self;
    fn into_u8(self) -> u8;
}

/// Shared state used to sync `Substream` and `SubstreamHandle`.
#[derive(Clone)]
struct SharedState<T: AtomicState> {
    inner: Arc<Inner>,
    _phantom: PhantomData<T>,
}

struct Inner {
    state: AtomicU8,
    waker: AtomicWaker,
}

impl<T: AtomicState> SharedState<T> {
    fn new(val: T) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: AtomicU8::new(val.into_u8()),
                waker: AtomicWaker::new(),
            }),
            _phantom: Default::default(),
        }
    }

    fn set(&self, new_value: T) {
        self.inner.state.store(new_value.into_u8(), Ordering::Release);
        self.inner.waker.wake();
    }

    fn get(&self) -> T {
        T::from_u8(self.inner.state.load(Ordering::Acquire))
    }

    // NOTE: this method should only be called from a single place,
    // otherwise the waker is re-registered for another context
    // and thus steals the previously registered waker.
    fn register_and_get(&self, cx: &mut Context<'_>) -> T {
        self.inner.waker.register(cx.waker());
        self.get()
    }
}

/// Substream stream.
#[derive(Debug, Clone)]
#[repr(u8)]
enum ChannelState {
    /// Substream is fully open.
    Open = 0,
    /// Set locally when the inbound buffer is exceeded (treated as a DoS).
    ///
    /// A `RESET_STREAM` flag is expected to be sent to the other peer,
    /// after which the channel moves to the `Reset` state.
    InitReset = 1,
    /// Terminal reset state, the stream is abruptly torn down.
    ///
    /// Entered when a RESET_STREAM is received from the peer, or locally after a
    /// reset is initiated (`InitReset`), a FIN_ACK times out, an unexpected
    /// FIN_ACK arrives, or the handle is dropped without a graceful close.
    ///
    /// This preempts both writer and reader state.
    Reset = 2,
}

impl AtomicState for ChannelState {
    fn from_u8(raw_state: u8) -> Self {
        match raw_state {
            0 => ChannelState::Open,
            1 => ChannelState::InitReset,
            2 => ChannelState::Reset,
            // Unreachable in practice: `into_u8` only ever stores a valid variant and the
            // `AtomicU8` only returns a previously-stored byte.
            // Return Reset defensively rather than a panic.
            _ => ChannelState::Reset,
        }
    }

    fn into_u8(self) -> u8 {
        self as u8
    }
}

/// State of the reading side of the stream.
#[derive(Debug, Clone)]
#[repr(u8)]
enum ReaderState {
    /// The reading stream is open.
    Open = 0,
    /// A Fin flag was received.
    Fin = 1,
    /// FinAck was sent back.
    FinAck = 2,
}

impl AtomicState for ReaderState {
    fn from_u8(raw_state: u8) -> Self {
        match raw_state {
            0 => ReaderState::Open,
            1 => ReaderState::Fin,
            2 => ReaderState::FinAck,
            // Unreachable in practice: `into_u8` only ever stores a valid variant and the
            // `AtomicU8` only returns a previously-stored byte.
            // Return FinAck defensively rather than a panic.
            _ => ReaderState::FinAck,
        }
    }

    fn into_u8(self) -> u8 {
        self as u8
    }
}

/// State of the writing side of the stream.
#[derive(Debug, Clone)]
#[repr(u8)]
enum WriterState {
    /// The writing stream is open.
    Open = 0,
    /// A Fin flag was sent.
    Fin = 1,
    /// FinAck was received.
    FinAck = 2,
    /// StopSending was received.
    StopSending = 3,
}

impl AtomicState for WriterState {
    fn from_u8(raw_state: u8) -> Self {
        match raw_state {
            0 => WriterState::Open,
            1 => WriterState::Fin,
            2 => WriterState::FinAck,
            3 => WriterState::StopSending,
            // Unreachable in practice: `into_u8` only ever stores a valid variant and the
            // `AtomicU8` only returns a previously-stored byte.
            // Return FinAck defensively rather than a panic.
            _ => WriterState::FinAck,
        }
    }

    fn into_u8(self) -> u8 {
        self as u8
    }
}

/// Channel-backed substream. Must be owned and polled by exactly one task at a time.
pub struct Substream {
    /// Read buffer.
    read_buffer: BytesMut,
    /// RX channel for receiving messages from `peer`.
    rx: Receiver<Message>,
    /// TX channel for sending messages to `peer`, wrapped in a [`PollSender`]
    /// so that backpressure is driven by the caller's waker.
    tx: Option<PollSender<Message>>,
    /// State of the channel.
    channel_state: SharedState<ChannelState>,
    /// State of the writing half.
    writer_state: SharedState<WriterState>,
}

impl Substream {
    /// Create new [`Substream`].
    pub fn new() -> (Self, SubstreamHandle) {
        // Tokio channels implement their own backpressure,
        // which solves the Substream <-> SubstreamHandle backpressure problem.
        //
        // Given MAX_FRAME_SIZE as the maximum size of each message, each channel buffers
        // at most 4 MiB (256 * 16 KiB).
        //
        // For inbound messages this capacity is a hard limit used to distinguish a slow
        // reader (which needs some buffering) from a DoS attempt, unlike the outbound
        // path, the reader cannot be slowed down, so an over-full inbound
        // channel is treated as the peer flooding us and the substream is reset.
        let (inbound_message_tx, inbound_message_rx) = channel(MAX_INFLIGHT_MESSAGES);
        let (outbound_message_tx, outbound_message_rx) = channel(MAX_INFLIGHT_MESSAGES);

        let channel_state = SharedState::new(ChannelState::Open);
        let writer_state = SharedState::new(WriterState::Open);
        let reader_state = SharedState::new(ReaderState::Open);

        let handle = SubstreamHandle {
            channel_state: channel_state.clone(),
            writer_state: writer_state.clone(),
            reader_state: reader_state.clone(),
            message_tx: Some(inbound_message_tx),
            message_rx: outbound_message_rx,
            fin_ack_timeout: None,
        };

        (
            Self {
                read_buffer: BytesMut::new(),
                tx: Some(PollSender::new(outbound_message_tx)),
                rx: inbound_message_rx,
                channel_state,
                writer_state,
            },
            handle,
        )
    }
}

/// Substream handle that is given to the WebRTC transport backend.
pub struct SubstreamHandle {
    /// State of the channel.
    channel_state: SharedState<ChannelState>,
    /// State of the writing half.
    writer_state: SharedState<WriterState>,
    /// State of the reading half.
    reader_state: SharedState<ReaderState>,
    /// TX channel for sending inbound messages from `peer` to the associated `Substream`.
    ///
    /// The sender is taken (dropped) when a FIN flag is received from the remote:
    /// closing it signals to the `Substream` reader that no further inbound
    /// payloads will arrive.
    message_tx: Option<Sender<Message>>,
    /// RX channel for receiving outbound messages to `peer` from the associated `Substream`.
    message_rx: Receiver<Message>,
    /// Timeout for waiting on FIN_ACK after sending FIN
    /// Boxed to maintain Unpin for Substream while allowing the Sleep to be polled.
    fin_ack_timeout: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl SubstreamHandle {
    /// Handle message received from a remote peer.
    ///
    /// Process an incoming WebRTC message, handling any payload and flags.
    ///
    /// Payload is processed first (if present), then flags are handled. This ensures that
    /// a FIN message containing final data will deliver that data before signaling closure.
    pub async fn on_message(&mut self, message: WebRtcMessage) -> crate::Result<()> {
        // Discard inbound once the channel is reset or a reset has been initiated.
        // After a peer RESET_STREAM, SCTP ordering means nothing more should arrive.
        if matches!(
            self.channel_state.get(),
            ChannelState::Reset | ChannelState::InitReset
        ) {
            return Ok(());
        }

        // Process payload first, before handling flags.
        match (self.message_tx.as_ref(), message.payload) {
            (None, Some(payload)) if !payload.is_empty() => {
                tracing::debug!(
                    target: LOG_TARGET,
                    payload_len = payload.len(),
                    "peer sent payload after FIN flag, spec violation"
                );
            }
            (Some(message_tx), Some(payload)) if !payload.is_empty() => {
                let send_result = message_tx.try_send(Message {
                    payload,
                    flag: None,
                });

                match send_result {
                    Ok(_) => (),
                    Err(TrySendError::Closed(_)) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            "substream reader has been closed, skipping payload"
                        );
                    }
                    Err(TrySendError::Full(_)) => {
                        // If exceeded, the peer is sending faster than the reader can consume
                        // and thus this can be considered a DoS attack by forcing us to buffer
                        // those messages.
                        tracing::debug!(
                            target: LOG_TARGET,
                            max_pending_inbound_messages_per_channel = MAX_INFLIGHT_MESSAGES,
                            "max number of pending inbound messages exceeded"
                        );
                        self.channel_state.set(ChannelState::InitReset);
                        // Do not return an error here, let the Stream and thus
                        // `poll_next` send out a `RESET_STREAM` flag and then close the channel.
                        return Ok(());
                    }
                }
            }
            _ => (),
        }

        // Now handle flags
        if let Some(flag) = message.flag {
            match flag {
                Flag::Fin => {
                    // Guard against duplicate FIN messages - only send RecvClosed once
                    if matches!(
                        self.reader_state.get(),
                        ReaderState::Fin | ReaderState::FinAck
                    ) {
                        // Already processed FIN, ignore duplicate
                        tracing::debug!(target: LOG_TARGET, "received duplicate FIN, ignoring");
                        return Ok(());
                    }
                    self.reader_state.set(ReaderState::Fin);
                    if self.message_tx.take().is_none() {
                        tracing::warn!(target: LOG_TARGET, "message channel was already dropped");
                    }
                    return Ok(());
                }
                Flag::FinAck => {
                    // Received FIN_ACK, we can now fully close our write half
                    let writer_state = self.writer_state.get();
                    if matches!(writer_state, WriterState::Fin) {
                        self.writer_state.set(WriterState::FinAck);
                    } else {
                        // If FIN_ACK is received upon an unexpected writer_state
                        // tear down the connection.
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?writer_state,
                            "received FIN_ACK in unexpected writer state, tearing down channel"
                        );
                        self.channel_state.set(ChannelState::Reset);
                        self.message_rx.close();
                        let _ = self.message_tx.take();
                        return Err(Error::ConnectionClosed);
                    }
                    return Ok(());
                }
                Flag::StopSending => {
                    // Discard flag if already closed/closing.
                    if !matches!(self.channel_state.get(), ChannelState::Reset)
                        && !matches!(
                            self.writer_state.get(),
                            WriterState::Fin | WriterState::FinAck
                        )
                    {
                        self.writer_state.set(WriterState::StopSending);
                        self.message_rx.close();
                    }

                    return Ok(());
                }
                Flag::ResetStream => {
                    // RESET_STREAM abruptly terminates both sides of the stream
                    // (matching go-libp2p behavior)
                    self.channel_state.set(ChannelState::Reset);
                    self.message_rx.close();
                    let _ = self.message_tx.take();
                    return Err(Error::ConnectionClosed);
                }
            }
        }

        Ok(())
    }

    // This function carries forward the writer half close process.
    //
    // The following behaviors are expected on:
    // WriterState::Open|StopSending state
    // - flush any pending message
    // - send FIN flag
    // - start timeout_fin_ack
    // - transition to WriterState::Fin
    // WriterState::Fin state
    // - wait for FIN_ACK
    // - handle timeout
    // - transition to WriterState::FinAck
    // WriterState::FinAck state:
    // - do nothing, shutdown complete
    fn poll_half_close(&mut self, cx: &mut Context<'_>) -> Poll<Option<Message>> {
        match self.writer_state.get() {
            // First call to shutdown, if peer sent StopSending we are still
            // free to send Fin to make sure this half closes properly.
            WriterState::Open | WriterState::StopSending => {
                // Initialize the timeout for FIN_ACK
                let mut timeout = Box::pin(tokio::time::sleep(FIN_ACK_TIMEOUT));
                // Poll the timeout once to register it with tokio's timer
                // This ensures we'll be woken when it expires
                if timeout.as_mut().poll(cx).is_ready() {
                    tracing::error!(
                        target: LOG_TARGET,
                        "misconfigured timer is not supposed to be ready"
                    );
                }
                self.fin_ack_timeout = Some(timeout);
                self.writer_state.set(WriterState::Fin);
                // Send message with FIN flag
                Poll::Ready(Some(Message {
                    payload: vec![],
                    flag: Some(Flag::Fin),
                }))
            }
            // Sent FIN, waiting for FIN_ACK - poll timeout and return Pending
            WriterState::Fin => {
                // Poll the timeout - if it fires, force shutdown completion
                match self.fin_ack_timeout.as_mut() {
                    Some(timeout) =>
                        if timeout.as_mut().poll(cx).is_ready() {
                            tracing::debug!(
                                target: LOG_TARGET,
                                "FIN_ACK timeout exceeded, forcing shutdown completion"
                            );
                        } else {
                            return Poll::Pending;
                        },
                    None => {
                        tracing::warn!(
                            target: LOG_TARGET,
                            "unexpected writer state, forcing shutdown completion"
                        );
                    }
                }

                // If the timeout is reached we treat it as having received the
                // acknowledge but the channel is reset anyway.
                self.channel_state.set(ChannelState::Reset);

                self.fin_ack_timeout = None;
                Poll::Ready(Some(Message {
                    payload: vec![],
                    flag: Some(Flag::ResetStream),
                }))
            }
            // Already received FIN_ACK, shutdown complete
            WriterState::FinAck => Poll::Ready(None),
        }
    }
}

impl Stream for SubstreamHandle {
    type Item = Message;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // There are three states which need to be taken into consideration to poll the stream:
        // - channel_state: it preempts any other state if Reset has been entered
        //
        // NOTE: a channel_state waker is not needed here. Every transition to it happens
        // while the connection task is already awake, so the handle is re-polled the same
        // cycle. The match drives the reset, `InitReset` emits `RESET_STREAM` and advances
        // to `Reset` wihle once in `Reset`, it just closes the stream.
        match self.channel_state.get() {
            ChannelState::Open => (),
            ChannelState::InitReset => {
                // A local reset was initiated, emit RESET_STREAM to notify the peer.
                self.channel_state.set(ChannelState::Reset);
                return Poll::Ready(Some(Message {
                    payload: vec![],
                    flag: Some(Flag::ResetStream),
                }));
            }
            ChannelState::Reset => {
                // If something went wrong, `RESET_STREAM` should have been sent,
                // in that case the channel is treated as closed.
                return Poll::Ready(None);
            }
        }

        // - reader_state: this is mainly driven by the `on_message` function which reacts to
        //   incoming messages, there are 2 side effects which connects the two streams:
        //   1. If FIN arrived then FIN_ACK is expected to be sent back.
        //   2. If FIN_ACK arrived the writer_state is updated.
        if matches!(self.reader_state.register_and_get(cx), ReaderState::Fin) {
            self.reader_state.set(ReaderState::FinAck);
            return Poll::Ready(Some(Message {
                payload: vec![],
                flag: Some(Flag::FinAck),
            }));
        }

        // - writer_state: here messages sent from the `Substream` needs to be forwarded wrapped by
        //   the right flags. Based on the state the close procedure can be carried or messages can
        //   simply be forwarded.
        let writer_state_stream_result = match self.writer_state.get() {
            WriterState::Open => {
                match self.message_rx.poll_recv(cx) {
                    // Writes are finished, start half close procedure.
                    Poll::Ready(None) => self.poll_half_close(cx),
                    res => res,
                }
            }
            WriterState::Fin | WriterState::StopSending => self.poll_half_close(cx),
            WriterState::FinAck => Poll::Ready(None),
        };

        if !matches!(writer_state_stream_result, Poll::Ready(None)) {
            return writer_state_stream_result;
        }

        // The writer state has reached conclusion, if the same applies to the reader
        // state then graceful shutdown has been carried, close the Stream.
        if matches!(self.reader_state.get(), ReaderState::FinAck) {
            return Poll::Ready(None);
        }

        Poll::Pending
    }
}

impl tokio::io::AsyncRead for Substream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // if there are any remaining bytes from a previous read, consume them first
        if self.read_buffer.remaining() > 0 {
            let num_bytes = std::cmp::min(self.read_buffer.remaining(), buf.remaining());

            buf.put_slice(&self.read_buffer[..num_bytes]);
            self.read_buffer.advance(num_bytes);

            // TODO: optimize by trying to read more data from substream and not exiting early
            return Poll::Ready(Ok(()));
        }

        match futures::ready!(self.rx.poll_recv(cx)) {
            None if matches!(self.channel_state.get(), ChannelState::Reset) =>
                Poll::Ready(Err(tokio::io::ErrorKind::ConnectionReset.into())),
            None => Poll::Ready(Ok(())),
            Some(Message { payload, flag: _ }) => {
                if payload.len() > MAX_FRAME_SIZE {
                    return Poll::Ready(Err(std::io::ErrorKind::PermissionDenied.into()));
                }

                match buf.remaining() >= payload.len() {
                    true => buf.put_slice(&payload),
                    false => {
                        let remaining = buf.remaining();
                        buf.put_slice(&payload[..remaining]);
                        self.read_buffer.put_slice(&payload[remaining..]);
                    }
                }

                Poll::Ready(Ok(()))
            }
        }
    }
}

impl tokio::io::AsyncWrite for Substream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let Some(tx) = self.tx.as_mut() else {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        };

        // Backpressure delegated to tokio channel.
        match futures::ready!(tx.poll_reserve(cx)) {
            Ok(()) => {}
            Err(_) => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        };

        let num_bytes = std::cmp::min(MAX_FRAME_SIZE, buf.len());
        let frame = buf[..num_bytes].to_vec();

        match tx.send_item(Message {
            payload: frame,
            flag: None,
        }) {
            Ok(()) => Poll::Ready(Ok(num_bytes)),
            Err(_) => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let Some(tx) = self.tx.as_ref() else {
            // shutdown already ran
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        };
        if tx.is_closed() {
            // StopSending or ResetStream closed the receiver. Anything we've
            // enqueued past this point will not be delivered to the peer.
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        // Channel still open. `poll_write` already waits for channel capacity before returning,
        //  so by the time we get here the channel has accepted every byte we acknowledged.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        // Backpressure is delegated, based on tokio channel and str0m reliably
        // sending messages in order.
        let _ = self.tx.take();

        // Shutdown process is complete if either the channel entered a Reset
        // state or the writer has received a FinAck or StopSending.
        //
        // NOTE: short-circuiting the waker registration here is fine because
        // channel_state takes precedence over any writing state.
        let shutdown = matches!(self.channel_state.register_and_get(cx), ChannelState::Reset)
            || matches!(
                self.writer_state.register_and_get(cx),
                WriterState::FinAck | WriterState::StopSending
            );

        if shutdown {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

impl Drop for SubstreamHandle {
    fn drop(&mut self) {
        let graceful = matches!(self.writer_state.get(), WriterState::FinAck)
            && matches!(self.reader_state.get(), ReaderState::FinAck);

        // This allows to close all the pending channels if the SubstreamHandle
        // has been dropped, if graceful shutdown already happened this is a no-op.
        if !graceful {
            self.channel_state.set(ChannelState::Reset);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

    #[tokio::test]
    async fn write_small_frame() {
        let (mut substream, mut handle) = Substream::new();

        substream.write_all(&vec![0u8; 1337]).await.unwrap();

        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![0u8; 1337],
                flag: None
            })
        );

        futures::future::poll_fn(|cx| match handle.poll_next_unpin(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("invalid event"),
        })
        .await;
    }

    #[tokio::test]
    async fn write_large_frame() {
        let (mut substream, mut handle) = Substream::new();

        substream.write_all(&vec![0u8; (2 * MAX_FRAME_SIZE) + 1]).await.unwrap();

        assert_eq!(
            handle.message_rx.recv().await,
            Some(Message {
                payload: vec![0u8; MAX_FRAME_SIZE],
                flag: None,
            })
        );
        assert_eq!(
            handle.message_rx.recv().await,
            Some(Message {
                payload: vec![0u8; MAX_FRAME_SIZE],
                flag: None,
            })
        );
        assert_eq!(
            handle.message_rx.recv().await,
            Some(Message {
                payload: vec![0u8; 1],
                flag: None,
            })
        );

        futures::future::poll_fn(|cx| match handle.poll_next_unpin(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("invalid event"),
        })
        .await;
    }

    #[tokio::test]
    async fn handle_stop_sending_with_graceful_shutdown() {
        let (_substream, mut handle) = Substream::new();

        // Receiving StopSending
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::StopSending),
            })
            .await
            .unwrap();

        // Expecting FIN to be sent immediately
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::Fin),
            })
        );
        assert!(matches!(handle.writer_state.get(), WriterState::Fin));

        // Receiving FIN_ACK
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        // Write side is closed now, not the read side.
        assert!(matches!(handle.writer_state.get(), WriterState::FinAck));

        // Receiving FIN
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::Fin),
            })
            .await
            .unwrap();

        assert!(matches!(handle.reader_state.get(), ReaderState::Fin));
        // Expecing FIN_ACK to be sent immediately
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::FinAck),
            })
        );
        assert!(matches!(handle.reader_state.get(), ReaderState::FinAck));

        assert_eq!(handle.next().await, None);
    }

    #[tokio::test]
    async fn substream_shutdown() {
        let (mut substream, mut handle) = Substream::new();

        substream.write_all(&vec![1u8; 1337]).await.unwrap();

        // Spawn shutdown since it waits for FIN_ACK
        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
        });

        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![1u8; 1337],
                flag: None,
            })
        );
        // After shutdown, should send FIN flag
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );

        // Send FIN_ACK to complete shutdown
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        shutdown_task.await.unwrap();
    }

    #[tokio::test]
    async fn try_to_read_from_closed_substream() {
        let (mut substream, mut handle) = Substream::new();
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::Fin),
            })
            .await
            .unwrap();

        match substream.read(&mut vec![0u8; 256]).await {
            Ok(read_bytes) => {
                assert_eq!(read_bytes, 0)
            }
            _ => panic!("invalid event"),
        }
    }

    #[tokio::test]
    async fn read_small_frame() {
        let (mut substream, handle) = Substream::new();
        handle
            .message_tx
            .as_ref()
            .unwrap()
            .send(Message {
                payload: vec![1u8; 256],
                flag: None,
            })
            .await
            .unwrap();

        let mut buf = vec![0u8; 2048];

        match substream.read(&mut buf).await {
            Ok(nread) => {
                assert_eq!(nread, 256);
                assert_eq!(buf[..nread], vec![1u8; 256]);
            }
            Err(error) => panic!("invalid event: {error:?}"),
        }

        let mut read_buf = ReadBuf::new(&mut buf);
        futures::future::poll_fn(|cx| {
            match Pin::new(&mut substream).poll_read(cx, &mut read_buf) {
                Poll::Pending => Poll::Ready(()),
                _ => panic!("invalid event"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn read_small_frame_in_two_reads() {
        let (mut substream, handle) = Substream::new();
        let mut first = vec![1u8; 256];
        first.extend_from_slice(&vec![2u8; 256]);

        handle
            .message_tx
            .as_ref()
            .unwrap()
            .send(Message {
                payload: first,
                flag: None,
            })
            .await
            .unwrap();

        let mut buf = vec![0u8; 256];

        match substream.read(&mut buf).await {
            Ok(nread) => {
                assert_eq!(nread, 256);
                assert_eq!(buf[..nread], vec![1u8; 256]);
            }
            Err(error) => panic!("invalid event: {error:?}"),
        }

        match substream.read(&mut buf).await {
            Ok(nread) => {
                assert_eq!(nread, 256);
                assert_eq!(buf[..nread], vec![2u8; 256]);
            }
            Err(error) => panic!("invalid event: {error:?}"),
        }

        let mut read_buf = ReadBuf::new(&mut buf);
        futures::future::poll_fn(|cx| {
            match Pin::new(&mut substream).poll_read(cx, &mut read_buf) {
                Poll::Pending => Poll::Ready(()),
                _ => panic!("invalid event"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn read_frames() {
        let (mut substream, handle) = Substream::new();
        let mut first = vec![1u8; 256];
        first.extend_from_slice(&vec![2u8; 256]);

        handle
            .message_tx
            .as_ref()
            .unwrap()
            .send(Message {
                payload: first,
                flag: None,
            })
            .await
            .unwrap();
        handle
            .message_tx
            .as_ref()
            .unwrap()
            .send(Message {
                payload: vec![4u8; 2048],
                flag: None,
            })
            .await
            .unwrap();

        let mut buf = vec![0u8; 256];

        match substream.read(&mut buf).await {
            Ok(nread) => {
                assert_eq!(nread, 256);
                assert_eq!(buf[..nread], vec![1u8; 256]);
            }
            Err(error) => panic!("invalid event: {error:?}"),
        }

        let mut buf = vec![0u8; 128];

        match substream.read(&mut buf).await {
            Ok(nread) => {
                assert_eq!(nread, 128);
                assert_eq!(buf[..nread], vec![2u8; 128]);
            }
            Err(error) => panic!("invalid event: {error:?}"),
        }

        let mut buf = vec![0u8; 128];

        match substream.read(&mut buf).await {
            Ok(nread) => {
                assert_eq!(nread, 128);
                assert_eq!(buf[..nread], vec![2u8; 128]);
            }
            Err(error) => panic!("invalid event: {error:?}"),
        }

        let mut buf = vec![0u8; MAX_FRAME_SIZE];

        match substream.read(&mut buf).await {
            Ok(nread) => {
                assert_eq!(nread, 2048);
                assert_eq!(buf[..nread], vec![4u8; 2048]);
            }
            Err(error) => panic!("invalid event: {error:?}"),
        }

        let mut read_buf = ReadBuf::new(&mut buf);
        futures::future::poll_fn(|cx| {
            match Pin::new(&mut substream).poll_read(cx, &mut read_buf) {
                Poll::Pending => Poll::Ready(()),
                _ => panic!("invalid event"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn backpressure_works() {
        let (mut substream, _handle) = Substream::new();

        // use all available bandwidth which by default is `256 * MAX_FRAME_SIZE`,
        for _ in 0..128 {
            substream.write_all(&vec![0u8; 2 * MAX_FRAME_SIZE]).await.unwrap();
        }

        // try to write one more byte but since all available bandwidth
        // is taken the call will block
        futures::future::poll_fn(
            |cx| match Pin::new(&mut substream).poll_write(cx, &[0u8; 1]) {
                Poll::Pending => Poll::Ready(()),
                _ => panic!("invalid event"),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn backpressure_released_wakes_blocked_writer() {
        use tokio::time::{sleep, timeout, Duration};

        let (mut substream, mut handle) = Substream::new();

        // Fill the channel to capacity, same pattern as `backpressure_works`.
        for _ in 0..MAX_INFLIGHT_MESSAGES {
            substream.write_all(&vec![0u8; MAX_FRAME_SIZE]).await.unwrap();
        }

        // Spawn a writer task that will try to write once more. This should initially block
        // because the channel is full and rely on the AtomicWaker to be woken later.
        let writer = tokio::spawn(async move {
            substream
                .write_all(&vec![1u8; MAX_FRAME_SIZE])
                .await
                .expect("write should eventually succeed");
        });

        // Give the writer a short moment to reach the blocked (Pending) state.
        sleep(Duration::from_millis(10)).await;
        assert!(
            !writer.is_finished(),
            "writer should be blocked by backpressure"
        );

        // Now consume a single message from the receiving side. This will:
        //  - free capacity in the channel
        //  - call `write_waker.wake()` from `poll_next`
        //
        // That wake must cause the blocked writer to be polled again and complete its write.
        let _ = handle.next().await.expect("expected at least one outbound message");

        // The writer should now complete in a timely fashion, proving that:
        //  - registering the waker before `try_reserve` works (no lost wakeup)
        //  - the wake from `poll_next` correctly unblocks the writer.
        timeout(Duration::from_secs(1), writer)
            .await
            .expect("writer task did not complete after capacity was freed")
            .expect("writer task panicked");
    }

    #[tokio::test]
    async fn fin_flag_sent_on_shutdown() {
        let (mut substream, mut handle) = Substream::new();

        // Spawn shutdown since it waits for FIN_ACK
        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
        });

        // Should receive FIN flag
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );

        // Verify state is Fin
        assert!(matches!(handle.writer_state.get(), WriterState::Fin));

        // Send FIN_ACK to complete shutdown cleanly (avoids waiting for timeout)
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        // Wait for shutdown to complete
        shutdown_task.await.unwrap();
    }

    #[tokio::test]
    async fn fin_ack_response_on_receiving_fin() {
        let (mut substream, mut handle) = Substream::new();

        // Spawn task to consume inbound events sent to the substream
        let consumer_task = tokio::spawn(async move {
            // Substream should receive RecvClosed
            let mut buf = vec![0u8; 1024];
            match substream.read(&mut buf).await {
                Ok(0) => {
                    // Expected - read half closed
                }
                other => panic!("Unexpected result: {:?}", other),
            }
        });

        // Simulate receiving FIN from remote
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::Fin),
            })
            .await
            .unwrap();

        // Wait for consumer task to complete
        consumer_task.await.unwrap();

        // Verify FIN_ACK was sent outbound to network
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::FinAck)
            })
        );
    }

    #[tokio::test]
    async fn fin_ack_received_transitions_to_fin_acked() {
        let (mut substream, mut handle) = Substream::new();

        // Spawn shutdown since it waits for FIN_ACK
        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
        });

        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            }),
        );

        // Verify we're in Fin state
        assert!(matches!(handle.writer_state.get(), WriterState::Fin));

        // Simulate receiving FIN_ACK from remote
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        // Should transition to FinAcked
        assert!(matches!(handle.writer_state.get(), WriterState::FinAck));

        // Shutdown should now complete
        shutdown_task.await.unwrap();
    }

    #[tokio::test]
    async fn full_fin_handshake() {
        let (mut substream, mut handle) = Substream::new();

        // Write some data
        substream.write_all(&[1u8; 100]).await.unwrap();

        // Spawn shutdown in background since it will wait for FIN_ACK
        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
        });

        // Verify data was sent
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![1u8; 100],
                flag: None,
            })
        );

        // Verify FIN was sent
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );

        // Simulate receiving FIN_ACK
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        // Should be in FinAcked state
        assert!(matches!(handle.writer_state.get(), WriterState::FinAck));

        // Shutdown should now complete
        shutdown_task.await.unwrap();
    }

    #[tokio::test]
    async fn stop_sending_flag_closes_send_half() {
        let (mut substream, mut handle) = Substream::new();

        // Simulate receiving STOP_SENDING
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::StopSending),
            })
            .await
            .unwrap();

        // Should transition to SendClosed
        assert!(matches!(
            handle.writer_state.get(),
            WriterState::StopSending
        ));

        // Attempting to write should fail
        match substream.write_all(&[0u8; 100]).await {
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe),
            _ => panic!("write should have failed"),
        }
    }

    #[tokio::test]
    async fn reset_stream_flag_closes_both_sides() {
        let (mut substream, mut handle) = Substream::new();

        // Simulate receiving RESET_STREAM
        let result = handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::ResetStream),
            })
            .await;

        // Should return connection closed error
        assert!(matches!(result, Err(Error::ConnectionClosed)));

        // Write side should be closed (state = SendClosed)
        assert!(matches!(handle.channel_state.get(), ChannelState::Reset));

        let mut buf = vec![0u8; 1024];
        match substream.read(&mut buf).await {
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => (),
            other => panic!("Unexpected result: {:?}", other),
        }
        assert!(substream.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn fin_ack_does_not_trigger_other_flag() {
        let (mut substream, mut handle) = Substream::new();

        // Spawn shutdown since it waits for FIN_ACK
        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
        });

        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            }),
        );
        // Verify we're in Fin state
        assert!(matches!(handle.writer_state.get(), WriterState::Fin));

        // Now simulate receiving FIN_ACK (value = 3)
        // This should NOT trigger STOP_SENDING (value = 1) or RESET_STREAM (value = 2)
        // even though 3 & 1 == 1 and 3 & 2 == 2
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        // Should transition to FinAcked, not SendClosed
        assert!(matches!(handle.writer_state.get(), WriterState::FinAck));

        // Shutdown should complete
        shutdown_task.await.unwrap();

        // Writing should still work (not closed by STOP_SENDING)
        // Note: We already sent FIN, so write won't actually work, but the state check happens
        // first
    }

    #[tokio::test]
    async fn flags_are_mutually_exclusive() {
        let (_substream, mut handle) = Substream::new();

        // Test that STOP_SENDING (1) is handled correctly
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::StopSending),
            })
            .await
            .unwrap();

        assert!(matches!(
            handle.writer_state.get(),
            WriterState::StopSending
        ));

        // Create a new substream for RESET_STREAM test
        let (_substream2, mut handle2) = Substream::new();

        // Test that RESET_STREAM (2) is handled correctly
        let result = handle2
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::ResetStream),
            })
            .await;

        assert!(matches!(result, Err(Error::ConnectionClosed)));
        assert!(matches!(handle2.channel_state.get(), ChannelState::Reset));

        // Create a new substream for FIN test
        let (mut substream3, mut handle3) = Substream::new();

        // Spawn shutdown since it waits for FIN_ACK
        let shutdown_task3 = tokio::spawn(async move {
            substream3.shutdown().await.unwrap();
        });

        assert_eq!(
            handle3.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            }),
        );
        // Verify we're in Fin state
        assert!(matches!(handle3.writer_state.get(), WriterState::Fin));

        // Test that FIN_ACK (3) is handled correctly
        handle3
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        assert!(matches!(handle3.writer_state.get(), WriterState::FinAck));

        // Shutdown should complete
        shutdown_task3.await.unwrap();
    }

    #[tokio::test]
    async fn stop_sending_wakes_blocked_writer() {
        use tokio::io::AsyncWriteExt;
        let (mut substream, mut handle) = Substream::new();

        // Fill up the channel to cause poll_write to return Pending
        for _ in 0..MAX_INFLIGHT_MESSAGES {
            substream.write_all(&[1u8; 100]).await.unwrap();
        }

        // Now the next write should block waiting for channel capacity
        let write_task = tokio::spawn(async move {
            // This write will block because channel is full
            let result = substream.write_all(&[2u8; 100]).await;
            // Should fail because STOP_SENDING was received
            assert!(result.is_err());
        });

        // Give the writer time to block on poll_reserve
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!write_task.is_finished(), "write should be blocked");

        // Simulate receiving STOP_SENDING from remote
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::StopSending),
            })
            .await
            .unwrap();

        // The write task should wake up and see the state change
        tokio::time::timeout(Duration::from_secs(1), write_task)
            .await
            .expect("write task should complete after STOP_SENDING")
            .unwrap();
    }

    #[tokio::test]
    async fn reset_stream_wakes_blocked_writer() {
        use tokio::io::AsyncWriteExt;
        let (mut substream, mut handle) = Substream::new();

        // Fill up the channel to cause poll_write to return Pending
        for _ in 0..MAX_INFLIGHT_MESSAGES {
            substream.write_all(&[1u8; 100]).await.unwrap();
        }

        // Now the next write should block waiting for channel capacity
        let write_task = tokio::spawn(async move {
            // This write will block because channel is full
            let result = substream.write_all(&[2u8; 100]).await;
            // Should fail because RESET_STREAM was received
            assert!(result.is_err());
        });

        // Give the writer time to block on poll_reserve
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!write_task.is_finished(), "write should be blocked");

        // Simulate receiving RESET_STREAM from remote
        let result = handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::ResetStream),
            })
            .await;
        // RESET_STREAM returns an error
        assert!(result.is_err());

        // The write task should wake up and see the state change
        tokio::time::timeout(Duration::from_secs(1), write_task)
            .await
            .expect("write task should complete after RESET_STREAM")
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_rejects_new_writes() {
        use tokio::io::AsyncWriteExt;
        let (mut substream, mut handle) = Substream::new();

        // Write some data
        substream.write_all(&[1u8; 100]).await.unwrap();

        // Spawn shutdown in background
        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
        });

        // Wait for data and FIN to be sent
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![1u8; 100],
                flag: None,
            })
        );
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );

        // Verify we transitioned through Closing to Fin
        assert!(matches!(handle.writer_state.get(), WriterState::Fin));

        // Send FIN_ACK to complete shutdown
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        // Shutdown should complete
        shutdown_task.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_idempotent() {
        use tokio::io::AsyncWriteExt;
        let (mut substream, mut handle) = Substream::new();

        // Spawn first shutdown
        let shutdown_task1 = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
            substream
        });

        // Wait for FIN to be sent
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );
        assert!(matches!(handle.writer_state.get(), WriterState::Fin));

        // Send FIN_ACK to complete first shutdown
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        // First shutdown should complete
        let mut substream = shutdown_task1.await.unwrap();

        // Second shutdown should succeed without error (already in FinAcked state)
        substream.shutdown().await.unwrap();
        assert!(matches!(handle.writer_state.get(), WriterState::FinAck));
    }

    #[tokio::test]
    async fn shutdown_timeout_without_fin_ack() {
        use tokio::time::{timeout, Duration};

        let (mut substream, mut handle) = Substream::new();

        // Spawn shutdown in background
        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
        });

        // Wait for FIN to be sent
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );

        // Verify we're in Fin state
        assert!(matches!(handle.writer_state.get(), WriterState::Fin));

        // DON'T send FIN_ACK - let it timeout
        // The shutdown should complete after FIN_ACK_TIMEOUT (10 seconds)
        // Add a bit of buffer to the timeout
        let _ = timeout(Duration::from_secs(11), shutdown_task).await;

        // The timeout branch surfaces a RESET_STREAM event before signalling closure.
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::ResetStream)
            }),
        );
        assert!(matches!(handle.channel_state.get(), ChannelState::Reset));
    }

    #[tokio::test]
    async fn handle_signals_closure_after_substream_dropped() {
        use futures::StreamExt;

        let (mut substream, mut handle) = Substream::new();

        // Complete shutdown handshake (client-initiated)
        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
            // Substream will be dropped here
        });

        // Receive FIN
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );

        // Send FIN_ACK
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        // Wait for shutdown to complete and Substream to drop
        shutdown_task.await.unwrap();

        assert!(matches!(handle.writer_state.get(), WriterState::FinAck));
        // If reader_state.inner.state is also closed, then we can expect a None.
        handle.reader_state.set(ReaderState::FinAck);
        // Verify handle signals closure (returns None)
        assert_eq!(
            handle.next().await,
            None,
            "SubstreamHandle should signal closure after Substream is dropped"
        );
    }

    #[tokio::test]
    async fn server_side_closure_after_receiving_fin() {
        use futures::StreamExt;

        let (mut substream, mut handle) = Substream::new();

        // Spawn task to consume from substream (server side)
        let server_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 1024];
            // This should fail because we receive RecvClosed
            match substream.read(&mut buf).await {
                Ok(0) => (),
                other => panic!("Unexpected result: {:?}", other),
            }
            // Substream dropped here (server closes after receiving FIN)
        });

        // Remote (client) sends FIN
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::Fin),
            })
            .await
            .unwrap();

        // Verify FIN_ACK was sent back
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::FinAck)
            })
        );

        // Wait for server to close substream
        server_task.await.unwrap();

        // Verify handle signals closure (returns Fin)
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );
    }

    #[tokio::test]
    async fn simultaneous_close() {
        // Test simultaneous close where both sides send FIN at the same time.
        // This verifies that:
        // 1. Both sides can be in Fin state simultaneously
        // 2. Both sides correctly respond to FIN with FIN_ACK even when in Fin state
        // 3. Both sides eventually transition to FinAcked

        let (mut substream, mut handle) = Substream::new();

        // Local side initiates shutdown (sends FIN, transitions to Fin)
        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
        });

        // Wait for local FIN to be sent
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );

        // Verify local is in Fin state
        assert!(matches!(handle.writer_state.get(), WriterState::Fin));

        // Now simulate remote also sending FIN (simultaneous close)
        // This should trigger FIN_ACK response even though we're in Fin state
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::Fin),
            })
            .await
            .unwrap();

        // Local should send FIN_ACK in response to remote's FIN
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::FinAck)
            })
        );

        // Local should still be in Fin (waiting for FIN_ACK from remote)
        assert!(matches!(handle.writer_state.get(), WriterState::Fin));
        assert!(matches!(handle.reader_state.get(), ReaderState::FinAck));

        // Now remote sends FIN_ACK (completing their side of the handshake)
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        // Local should now transition to FinAcked
        assert!(matches!(handle.writer_state.get(), WriterState::FinAck));

        // Shutdown should complete successfully
        shutdown_task.await.unwrap();
    }

    #[tokio::test]
    async fn fin_with_payload_delivers_data_before_close() {
        // Test that when a FIN message contains payload data, the data is delivered
        // to the substream before the RecvClosed event. This is important because
        // the spec allows a FIN message to contain final data.

        let (mut substream, mut handle) = Substream::new();

        // Simulate receiving FIN with payload from remote
        handle
            .on_message(WebRtcMessage {
                payload: Some(b"final data".to_vec()),
                flag: Some(Flag::Fin),
            })
            .await
            .unwrap();

        // First, we should receive the payload data
        let mut buf = vec![0u8; 1024];
        let n = substream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"final data");

        // Then, subsequent read should fail with BrokenPipe (RecvClosed)
        match substream.read(&mut buf).await {
            Ok(0) => (),
            other => panic!("Expected BrokenPipe error, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn graceful_drop_does_not_reset_substream() {
        let (mut substream, mut handle) = Substream::new();

        // Drive the write half to FinAck (StopSending -> FIN -> FIN_ACK).
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::StopSending),
            })
            .await
            .unwrap();
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();
        assert!(matches!(handle.writer_state.get(), WriterState::FinAck));

        // Drive the read half to FinAck (peer FIN -> we emit FIN_ACK).
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::Fin),
            })
            .await
            .unwrap();
        assert_eq!(
            handle.next().await,
            Some(Message {
                payload: vec![],
                flag: Some(Flag::FinAck)
            })
        );
        assert!(matches!(handle.reader_state.get(), ReaderState::FinAck));

        // Both halves gracefully closed -> stream is done.
        assert_eq!(handle.next().await, None);

        // Dropping the gracefully-closed handle MUST NOT reset the channel.
        drop(handle);
        assert!(matches!(substream.channel_state.get(), ChannelState::Open));

        // The reader observes a clean EOF (Ok(0)), never a ConnectionReset error.
        let mut buf = vec![0u8; 256];
        let n = substream.read(&mut buf).await.expect("graceful drop must not surface an error");
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn non_graceful_drop_resets_substream() {
        let (mut substream, handle) = Substream::new();

        // No handshake performed; dropping must force a reset.
        drop(handle);
        assert!(matches!(substream.channel_state.get(), ChannelState::Reset));

        let mut buf = vec![0u8; 256];
        match substream.read(&mut buf).await {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
            other => panic!("expected ConnectionReset, got {other:?}"),
        }
    }
}
