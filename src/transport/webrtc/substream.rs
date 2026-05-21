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
    transport::webrtc::{schema::webrtc::message::Flag, util::WebRtcMessage, LOG_TARGET},
    Error,
};

use bytes::{Buf, BufMut, BytesMut};
use futures::{Future, Stream};
use tokio::sync::{
    mpsc::{channel, Receiver, Sender},
    oneshot,
};
use tokio_util::sync::PollSender;

use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll, Waker},
    time::Duration,
};

/// Maximum frame size.
const MAX_FRAME_SIZE: usize = 16384;

/// Timeout for waiting on FIN_ACK after sending FIN.
/// Matches go-libp2p and js-libp2p's 10-second stream close timeout.
const FIN_ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// Substream event emitted toward the peer.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Payload (with an optional control flag) destined for the peer.
    Message {
        payload: Vec<u8>,
        flag: Option<Flag>,
    },
}

/// Substream FSM. Lives entirely on the [`SubstreamHandle`] task; the
/// user-facing [`Substream`] never reads or writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Substream is fully open.
    Open,

    /// Remote sent STOP_SENDING; we may not write more data, but we may
    /// still gracefully close with FIN.
    SendClosed,

    /// RESET_STREAM has been observed or emitted.
    Reset,

    /// We sent FIN, waiting for FIN_ACK.
    FinSent,

    /// We received FIN_ACK, write half is closed.
    FinAcked,
}

/// User-facing substream. Implements `AsyncRead`/`AsyncWrite`. Owned and polled
/// by a single protocol task at a time.
pub struct Substream {
    /// Buffered bytes left over from a previous read frame.
    read_buffer: BytesMut,

    /// Outbound sender. `None` after `poll_shutdown` so further writes fail fast.
    /// Dropping the sender also closes the channel from the handle's perspective.
    tx: Option<PollSender<Event>>,

    /// Inbound receiver. Yields `None` when the handle drops `inbound_tx`
    /// (FIN/RESET received from the peer).
    rx: Receiver<Event>,

    /// Sent on first `poll_shutdown` or `Drop` to tell the handle the user is
    /// finished writing and the FIN handshake should begin.
    done_signal: Option<oneshot::Sender<()>>,

    /// Awaited by `poll_shutdown`; resolved when the handle reaches
    /// FinAcked/Reset (FIN_ACK received, timeout, or RESET).
    shutdown_complete: Option<oneshot::Receiver<()>>,
}

/// Network-facing counterpart of a [`Substream`]. Owned and polled exclusively
/// by the WebRTC connection event loop.
pub struct SubstreamHandle {
    /// FSM state. Plain field — no mutex needed because only this task touches it.
    state: State,

    /// Inbound sender (handle → substream). Dropped to close the read half so
    /// the substream's `poll_read` surfaces `BrokenPipe`.
    inbound_tx: Option<Sender<Event>>,

    /// Outbound receiver (substream → handle). Dropped on STOP_SENDING/RESET
    /// so the substream's `poll_write` surfaces `BrokenPipe` via the channel's
    /// own waker mechanism — no separate cross-task waker required.
    rx: Option<Receiver<Event>>,

    /// Receives the "user is done writing" oneshot from the substream.
    done_signal: Option<oneshot::Receiver<()>>,

    /// Fired on FIN_ACK / FIN_ACK timeout / RESET so `poll_shutdown` can return.
    shutdown_complete: Option<oneshot::Sender<()>>,

    /// Frames the handle has queued to emit on the wire (FIN, FIN_ACK, RESET).
    /// Drained ahead of user payloads in `poll_next`.
    pending_out: VecDeque<Event>,

    /// Timer that triggers RESET if FIN_ACK never arrives.
    fin_ack_timeout: Option<Pin<Box<tokio::time::Sleep>>>,

    /// Dedup flag: only forward read-half-closure once even if multiple FINs arrive.
    read_closed: bool,

    /// Last waker registered by `poll_next`; woken from `on_message` whenever
    /// pending state changes so the connection loop re-polls us.
    waker: Option<Waker>,
}

impl Substream {
    /// Create a new linked ([`Substream`], [`SubstreamHandle`]) pair.
    pub fn new() -> (Self, SubstreamHandle) {
        let (outbound_tx, outbound_rx) = channel(256);
        let (inbound_tx, inbound_rx) = channel(256);
        let (done_tx, done_rx) = oneshot::channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let handle = SubstreamHandle {
            state: State::Open,
            inbound_tx: Some(inbound_tx),
            rx: Some(outbound_rx),
            done_signal: Some(done_rx),
            shutdown_complete: Some(shutdown_tx),
            pending_out: VecDeque::new(),
            fin_ack_timeout: None,
            read_closed: false,
            waker: None,
        };

        let substream = Self {
            read_buffer: BytesMut::new(),
            tx: Some(PollSender::new(outbound_tx)),
            rx: inbound_rx,
            done_signal: Some(done_tx),
            shutdown_complete: Some(shutdown_rx),
        };

        (substream, handle)
    }
}

impl SubstreamHandle {
    /// Handle a message received from the peer.
    ///
    /// Payload is delivered first (preserving the FIN-with-payload contract)
    /// and the flag is then applied to the local FSM.
    pub async fn on_message(&mut self, message: WebRtcMessage) -> crate::Result<()> {
        if let Some(payload) = message.payload {
            if !payload.is_empty() {
                tracing::trace!(
                    target: LOG_TARGET,
                    payload_len = payload.len(),
                    "forwarding payload to substream",
                );
                if let Some(tx) = self.inbound_tx.as_ref() {
                    if tx.send(Event::Message { payload, flag: None }).await.is_err() {
                        tracing::debug!(
                            target: LOG_TARGET,
                            "substream dropped, payload discarded",
                        );
                    }
                }
            }
        }

        let Some(flag) = message.flag else {
            return Ok(());
        };

        match flag {
            Flag::Fin => {
                if self.read_closed {
                    tracing::debug!(target: LOG_TARGET, "duplicate FIN, ignoring");
                    return Ok(());
                }
                self.read_closed = true;
                // Drop the read half. The payload above was already queued onto the
                // channel, so the substream observes it before the close.
                self.inbound_tx = None;
                self.queue_frame(Event::Message {
                    payload: vec![],
                    flag: Some(Flag::FinAck),
                });
            }
            Flag::FinAck => {
                if matches!(self.state, State::FinSent) {
                    self.state = State::FinAcked;
                    self.fin_ack_timeout = None;
                    if let Some(tx) = self.shutdown_complete.take() {
                        let _ = tx.send(());
                    }
                    self.wake();
                } else {
                    tracing::warn!(
                        target: LOG_TARGET,
                        state = ?self.state,
                        "FIN_ACK in unexpected state, ignoring",
                    );
                }
            }
            Flag::StopSending => {
                if !matches!(self.state, State::FinSent | State::FinAcked) {
                    self.state = State::SendClosed;
                }
                // Drop the outbound receiver so blocked writers wake via the
                // channel's own closure signal.
                self.rx = None;
                self.wake();
            }
            Flag::ResetStream => {
                self.state = State::Reset;
                self.inbound_tx = None;
                self.rx = None;
                self.fin_ack_timeout = None;
                if let Some(tx) = self.shutdown_complete.take() {
                    let _ = tx.send(());
                }
                self.wake();
                return Err(Error::ConnectionClosed);
            }
        }

        Ok(())
    }

    fn queue_frame(&mut self, event: Event) {
        self.pending_out.push_back(event);
        self.wake();
    }

    fn wake(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    /// Begin the FIN handshake. Caller must have established that we're in a
    /// state that permits it (`Open` or `SendClosed`).
    fn start_half_close(&mut self, cx: &mut Context<'_>) {
        let mut timeout = Box::pin(tokio::time::sleep(FIN_ACK_TIMEOUT));
        // Prime the timer so we'll be woken on expiry.
        let _ = timeout.as_mut().poll(cx);
        self.fin_ack_timeout = Some(timeout);
        self.state = State::FinSent;
        self.pending_out.push_back(Event::Message {
            payload: vec![],
            flag: Some(Flag::Fin),
        });
    }
}

impl Stream for SubstreamHandle {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // 1. Drain queued control frames (FIN, FIN_ACK, RESET).
            if let Some(event) = self.pending_out.pop_front() {
                return Poll::Ready(Some(event));
            }

            // 2. The substream is fully done when either:
            //    - we observed/emitted RESET (abrupt close), or
            //    - both halves are closed: our write half is acked AND the peer
            //      has closed the read half (FIN received or RESET).
            // FinAcked alone is *not* enough: the peer may still be sending us
            // data over the read half (e.g. the response phase of a perf run).
            if matches!(self.state, State::Reset)
                || (matches!(self.state, State::FinAcked) && self.inbound_tx.is_none())
            {
                return Poll::Ready(None);
            }

            // 3. Forward user-initiated outbound data.
            let rx_poll = self.rx.as_mut().map(|rx| rx.poll_recv(cx));
            match rx_poll {
                Some(Poll::Ready(Some(event))) => return Poll::Ready(Some(event)),
                Some(Poll::Ready(None)) => self.rx = None,
                Some(Poll::Pending) | None => {}
            }

            // 4. Has the user signaled they're done writing (poll_shutdown / Drop)?
            let user_done = match self.done_signal.as_mut() {
                Some(rx) => match Pin::new(rx).poll(cx) {
                    Poll::Ready(_) => {
                        self.done_signal = None;
                        true
                    }
                    Poll::Pending => false,
                },
                None => true,
            };

            if user_done && matches!(self.state, State::Open | State::SendClosed) {
                self.start_half_close(cx);
                continue;
            }

            // 5. While in FinSent, race FIN_ACK vs. the timer.
            if matches!(self.state, State::FinSent) {
                if let Some(timeout) = self.fin_ack_timeout.as_mut() {
                    if timeout.as_mut().poll(cx).is_ready() {
                        tracing::debug!(
                            target: LOG_TARGET,
                            "FIN_ACK timeout exceeded, forcing RESET",
                        );
                        self.fin_ack_timeout = None;
                        self.state = State::Reset;
                        self.inbound_tx = None;
                        if let Some(tx) = self.shutdown_complete.take() {
                            let _ = tx.send(());
                        }
                        self.pending_out.push_back(Event::Message {
                            payload: vec![],
                            flag: Some(Flag::ResetStream),
                        });
                        continue;
                    }
                }
            }

            self.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
    }
}

impl tokio::io::AsyncRead for Substream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // If there are any remaining bytes from a previous read, consume them first
        if self.read_buffer.remaining() > 0 {
            let num_bytes = std::cmp::min(self.read_buffer.remaining(), buf.remaining());

            buf.put_slice(&self.read_buffer[..num_bytes]);
            self.read_buffer.advance(num_bytes);

            // TODO: optimize by trying to read more data from substream and not exiting early
            return Poll::Ready(Ok(()));
        }

        match futures::ready!(self.rx.poll_recv(cx)) {
            None => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
            Some(Event::Message { payload, flag: _ }) => {
                if payload.len() > MAX_FRAME_SIZE {
                    return Poll::Ready(Err(std::io::ErrorKind::PermissionDenied.into()));
                }

                if buf.remaining() >= payload.len() {
                    buf.put_slice(&payload);
                } else {
                    let remaining = buf.remaining();
                    buf.put_slice(&payload[..remaining]);
                    self.read_buffer.put_slice(&payload[remaining..]);
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
        let tx = match self.tx.as_mut() {
            Some(tx) => tx,
            None => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        };

        match futures::ready!(tx.poll_reserve(cx)) {
            Ok(()) => {}
            Err(_) => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        }

        let num_bytes = std::cmp::min(MAX_FRAME_SIZE, buf.len());
        let frame = buf[..num_bytes].to_vec();

        match tx.send_item(Event::Message {
            payload: frame,
            flag: None,
        }) {
            Ok(()) => Poll::Ready(Ok(num_bytes)),
            Err(_) => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        // `poll_write` already waits for channel capacity before returning,
        // so by the time we get here every acknowledged byte has been handed off.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        // First call: signal the handle and stop accepting writes locally.
        if let Some(signal) = self.done_signal.take() {
            let _ = signal.send(());
            self.tx = None;
        }

        match &mut self.shutdown_complete {
            None => Poll::Ready(Ok(())),
            Some(rx) => match Pin::new(rx).poll(cx) {
                Poll::Ready(_) => {
                    self.shutdown_complete = None;
                    Poll::Ready(Ok(()))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

impl Drop for Substream {
    fn drop(&mut self) {
        if let Some(signal) = self.done_signal.take() {
            let _ = signal.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tokio::{
        io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
        sync::mpsc::error::TryRecvError,
    };

    #[tokio::test]
    async fn write_small_frame() {
        let (mut substream, mut handle) = Substream::new();

        substream.write_all(&vec![0u8; 1337]).await.unwrap();

        assert_eq!(
            handle.next().await,
            Some(Event::Message {
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
            handle.next().await,
            Some(Event::Message {
                payload: vec![0u8; MAX_FRAME_SIZE],
                flag: None,
            })
        );
        assert_eq!(
            handle.next().await,
            Some(Event::Message {
                payload: vec![0u8; MAX_FRAME_SIZE],
                flag: None,
            })
        );
        assert_eq!(
            handle.next().await,
            Some(Event::Message {
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
    async fn try_to_write_to_closed_substream() {
        let (mut substream, mut handle) = Substream::new();
        // Drive into SendClosed via the real FSM path.
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::StopSending),
            })
            .await
            .unwrap();

        match substream.write_all(&vec![0u8; 1337]).await {
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe),
            _ => panic!("invalid event"),
        }
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
            Some(Event::Message {
                payload: vec![1u8; 1337],
                flag: None,
            })
        );
        // After shutdown, should send FIN flag
        assert_eq!(
            handle.next().await,
            Some(Event::Message {
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
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe),
            _ => panic!("invalid event"),
        }
    }

    #[tokio::test]
    async fn read_small_frame() {
        let (mut substream, handle) = Substream::new();
        handle
            .inbound_tx
            .as_ref()
            .unwrap()
            .send(Event::Message {
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
            .inbound_tx
            .as_ref()
            .unwrap()
            .send(Event::Message {
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
        let inbound = handle.inbound_tx.as_ref().unwrap();

        inbound
            .send(Event::Message {
                payload: first,
                flag: None,
            })
            .await
            .unwrap();
        inbound
            .send(Event::Message {
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
        for _ in 0..128 {
            substream.write_all(&vec![0u8; 2 * MAX_FRAME_SIZE]).await.unwrap();
        }

        // Spawn a writer task that will try to write once more. This should initially block
        // because the channel is full; tokio's internal mpsc waker is responsible for waking
        // it once capacity is freed.
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

        // Consume a single message from the receiving side, freeing one capacity slot.
        let _ = handle.next().await.expect("expected at least one outbound message");

        // The writer should now complete in a timely fashion.
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
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );

        // Verify state is FinSent
        assert_eq!(handle.state, State::FinSent);

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
            // Substream should receive BrokenPipe (read half closed)
            let mut buf = vec![0u8; 1024];
            match substream.read(&mut buf).await {
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
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
            Some(Event::Message {
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
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            }),
        );
        // Verify we're in FinSent state
        assert_eq!(handle.state, State::FinSent);

        // Simulate receiving FIN_ACK from remote
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        // Should transition to FinAcked
        assert_eq!(handle.state, State::FinAcked);

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
            Some(Event::Message {
                payload: vec![1u8; 100],
                flag: None,
            })
        );

        // Verify FIN was sent
        assert_eq!(
            handle.next().await,
            Some(Event::Message {
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
        assert_eq!(handle.state, State::FinAcked);

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
        assert_eq!(handle.state, State::SendClosed);

        // Attempting to write should fail
        match substream.write_all(&[0u8; 100]).await {
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe),
            _ => panic!("write should have failed"),
        }
    }

    #[tokio::test]
    async fn reset_stream_flag_closes_both_sides() {
        use tokio::io::AsyncWriteExt;
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

        // Both halves should be closed
        assert_eq!(handle.state, State::Reset);

        // Attempting to write should fail
        match substream.write_all(&[0u8; 100]).await {
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe),
            _ => panic!("write should have failed"),
        }

        // Read half is closed: the inbound channel has been dropped on the handle side.
        assert!(matches!(substream.rx.try_recv(), Err(TryRecvError::Disconnected)));
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
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            }),
        );
        // Verify we're in FinSent state
        assert_eq!(handle.state, State::FinSent);

        // Now simulate receiving FIN_ACK
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        // Should transition to FinAcked, not SendClosed
        assert_eq!(handle.state, State::FinAcked);

        // Shutdown should complete
        shutdown_task.await.unwrap();
    }

    #[tokio::test]
    async fn flags_are_mutually_exclusive() {
        let (_substream, mut handle) = Substream::new();

        // Test that STOP_SENDING is handled correctly
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::StopSending),
            })
            .await
            .unwrap();

        assert_eq!(handle.state, State::SendClosed);

        // Create a new substream for RESET_STREAM test
        let (_substream2, mut handle2) = Substream::new();

        let result = handle2
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::ResetStream),
            })
            .await;

        assert!(matches!(result, Err(Error::ConnectionClosed)));
        assert_eq!(handle2.state, State::Reset);

        // Create a new substream for FIN test
        let (mut substream3, mut handle3) = Substream::new();

        // Spawn shutdown since it waits for FIN_ACK
        let shutdown_task3 = tokio::spawn(async move {
            substream3.shutdown().await.unwrap();
        });

        assert_eq!(
            handle3.next().await,
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            }),
        );
        // Verify we're in FinSent state
        assert_eq!(handle3.state, State::FinSent);

        handle3
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        assert_eq!(handle3.state, State::FinAcked);

        // Shutdown should complete
        shutdown_task3.await.unwrap();
    }

    #[tokio::test]
    async fn stop_sending_wakes_blocked_writer() {
        use tokio::io::AsyncWriteExt;
        let (mut substream, mut handle) = Substream::new();

        // Fill up the channel to cause poll_write to return Pending
        // Channel capacity is 256
        for _ in 0..256 {
            substream.write_all(&[1u8; 100]).await.unwrap();
        }

        // Now the next write should block waiting for channel capacity
        let write_task = tokio::spawn(async move {
            let result = substream.write_all(&[2u8; 100]).await;
            // Should fail because STOP_SENDING closed the channel
            assert!(result.is_err());
        });

        // Give the writer time to block on poll_reserve
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!write_task.is_finished(), "write should be blocked");

        // STOP_SENDING drops the handle's outbound receiver, which closes the
        // mpsc channel and wakes the blocked sender via tokio's own machinery.
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::StopSending),
            })
            .await
            .unwrap();

        // The write task should wake up and see the channel as closed
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
        for _ in 0..256 {
            substream.write_all(&[1u8; 100]).await.unwrap();
        }

        let write_task = tokio::spawn(async move {
            let result = substream.write_all(&[2u8; 100]).await;
            assert!(result.is_err());
        });

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!write_task.is_finished(), "write should be blocked");

        // RESET_STREAM drops the outbound receiver, mirroring STOP_SENDING's wake path.
        let result = handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::ResetStream),
            })
            .await;
        assert!(result.is_err());

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

        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
        });

        assert_eq!(
            handle.next().await,
            Some(Event::Message {
                payload: vec![1u8; 100],
                flag: None,
            })
        );
        assert_eq!(
            handle.next().await,
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );

        assert_eq!(handle.state, State::FinSent);

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
    async fn shutdown_idempotent() {
        use tokio::io::AsyncWriteExt;
        let (mut substream, mut handle) = Substream::new();

        let shutdown_task1 = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
            substream
        });

        assert_eq!(
            handle.next().await,
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );
        assert_eq!(handle.state, State::FinSent);

        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        let mut substream = shutdown_task1.await.unwrap();

        // Second shutdown is a no-op
        substream.shutdown().await.unwrap();
        assert_eq!(handle.state, State::FinAcked);
    }

    #[tokio::test]
    async fn shutdown_timeout_without_fin_ack() {
        use tokio::time::{timeout, Duration};

        let (mut substream, mut handle) = Substream::new();

        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
        });

        assert_eq!(
            handle.next().await,
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );

        assert_eq!(handle.state, State::FinSent);

        // DON'T send FIN_ACK - let it timeout
        let _ = timeout(Duration::from_secs(11), shutdown_task).await;

        // The timeout surfaces a RESET_STREAM frame before signalling closure.
        assert_eq!(
            handle.next().await,
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::ResetStream)
            }),
        );
        assert_eq!(handle.state, State::Reset);
    }

    #[tokio::test]
    async fn handle_signals_closure_after_substream_dropped() {
        use futures::StreamExt;

        let (mut substream, mut handle) = Substream::new();

        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
            // Substream is dropped here.
        });

        assert_eq!(
            handle.next().await,
            Some(Event::Message {
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

        shutdown_task.await.unwrap();

        // Our write half is acked, but the read half is still open: the channel
        // must stay alive so the peer can keep sending data (e.g. perf download).
        // Simulate the peer closing its write half too.
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::Fin),
            })
            .await
            .unwrap();

        // FIN_ACK reply is queued; drain it.
        assert_eq!(
            handle.next().await,
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::FinAck)
            })
        );

        // Now both halves are closed — the handle finally signals closure.
        assert_eq!(
            handle.next().await,
            None,
            "SubstreamHandle should signal closure once both halves are closed"
        );
    }

    #[tokio::test]
    async fn server_side_closure_after_receiving_fin() {
        use futures::StreamExt;

        let (mut substream, mut handle) = Substream::new();

        let server_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 1024];
            match substream.read(&mut buf).await {
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                    // Expected - read half closed by FIN
                }
                other => panic!("Unexpected result: {:?}", other),
            }
            // Substream dropped here.
        });

        // Remote (client) sends FIN.
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::Fin),
            })
            .await
            .unwrap();

        // FIN_ACK is queued in response.
        assert_eq!(
            handle.next().await,
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::FinAck)
            })
        );

        server_task.await.unwrap();

        // After the substream is dropped, the handle initiates its own half-close (FIN).
        assert_eq!(
            handle.next().await,
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );
    }

    #[tokio::test]
    async fn simultaneous_close() {
        // Both sides send FIN at the same time. We must still respond with
        // FIN_ACK to the peer's FIN while ourselves waiting for theirs.
        let (mut substream, mut handle) = Substream::new();

        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
        });

        assert_eq!(
            handle.next().await,
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );

        assert_eq!(handle.state, State::FinSent);

        // Remote sends FIN while we're in FinSent.
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::Fin),
            })
            .await
            .unwrap();

        // We respond with FIN_ACK regardless of being in FinSent.
        assert_eq!(
            handle.next().await,
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::FinAck)
            })
        );

        // Still in FinSent (waiting for the peer's FIN_ACK).
        assert_eq!(handle.state, State::FinSent);

        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        assert_eq!(handle.state, State::FinAcked);
        shutdown_task.await.unwrap();
    }

    #[tokio::test]
    async fn read_half_stays_open_after_local_shutdown() {
        // Regression for the libp2p-perf flow: client uploads, half-closes its
        // write side, then reads the server's response back. Receiving FIN_ACK
        // on our own FIN must NOT tear down the read half — the peer is
        // expected to keep sending until it sends its own FIN.
        let (mut substream, mut handle) = Substream::new();

        // Upload one frame, then shutdown our write half.
        substream.write_all(&[7u8; 100]).await.unwrap();
        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
            substream
        });

        // Drain upload + FIN, then ack our FIN.
        assert_eq!(
            handle.next().await,
            Some(Event::Message {
                payload: vec![7u8; 100],
                flag: None,
            })
        );
        assert_eq!(
            handle.next().await,
            Some(Event::Message {
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

        // shutdown() resolves — write half is fully closed.
        let mut substream = shutdown_task.await.unwrap();
        assert_eq!(handle.state, State::FinAcked);

        // Critically: the handle is *not* done. Peer still owes us data.
        // Deliver the "download" payload through the inbound channel.
        handle
            .on_message(WebRtcMessage {
                payload: Some(b"download-bytes".to_vec()),
                flag: None,
            })
            .await
            .unwrap();

        let mut buf = vec![0u8; 64];
        let n = substream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"download-bytes");

        // Now the peer closes its write half. The substream should observe EOF
        // and the handle should finalise on its next poll.
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::Fin),
            })
            .await
            .unwrap();
        match substream.read(&mut buf).await {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            other => panic!("expected BrokenPipe after peer FIN, got {:?}", other),
        }

        // Drain the FIN_ACK we owe the peer, then the handle is done.
        assert_eq!(
            handle.next().await,
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::FinAck)
            })
        );
        assert_eq!(handle.next().await, None);
    }

    #[tokio::test]
    async fn fin_with_payload_delivers_data_before_close() {
        // FIN carrying a payload: data must reach the substream before the
        // read half is closed.
        let (mut substream, mut handle) = Substream::new();

        handle
            .on_message(WebRtcMessage {
                payload: Some(b"final data".to_vec()),
                flag: Some(Flag::Fin),
            })
            .await
            .unwrap();

        let mut buf = vec![0u8; 1024];
        let n = substream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"final data");

        match substream.read(&mut buf).await {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            other => panic!("Expected BrokenPipe error, got: {:?}", other),
        }
    }
}
