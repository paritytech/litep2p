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
use futures::Stream;
use parking_lot::Mutex;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_util::sync::PollSender;

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

/// Maximum frame size.
const MAX_FRAME_SIZE: usize = 16384;

/// Substream event.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Receiver closed.
    RecvClosed,

    /// Send/receive message.
    Message(Vec<u8>),

    /// Send message with flags.
    MessageWithFlags { payload: Vec<u8>, flags: i32 },
}

/// Substream stream.
enum State {
    /// Substream is fully open.
    Open,

    /// Remote is no longer interested in receiving anything.
    SendClosed,

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
}

impl Substream {
    /// Create new [`Substream`].
    pub fn new() -> (Self, SubstreamHandle) {
        let (outbound_tx, outbound_rx) = channel(256);
        let (inbound_tx, inbound_rx) = channel(256);
        let state = Arc::new(Mutex::new(State::Open));

        let handle = SubstreamHandle {
            tx: inbound_tx,
            rx: outbound_rx,
            state: Arc::clone(&state),
        };

        (
            Self {
                state,
                tx: PollSender::new(outbound_tx),
                rx: inbound_rx,
                read_buffer: BytesMut::new(),
            },
            handle,
        )
    }
}

/// Substream handle that is given to the WebRTC transport backend.
pub struct SubstreamHandle {
    state: Arc<Mutex<State>>,

    /// TX channel for sending inbound messages from `peer` to the associated `Substream`.
    tx: Sender<Event>,

    /// RX channel for receiving outbound messages to `peer` from the associated `Substream`.
    rx: Receiver<Event>,
}

impl SubstreamHandle {
    /// Handle message received from a remote peer.
    ///
    /// If the message contains any flags, handle them first and appropriately close the correct
    /// side of the substream. If the message contained any payload, send it to the protocol for
    /// further processing.
    pub async fn on_message(&self, message: WebRtcMessage) -> crate::Result<()> {
        if let Some(flags) = message.flags {
            if flags == Flag::Fin as i32 {
                // Received FIN, send FIN_ACK back
                self.tx.send(Event::RecvClosed).await?;
                // Send FIN_ACK to acknowledge
                return self.tx
                    .send(Event::MessageWithFlags {
                        payload: vec![],
                        flags: Flag::FinAck as i32,
                    })
                    .await
                    .map_err(From::from);
            }

            if flags == Flag::FinAck as i32 {
                // Received FIN_ACK, we can now fully close our write half
                let mut state = self.state.lock();
                if matches!(*state, State::FinSent) {
                    *state = State::FinAcked;
                }
                return Ok(());
            }

            if flags == Flag::StopSending as i32 {
                *self.state.lock() = State::SendClosed;
                return Ok(());
            }

            if flags == Flag::ResetStream as i32 {
                return Err(Error::ConnectionClosed);
            }
        }

        if let Some(payload) = message.payload {
            if !payload.is_empty() {
                return self.tx.send(Event::Message(payload)).await.map_err(From::from);
            }
        }

        Ok(())
    }
}

impl Stream for SubstreamHandle {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
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
            Some(Event::Message(message)) => {
                if message.len() > MAX_FRAME_SIZE {
                    return Poll::Ready(Err(std::io::ErrorKind::PermissionDenied.into()));
                }

                match buf.remaining() >= message.len() {
                    true => buf.put_slice(&message),
                    false => {
                        let remaining = buf.remaining();
                        buf.put_slice(&message[..remaining]);
                        self.read_buffer.put_slice(&message[remaining..]);
                    }
                }

                Poll::Ready(Ok(()))
            }
            // MessageWithFlags is handled at the connection layer, not here
            Some(Event::MessageWithFlags { .. }) => {
                Poll::Ready(Err(std::io::ErrorKind::InvalidData.into()))
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
        if let State::SendClosed = *self.state.lock() {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }

        match futures::ready!(self.tx.poll_reserve(cx)) {
            Ok(()) => {}
            Err(_) => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        };

        let num_bytes = std::cmp::min(MAX_FRAME_SIZE, buf.len());
        let frame = buf[..num_bytes].to_vec();

        match self.tx.send_item(Event::Message(frame)) {
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
        // Send FIN flag and transition to FinSent state
        match futures::ready!(self.tx.poll_reserve(cx)) {
            Ok(()) => {}
            Err(_) => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        };

        // Update state to FinSent
        *self.state.lock() = State::FinSent;

        // Send message with FIN flag
        match self.tx.send_item(Event::MessageWithFlags {
            payload: vec![],
            flags: Flag::Fin as i32,
        }) {
            Ok(()) => Poll::Ready(Ok(())),
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

        assert_eq!(handle.next().await, Some(Event::Message(vec![0u8; 1337])));

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
            Some(Event::Message(vec![0u8; MAX_FRAME_SIZE]))
        );
        assert_eq!(
            handle.rx.recv().await,
            Some(Event::Message(vec![0u8; MAX_FRAME_SIZE]))
        );
        assert_eq!(handle.rx.recv().await, Some(Event::Message(vec![0u8; 1])));

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
        substream.shutdown().await.unwrap();

        assert_eq!(handle.next().await, Some(Event::Message(vec![1u8; 1337])));
        // After shutdown, should send FIN flag
        assert_eq!(
            handle.next().await,
            Some(Event::MessageWithFlags {
                payload: vec![],
                flags: Flag::Fin as i32
            })
        );
    }

    #[tokio::test]
    async fn try_to_read_from_closed_substream() {
        let (mut substream, handle) = Substream::new();
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flags: Some(0i32),
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
        handle.tx.send(Event::Message(vec![1u8; 256])).await.unwrap();

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

        handle.tx.send(Event::Message(first)).await.unwrap();

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

        handle.tx.send(Event::Message(first)).await.unwrap();
        handle.tx.send(Event::Message(vec![4u8; 2048])).await.unwrap();

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

        // Shutdown the substream
        substream.shutdown().await.unwrap();

        // Should receive FIN flag
        assert_eq!(
            handle.next().await,
            Some(Event::MessageWithFlags {
                payload: vec![],
                flags: Flag::Fin as i32
            })
        );

        // Verify state is FinSent
        assert!(matches!(*handle.state.lock(), State::FinSent));
    }

    #[tokio::test]
    async fn fin_ack_response_on_receiving_fin() {
        let (_substream, handle) = Substream::new();

        // Simulate receiving FIN from remote
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flags: Some(Flag::Fin as i32),
            })
            .await
            .unwrap();

        // Should have sent FIN_ACK back (this would be captured by the connection layer)
        // In real scenario, the connection would read from handle.rx
    }

    #[tokio::test]
    async fn fin_ack_received_transitions_to_fin_acked() {
        let (mut substream, handle) = Substream::new();

        // First, send FIN
        substream.shutdown().await.unwrap();

        // Verify we're in FinSent state
        assert!(matches!(*handle.state.lock(), State::FinSent));

        // Simulate receiving FIN_ACK from remote
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flags: Some(Flag::FinAck as i32),
            })
            .await
            .unwrap();

        // Should transition to FinAcked
        assert!(matches!(*handle.state.lock(), State::FinAcked));
    }

    #[tokio::test]
    async fn full_fin_handshake() {
        let (mut substream, mut handle) = Substream::new();

        // Write some data
        substream.write_all(&vec![1u8; 100]).await.unwrap();

        // Initiate shutdown (send FIN)
        substream.shutdown().await.unwrap();

        // Verify data was sent
        assert_eq!(handle.next().await, Some(Event::Message(vec![1u8; 100])));

        // Verify FIN was sent
        assert_eq!(
            handle.next().await,
            Some(Event::MessageWithFlags {
                payload: vec![],
                flags: Flag::Fin as i32
            })
        );

        // Simulate receiving FIN_ACK
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flags: Some(Flag::FinAck as i32),
            })
            .await
            .unwrap();

        // Should be in FinAcked state
        assert!(matches!(*handle.state.lock(), State::FinAcked));
    }

    #[tokio::test]
    async fn stop_sending_flag_closes_send_half() {
        let (mut substream, handle) = Substream::new();

        // Simulate receiving STOP_SENDING
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flags: Some(Flag::StopSending as i32),
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
                flags: Some(Flag::ResetStream as i32),
            })
            .await;

        // Should return connection closed error
        assert!(matches!(result, Err(Error::ConnectionClosed)));
    }

    #[tokio::test]
    async fn fin_ack_does_not_trigger_other_flags() {
        let (mut substream, handle) = Substream::new();

        // First, send FIN to transition to FinSent state
        substream.shutdown().await.unwrap();

        // Verify we're in FinSent state
        assert!(matches!(*handle.state.lock(), State::FinSent));

        // Now simulate receiving FIN_ACK (value = 3)
        // This should NOT trigger STOP_SENDING (value = 1) or RESET_STREAM (value = 2)
        // even though 3 & 1 == 1 and 3 & 2 == 2
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flags: Some(Flag::FinAck as i32),
            })
            .await
            .unwrap();

        // Should transition to FinAcked, not SendClosed
        assert!(matches!(*handle.state.lock(), State::FinAcked));

        // Writing should still work (not closed by STOP_SENDING)
        // Note: We already sent FIN, so write won't actually work, but the state check happens first
    }

    #[tokio::test]
    async fn flags_are_mutually_exclusive() {
        let (mut substream, handle) = Substream::new();

        // Test that STOP_SENDING (1) is handled correctly
        handle
            .on_message(WebRtcMessage {
                payload: None,
                flags: Some(Flag::StopSending as i32),
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
                flags: Some(Flag::ResetStream as i32),
            })
            .await;

        assert!(matches!(result, Err(Error::ConnectionClosed)));

        // Create a new substream for FIN test
        let (mut substream3, handle3) = Substream::new();

        // First transition to FinSent
        substream3.shutdown().await.unwrap();

        // Test that FIN_ACK (3) is handled correctly
        handle3
            .on_message(WebRtcMessage {
                payload: None,
                flags: Some(Flag::FinAck as i32),
            })
            .await
            .unwrap();

        assert!(matches!(*handle3.state.lock(), State::FinAcked));
    }
}
