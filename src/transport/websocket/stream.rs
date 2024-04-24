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

use bytes::{Buf, Bytes};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};

use std::{
    pin::Pin,
    task::{Context, Poll},
};

// TODO: add tests

/// Send state.
enum State {
    /// State is poisoned.
    Poisoned,

    /// Sink is accepting input.
    ReadyToSend,

    /// Sink is ready to send.
    ReadyPending { to_write: Vec<u8> },

    /// Flush is pending for the sink.
    FlushPending,
}

/// Buffered stream which implements `AsyncRead + AsyncWrite`
pub(super) struct BufferedStream<S: AsyncRead + AsyncWrite + Unpin> {
    /// Write buffer.
    write_buffer: Vec<u8>,

    /// Write pointer.
    write_ptr: usize,

    // Read buffer.
    read_buffer: Option<Bytes>,

    /// Underlying WebSocket stream.
    stream: WebSocketStream<S>,

    /// Read state.
    state: State,
}

impl<S: AsyncRead + AsyncWrite + Unpin> BufferedStream<S> {
    /// Create new [`BufferedStream`].
    pub(super) fn new(stream: WebSocketStream<S>) -> Self {
        Self {
            write_buffer: Vec::with_capacity(2000),
            read_buffer: None,
            write_ptr: 0usize,
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
        self.write_ptr += buf.len();

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
                    let message = self.write_buffer[..self.write_ptr].to_vec();
                    self.state = State::ReadyPending { to_write: message };

                    match futures::ready!(self.stream.poll_ready_unpin(cx)) {
                        Ok(()) => continue,
                        Err(_error) => {
                            return Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into()));
                        }
                    }
                }
                State::ReadyPending { to_write } => {
                    match self.stream.start_send_unpin(Message::Binary(to_write.clone())) {
                        Ok(_) => {
                            self.state = State::FlushPending;
                            continue;
                        }
                        Err(_error) =>
                            return Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into())),
                    }
                }
                State::FlushPending => match futures::ready!(self.stream.poll_flush_unpin(cx)) {
                    Ok(_res) => {
                        // TODO: optimize
                        self.state = State::ReadyToSend;
                        self.write_ptr = 0;
                        self.write_buffer = Vec::with_capacity(2000);
                        return Poll::Ready(Ok(()));
                    }
                    Err(_) => return Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into())),
                },
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
            if self.read_buffer.is_none() {
                match self.stream.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(chunk))) => match chunk {
                        Message::Binary(chunk) => self.read_buffer.replace(chunk.into()),
                        _event => return Poll::Ready(Err(std::io::ErrorKind::Unsupported.into())),
                    },
                    Poll::Ready(Some(Err(_error))) =>
                        return Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into())),
                    Poll::Ready(None) => return Poll::Ready(Ok(0)),
                    Poll::Pending => return Poll::Pending,
                };
            }

            let buffer = self.read_buffer.as_mut().expect("buffer to exist");
            let bytes_read = buf.len().min(buffer.len());
            let _orig_size = buffer.len();
            buf[..bytes_read].copy_from_slice(&buffer[..bytes_read]);

            buffer.advance(bytes_read);

            // TODO: this can't be correct
            if !buffer.is_empty() || bytes_read != 0 {
                return Poll::Ready(Ok(bytes_read));
            } else {
                self.read_buffer.take();
            }
        }
    }
}
