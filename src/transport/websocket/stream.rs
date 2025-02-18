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

//! Stream implementation for `tokio_tungstenite::WebSocketStream` that implements
//! `AsyncRead + AsyncWrite`

use bytes::{Buf, Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};

use std::{
    pin::Pin,
    task::{Context, Poll},
};

// TODO: add tests

const DEFAULT_BUF_SIZE: usize = 8 * 1024;

/// Send state.
enum State {
    /// State is poisoned.
    Poisoned,

    /// Sink is accepting input.
    ReadyToSend,

    /// Flush is pending for the sink.
    FlushPending,
}

/// Buffered stream which implements `AsyncRead + AsyncWrite`
pub(super) struct BufferedStream<S: AsyncRead + AsyncWrite + Unpin> {
    /// Write buffer.
    write_buffer: BytesMut,

    /// Read buffer.
    ///
    /// The buffer is taken directly from the WebSocket stream.
    read_buffer: Bytes,

    /// Underlying WebSocket stream.
    stream: WebSocketStream<S>,

    /// Read state.
    state: State,
}

impl<S: AsyncRead + AsyncWrite + Unpin> BufferedStream<S> {
    /// Create new [`BufferedStream`].
    pub(super) fn new(stream: WebSocketStream<S>) -> Self {
        Self {
            write_buffer: BytesMut::with_capacity(DEFAULT_BUF_SIZE),
            read_buffer: Bytes::new(),
            stream,
            state: State::ReadyToSend,
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> futures::AsyncWrite for BufferedStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.write_buffer.extend_from_slice(buf);

        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.write_buffer.is_empty() {
            return self
                .stream
                .poll_ready_unpin(cx)
                .map_err(|_| std::io::ErrorKind::UnexpectedEof.into());
        }

        loop {
            match std::mem::replace(&mut self.state, State::Poisoned) {
                State::ReadyToSend => {
                    match self.stream.poll_ready_unpin(cx) {
                        Poll::Ready(Ok(())) => {}
                        Poll::Ready(Err(_error)) =>
                            return Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into())),
                        Poll::Pending => {
                            self.state = State::ReadyToSend;
                            return Poll::Pending;
                        }
                    }

                    let message = std::mem::take(&mut self.write_buffer);
                    match self.stream.start_send_unpin(Message::Binary(message.freeze())) {
                        Ok(()) => {}
                        Err(_error) =>
                            return Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into())),
                    }

                    // Transition to flush pending state.
                    self.state = State::FlushPending;
                    continue;
                }

                State::FlushPending => {
                    match self.stream.poll_flush_unpin(cx) {
                        Poll::Ready(Ok(())) => {}
                        Poll::Ready(Err(_error)) =>
                            return Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into())),
                        Poll::Pending => {
                            self.state = State::ReadyToSend;
                            return Poll::Pending;
                        }
                    }

                    self.state = State::ReadyToSend;
                    self.write_buffer = BytesMut::with_capacity(DEFAULT_BUF_SIZE);
                    return Poll::Ready(Ok(()));
                }
                State::Poisoned =>
                    return Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into())),
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match futures::ready!(self.stream.poll_close_unpin(cx)) {
            Ok(_) => Poll::Ready(Ok(())),
            Err(_) => Poll::Ready(Err(std::io::ErrorKind::PermissionDenied.into())),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> futures::AsyncRead for BufferedStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        loop {
            if self.read_buffer.is_empty() {
                let next_chunk = match self.stream.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(chunk))) => match chunk {
                        Message::Binary(chunk) => chunk,
                        _event => return Poll::Ready(Err(std::io::ErrorKind::Unsupported.into())),
                    },
                    Poll::Ready(Some(Err(_error))) =>
                        return Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into())),
                    Poll::Ready(None) => return Poll::Ready(Ok(0)),
                    Poll::Pending => return Poll::Pending,
                };

                self.read_buffer = next_chunk;
                continue;
            }

            let len = std::cmp::min(self.read_buffer.len(), buf.len());
            buf[..len].copy_from_slice(&self.read_buffer[..len]);
            self.read_buffer.advance(len);
            return Poll::Ready(Ok(len));
        }
    }
}
