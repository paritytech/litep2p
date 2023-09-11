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

//! Channel-backed substream.

use crate::{
    codec::{generic::Generic, identity::Identity, unsigned_varint::UnsignedVarint, ProtocolCodec},
    error::Error,
};

use bytes::BytesMut;
use futures::{Sink, Stream};
use str0m::channel::ChannelId;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::PollSender;

use std::{
    pin::Pin,
    task::{Context, Poll},
};

// TODO: use substream id

/// Channel-backed substream.
#[derive(Debug)]
pub struct Substream {
    /// Channel ID.
    id: ChannelId,

    /// TX channel for sending messages to transport.
    tx: PollSender<(ChannelId, Vec<u8>)>,

    /// RX channel for receiving messages from transport.
    rx: ReceiverStream<Vec<u8>>,

    /// Protocol codec.
    codec: Option<ProtocolCodec>,
}

impl Substream {
    /// Create new [`Substream`].
    pub fn new(id: ChannelId, tx: Sender<(ChannelId, Vec<u8>)>) -> (Self, Sender<Vec<u8>>) {
        let (to_protocol, rx) = channel(64);

        (
            Self {
                id,
                codec: None,
                tx: PollSender::new(tx),
                rx: ReceiverStream::new(rx),
            },
            to_protocol,
        )
    }

    /// Apply codec for the substream.
    pub fn apply_codec(&mut self, codec: ProtocolCodec) {
        self.codec = Some(codec);
    }
}

impl Sink<bytes::Bytes> for Substream {
    type Error = Error;

    fn poll_ready<'a>(mut self: Pin<&mut Self>, cx: &mut Context<'a>) -> Poll<Result<(), Error>> {
        let pinned = Pin::new(&mut self.tx);
        pinned.poll_ready(cx).map_err(|_| Error::Unknown)
    }

    fn start_send(mut self: Pin<&mut Self>, item: bytes::Bytes) -> Result<(), Error> {
        let item: Vec<u8> = match self.codec.as_ref().expect("codec to exist") {
            ProtocolCodec::Identity(_) => Identity::encode(item)?.into(),
            ProtocolCodec::UnsignedVarint(_) => UnsignedVarint::encode(item)?.into(),
            ProtocolCodec::Generic => Generic::encode(item)?.into(),
        };
        let id = self.id;

        Pin::new(&mut self.tx).start_send((id, item)).map_err(|_| Error::Unknown)
    }

    fn poll_flush<'a>(mut self: Pin<&mut Self>, cx: &mut Context<'a>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.tx).poll_flush(cx).map_err(|_| Error::Unknown)
    }

    fn poll_close<'a>(mut self: Pin<&mut Self>, cx: &mut Context<'a>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.tx).poll_close(cx).map_err(|_| Error::Unknown)
    }
}

impl Stream for Substream {
    type Item = crate::Result<BytesMut>;

    fn poll_next<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'a>,
    ) -> Poll<Option<crate::Result<BytesMut>>> {
        match Pin::new(&mut self.rx).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(value)) => Poll::Ready(Some(Ok(BytesMut::from(value.as_slice())))),
        }
    }
}

// TODO: rename?
pub struct SubstreamBackend {
    /// TX channel for creating new [`Substream`] objects.
    tx: Sender<(ChannelId, Vec<u8>)>,

    /// RX channel for receiving messages from protocols.
    rx: Receiver<(ChannelId, Vec<u8>)>,
}

impl SubstreamBackend {
    /// Create new [`SubstreamBackend`].
    pub fn new() -> Self {
        let (tx, rx) = channel(1024);

        Self { tx, rx }
    }

    /// Create new substream.
    pub fn substream(&mut self, id: ChannelId) -> (Substream, Sender<Vec<u8>>) {
        Substream::new(id, self.tx.clone())
    }

    /// Poll next event.
    pub async fn next_event(&mut self) -> Option<(ChannelId, Vec<u8>)> {
        self.rx.recv().await
    }
}
