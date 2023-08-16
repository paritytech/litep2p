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

use crate::{codec::unsigned_varint::UnsignedVarint, error::Error};

use bytes::{Bytes, BytesMut};
use futures::{AsyncRead, AsyncWrite, Sink, SinkExt, Stream, StreamExt};
use tokio_util::codec::Framed;
use webrtc::data::data_channel::{DataChannel, PollDataChannel};

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

/// WebRTC substream.
pub struct WebRtcSubstream {
    inner: Framed<PollDataChannel, UnsignedVarint>,
}

impl WebRtcSubstream {
    /// Create new [`Substream`].
    pub fn new(inner: Arc<DataChannel>) -> Self {
        let inner = Framed::new(PollDataChannel::new(inner), UnsignedVarint::default());

        Self { inner }
    }
}

impl Sink<BytesMut> for WebRtcSubstream {
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready_unpin(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: BytesMut) -> Result<(), Self::Error> {
        self.inner.start_send_unpin(item.freeze())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_flush_unpin(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_close_unpin(cx)
    }
}

impl Stream for WebRtcSubstream {
    type Item = crate::Result<BytesMut>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}
