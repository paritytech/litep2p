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

use crate::{peer_id::PeerId, types::protocol::ProtocolName};

use bytes::Bytes;
use futures::{Sink, SinkExt, Stream};

use std::{
    collections::HashMap,
    fmt::Debug,
    pin::Pin,
    task::{Context, Poll},
};

/// Trait which describes the behavior of a substream.
pub trait Substream: Debug + Stream + Sink<Bytes> + Unpin {
    /// Get protocol name.
    fn protocol(&self) -> &ProtocolName;
}

pub struct SubstreamSet<S: Substream> {
    substreams: HashMap<PeerId, S>,
}

impl<S: Substream> SubstreamSet<S> {
    // TODO: rewrite this
    async fn send_notification(&mut self, peer: PeerId, data: Bytes) -> Result<(), ()> {
        match self.substreams.get_mut(&peer) {
            Some(substream) => {
                substream.send(data).await;
                Ok(())
            }
            None => Err(()),
        }
    }
}

impl<S: Substream> Stream for SubstreamSet<S> {
    type Item = (PeerId, S::Item);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // TODO: pin project?
        let inner = Pin::into_inner(self);

        for (peer, mut substream) in inner.substreams.iter_mut() {
            match Pin::new(&mut substream).poll_next(cx) {
                Poll::Pending => continue,
                Poll::Ready(Some(data)) => return Poll::Ready(Some((*peer, data))),
                Poll::Ready(None) => return Poll::Ready(None), // TODO: remove substream from `substreams`
            }
        }

        Poll::Pending
    }
}
