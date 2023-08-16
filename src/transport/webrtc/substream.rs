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
    codec::{unsigned_varint::UnsignedVarint, ProtocolCodec},
    error::Error,
    transport::webrtc::{schema, util::WebRtcMessage},
};

use bytes::{Bytes, BytesMut};
use futures::{ready, Sink, SinkExt, Stream, StreamExt};
use prost::Message;
use tokio_util::codec::Framed;
use webrtc::data::data_channel::{DataChannel, PollDataChannel};

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

// TODO: calls need to respect the half-closed states

/// WebRTC substream.
#[derive(Debug)]
pub struct WebRtcSubstream {
    /// Inner I/O object.
    inner: Framed<PollDataChannel, UnsignedVarint>,

    /// Codec used by the protocol.
    codec: Option<ProtocolCodec>,
}

impl WebRtcSubstream {
    /// Create new [`Substream`].
    pub fn new(inner: Arc<DataChannel>) -> Self {
        Self {
            inner: Framed::new(PollDataChannel::new(inner), UnsignedVarint::default()),
            codec: None,
        }
    }

    /// Apply protocol codec for the substream.
    pub fn apply_codec(&mut self, codec: ProtocolCodec) {
        self.codec = Some(codec);
    }
}

impl Sink<Bytes> for WebRtcSubstream {
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready_unpin(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        let protobuf_payload = schema::webrtc::Message {
            message: (!item.is_empty()).then_some(item.into()),
            ..Default::default()
        };

        // TODO: create static buffer
        let mut payload = BytesMut::with_capacity(protobuf_payload.encoded_len());
        protobuf_payload
            .encode(&mut payload)
            .expect("Vec<u8> to provide needed capacity");

        self.inner.start_send_unpin(payload.freeze())
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
        match ready!(self.inner.poll_next_unpin(cx)) {
            Some(Ok(message)) => match schema::webrtc::Message::decode(message) {
                Err(_) => Poll::Ready(None),
                Ok(message) => {
                    // TODO: handle flags

                    match message.message {
                        Some(message) => match self.codec {
                            Some(ProtocolCodec::Identity(_)) | None => {
                                Poll::Ready(Some(Ok(BytesMut::from(&message[..]))))
                            }
                            Some(ProtocolCodec::UnsignedVarint) => Poll::Ready(Some(
                                UnsignedVarint::decode(&mut BytesMut::from(&message[..])),
                            )),
                        },
                        None => return Poll::Ready(None),
                    }
                }
            },
            Some(Err(_)) => return Poll::Ready(None),
            None => return Poll::Ready(None),
        }
    }
}
