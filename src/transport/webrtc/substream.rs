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
    transport::webrtc::{schema::webrtc::message::Flag, util::WebRtcMessage},
    Error,
};

use bytes::{Buf, BufMut, BytesMut};
use futures::{task::AtomicWaker, Future, Stream};
use parking_lot::Mutex;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_util::sync::PollSender;

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

/// Maximum frame size.
const MAX_FRAME_SIZE: usize = 16384;

/// Timeout for waiting on FIN_ACK after sending FIN.
/// Matches go-libp2p's 5 second stream close timeout.
#[cfg(not(test))]
const FIN_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Shorter timeout for tests.
#[cfg(test)]
const FIN_ACK_TIMEOUT: Duration = Duration::from_secs(2);

/// Substream event.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Receiver closed.
    RecvClosed,

    /// Send/receive message with optional flag.
    Message {
        payload: Vec<u8>,
        flag: Option<Flag>,
    },
}

/// Substream stream.
#[derive(Debug, Clone, Copy)]
enum State {
    /// Substream is fully open.
    Open,

    /// Remote is no longer interested in receiving anything.
    SendClosed,

    /// Shutdown initiated, flushing pending data before sending FIN.
    Closing,

    /// We sent FIN, waiting for FIN_ACK.
    FinSent,

    /// We received FIN_ACK, write half is closed.
    FinAcked,
}

/// Channel-backed substream. Must be owned and polled by exactly one task at a time.
pub struct Substream {
    /// Substream state.
    state: Arc<Mutex<State>>,

    /// Read buffer.
    read_buffer: BytesMut,

    /// TX channel for sending messages to `peer`, wrapped in a [`PollSender`]
    /// so that backpressure is driven by the caller's waker.
    tx: PollSender<Event>,

    /// RX channel for receiving messages from `peer`.
    rx: Receiver<Event>,

    /// Waker to notify when shutdown completes (FIN_ACK received).
    shutdown_waker: Arc<AtomicWaker>,

    /// Timeout for waiting on FIN_ACK after sending FIN.
    /// Boxed to maintain Unpin for Substream while allowing the Sleep to be polled.
    fin_ack_timeout: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl Substream {
    /// Create new [`Substream`].
    pub fn new() -> (Self, SubstreamHandle) {
        let (outbound_tx, outbound_rx) = channel(256);
        let (inbound_tx, inbound_rx) = channel(256);
        let state = Arc::new(Mutex::new(State::Open));
        let shutdown_waker = Arc::new(AtomicWaker::new());

        let handle = SubstreamHandle {
            inbound_tx,
            outbound_tx: outbound_tx.clone(),
            rx: outbound_rx,
            state: Arc::clone(&state),
            shutdown_waker: Arc::clone(&shutdown_waker),
        };

        (
            Self {
                state,
                tx: PollSender::new(outbound_tx),
                rx: inbound_rx,
                read_buffer: BytesMut::new(),
                shutdown_waker,
                fin_ack_timeout: None,
            },
            handle,
        )
    }
}

/// Substream handle that is given to the WebRTC transport backend.
pub struct SubstreamHandle {
    state: Arc<Mutex<State>>,

    /// TX channel for sending inbound messages from `peer` to the associated `Substream`.
    inbound_tx: Sender<Event>,

    /// TX channel for sending outbound messages to `peer` (e.g., FIN_ACK responses).
    outbound_tx: Sender<Event>,

    /// RX channel for receiving outbound messages to `peer` from the associated `Substream`.
    rx: Receiver<Event>,

    /// Waker to notify when shutdown completes (FIN_ACK received).
    shutdown_waker: Arc<AtomicWaker>,
}

impl SubstreamHandle {
    /// Handle message received from a remote peer.
    ///
    /// If the message contains a flag, handle it first and appropriately close the correct
    /// side of the substream. If the message contained any payload, send it to the protocol for
    /// further processing.
    pub async fn on_message(&self, message: WebRtcMessage) -> crate::Result<()> {
        if let Some(flag) = message.flag {
            match flag {
                Flag::Fin => {
                    // Received FIN from remote, close our read half
                    self.inbound_tx.send(Event::RecvClosed).await?;

                    // Send FIN_ACK back to remote
                    // Note: We stay in current state to allow shutdown() to send our own FIN if
                    // needed
                    return self
                        .outbound_tx
                        .send(Event::Message {
                            payload: vec![],
                            flag: Some(Flag::FinAck),
                        })
                        .await
                        .map_err(From::from);
                }
                Flag::FinAck => {
                    // Received FIN_ACK, we can now fully close our write half
                    let mut state = self.state.lock();
                    if matches!(*state, State::FinSent) {
                        *state = State::FinAcked;
                        // Wake up any task waiting on shutdown
                        self.shutdown_waker.wake();
                    }
                    return Ok(());
                }
                Flag::StopSending => {
                    *self.state.lock() = State::SendClosed;
                    return Ok(());
                }
                Flag::ResetStream => {
                    return Err(Error::ConnectionClosed);
                }
            }
        }

        if let Some(payload) = message.payload {
            if !payload.is_empty() {
                return self
                    .inbound_tx
                    .send(Event::Message {
                        payload,
                        flag: None,
                    })
                    .await
                    .map_err(From::from);
            }
        }

        Ok(())
    }
}

impl Stream for SubstreamHandle {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // First, try to drain any pending outbound messages
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(event)) => return Poll::Ready(Some(event)),
            Poll::Ready(None) => {
                // Outbound channel closed (all senders dropped)
                return Poll::Ready(None);
            }
            Poll::Pending => {
                // No messages available, check if we should signal closure
            }
        }

        // Check if Substream has been dropped (inbound channel closed)
        // When Substream is dropped, there will be no more outbound messages
        // Since we've already tried to recv above and got Pending, we know the queue is empty
        // Therefore, it's safe to signal closure
        if self.inbound_tx.is_closed() {
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
            None | Some(Event::RecvClosed) =>
                Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
            Some(Event::Message { payload, flag: _ }) => {
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
        // Reject writes if we're closing or closed
        match *self.state.lock() {
            State::SendClosed | State::Closing | State::FinSent | State::FinAcked => {
                return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            State::Open => {}
        }

        match futures::ready!(self.tx.poll_reserve(cx)) {
            Ok(()) => {}
            Err(_) => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        };

        let num_bytes = std::cmp::min(MAX_FRAME_SIZE, buf.len());
        let frame = buf[..num_bytes].to_vec();

        match self.tx.send_item(Event::Message {
            payload: frame,
            flag: None,
        }) {
            Ok(()) => Poll::Ready(Ok(num_bytes)),
            Err(_) => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        // State machine for proper shutdown:
        // 1. Transition to Closing (stops accepting new writes)
        // 2. Flush pending data
        // 3. Send FIN flag
        // 4. Transition to FinSent
        // 5. Wait for FIN_ACK
        // 6. Transition to FinAcked and complete

        let current_state = *self.state.lock();

        match current_state {
            // Already received FIN_ACK, shutdown complete
            State::FinAcked => return Poll::Ready(Ok(())),

            // Sent FIN, waiting for FIN_ACK - poll timeout and return Pending
            State::FinSent => {
                // Poll the timeout - if it fires, force shutdown completion
                if let Some(timeout) = self.fin_ack_timeout.as_mut() {
                    if timeout.as_mut().poll(cx).is_ready() {
                        tracing::debug!(
                            target: "litep2p::webrtc::substream",
                            "FIN_ACK timeout exceeded, forcing shutdown completion"
                        );
                        *self.state.lock() = State::FinAcked;
                        return Poll::Ready(Ok(()));
                    }
                }

                // Store the waker so it can be triggered when we receive FIN_ACK
                self.shutdown_waker.register(cx.waker());
                return Poll::Pending;
            }

            // First call to shutdown - transition to Closing
            State::Open => {
                *self.state.lock() = State::Closing;
            }

            State::Closing => {
                // Already in closing state, continue with shutdown process
            }

            State::SendClosed => {
                // Remote closed send, we can still send FIN
            }
        }

        // Flush any pending data
        // Note: Currently poll_flush is a no-op, but the channel backpressure
        // provides implicit flushing since we wait for poll_reserve below
        futures::ready!(self.as_mut().poll_flush(cx))?;

        // Reserve space to send FIN
        match futures::ready!(self.tx.poll_reserve(cx)) {
            Ok(()) => {}
            Err(_) => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        };

        // Send message with FIN flag
        match self.tx.send_item(Event::Message {
            payload: vec![],
            flag: Some(Flag::Fin),
        }) {
            Ok(()) => {
                // Transition to FinSent after successfully sending FIN
                // Initialize the timeout for FIN_ACK
                *self.state.lock() = State::FinSent;
                let mut timeout = Box::pin(tokio::time::sleep(FIN_ACK_TIMEOUT));
                // Poll the timeout once to register it with tokio's timer
                // This ensures we'll be woken when it expires
                let _ = timeout.as_mut().poll(cx);
                self.fin_ack_timeout = Some(timeout);
                self.shutdown_waker.register(cx.waker());

                Poll::Pending
            }
            Err(_) => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
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
            handle.rx.recv().await,
            Some(Event::Message {
                payload: vec![0u8; MAX_FRAME_SIZE],
                flag: None,
            })
        );
        assert_eq!(
            handle.rx.recv().await,
            Some(Event::Message {
                payload: vec![0u8; MAX_FRAME_SIZE],
                flag: None,
            })
        );
        assert_eq!(
            handle.rx.recv().await,
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
        let (mut substream, handle) = Substream::new();
        *handle.state.lock() = State::SendClosed;

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
        let (mut substream, handle) = Substream::new();
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

        handle
            .inbound_tx
            .send(Event::Message {
                payload: first,
                flag: None,
            })
            .await
            .unwrap();
        handle
            .inbound_tx
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
        let _shutdown_task = tokio::spawn(async move {
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
        assert!(matches!(*handle.state.lock(), State::FinSent));
    }

    #[tokio::test]
    async fn fin_ack_response_on_receiving_fin() {
        let (mut substream, mut handle) = Substream::new();

        // Spawn task to consume inbound events sent to the substream
        let consumer_task = tokio::spawn(async move {
            // Substream should receive RecvClosed
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
        let (mut substream, handle) = Substream::new();

        // Spawn shutdown since it waits for FIN_ACK
        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
        });

        // Wait a bit for FIN to be sent
        tokio::task::yield_now().await;

        // Verify we're in FinSent state
        assert!(matches!(*handle.state.lock(), State::FinSent));

        // Simulate receiving FIN_ACK from remote
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        // Should transition to FinAcked
        assert!(matches!(*handle.state.lock(), State::FinAcked));

        // Shutdown should now complete
        shutdown_task.await.unwrap();
    }

    #[tokio::test]
    async fn full_fin_handshake() {
        let (mut substream, mut handle) = Substream::new();

        // Write some data
        substream.write_all(&vec![1u8; 100]).await.unwrap();

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
        assert!(matches!(*handle.state.lock(), State::FinAcked));

        // Shutdown should now complete
        shutdown_task.await.unwrap();
    }

    #[tokio::test]
    async fn stop_sending_flag_closes_send_half() {
        let (mut substream, handle) = Substream::new();

        // Simulate receiving STOP_SENDING
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::StopSending),
            })
            .await
            .unwrap();

        // Should transition to SendClosed
        assert!(matches!(*handle.state.lock(), State::SendClosed));

        // Attempting to write should fail
        match substream.write_all(&vec![0u8; 100]).await {
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe),
            _ => panic!("write should have failed"),
        }
    }

    #[tokio::test]
    async fn reset_stream_flag_returns_error() {
        let (_substream, handle) = Substream::new();

        // Simulate receiving RESET_STREAM
        let result = handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::ResetStream),
            })
            .await;

        // Should return connection closed error
        assert!(matches!(result, Err(Error::ConnectionClosed)));
    }

    #[tokio::test]
    async fn fin_ack_does_not_trigger_other_flag() {
        let (mut substream, handle) = Substream::new();

        // Spawn shutdown since it waits for FIN_ACK
        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
        });

        // Wait a bit for FIN to be sent
        tokio::task::yield_now().await;

        // Verify we're in FinSent state
        assert!(matches!(*handle.state.lock(), State::FinSent));

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
        assert!(matches!(*handle.state.lock(), State::FinAcked));

        // Shutdown should complete
        shutdown_task.await.unwrap();

        // Writing should still work (not closed by STOP_SENDING)
        // Note: We already sent FIN, so write won't actually work, but the state check happens
        // first
    }

    #[tokio::test]
    async fn flags_are_mutually_exclusive() {
        let (_substream, handle) = Substream::new();

        // Test that STOP_SENDING (1) is handled correctly
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::StopSending),
            })
            .await
            .unwrap();

        assert!(matches!(*handle.state.lock(), State::SendClosed));

        // Create a new substream for RESET_STREAM test
        let (_substream2, handle2) = Substream::new();

        // Test that RESET_STREAM (2) is handled correctly
        let result = handle2
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::ResetStream),
            })
            .await;

        assert!(matches!(result, Err(Error::ConnectionClosed)));

        // Create a new substream for FIN test
        let (mut substream3, handle3) = Substream::new();

        // Spawn shutdown since it waits for FIN_ACK
        let shutdown_task3 = tokio::spawn(async move {
            substream3.shutdown().await.unwrap();
        });

        // Wait a bit for FIN to be sent
        tokio::task::yield_now().await;

        // Test that FIN_ACK (3) is handled correctly
        handle3
            .on_message(WebRtcMessage {
                payload: None,
                flag: Some(Flag::FinAck),
            })
            .await
            .unwrap();

        assert!(matches!(*handle3.state.lock(), State::FinAcked));

        // Shutdown should complete
        shutdown_task3.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_rejects_new_writes() {
        use tokio::io::AsyncWriteExt;
        let (mut substream, mut handle) = Substream::new();

        // Write some data
        substream.write_all(&vec![1u8; 100]).await.unwrap();

        // Spawn shutdown in background
        let shutdown_task = tokio::spawn(async move {
            substream.shutdown().await.unwrap();
        });

        // Wait for data and FIN to be sent
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

        // Verify we transitioned through Closing to FinSent
        assert!(matches!(*handle.state.lock(), State::FinSent));

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
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );
        assert!(matches!(*handle.state.lock(), State::FinSent));

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
        assert!(matches!(*handle.state.lock(), State::FinAcked));
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
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::Fin)
            })
        );

        // Verify we're in FinSent state
        assert!(matches!(*handle.state.lock(), State::FinSent));

        // DON'T send FIN_ACK - let it timeout
        // The shutdown should complete after FIN_ACK_TIMEOUT (2 seconds in tests)
        // Add a bit of buffer to the timeout
        let result = timeout(Duration::from_secs(4), shutdown_task).await;

        assert!(result.is_ok(), "Shutdown should complete after timeout");
        assert!(
            result.unwrap().is_ok(),
            "Shutdown should succeed after timeout"
        );

        // Should have transitioned to FinAcked after timeout
        assert!(matches!(*handle.state.lock(), State::FinAcked));
    }

    #[tokio::test]
    async fn closing_state_blocks_writes() {
        use tokio::io::AsyncWriteExt;

        let (mut substream, handle) = Substream::new();

        // Manually transition to Closing state
        *handle.state.lock() = State::Closing;

        // Attempt to write should fail
        let result = substream.write_all(&vec![1u8; 100]).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
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
            Some(Event::Message {
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
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                    // Expected - read half closed by FIN
                }
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
            Some(Event::Message {
                payload: vec![],
                flag: Some(Flag::FinAck)
            })
        );

        // Wait for server to close substream
        server_task.await.unwrap();

        // Verify handle signals closure (returns None) - this is the key fix!
        assert_eq!(
            handle.next().await,
            None,
            "SubstreamHandle should signal closure after server receives FIN and drops Substream"
        );
    }
}
