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

use crate::{error::Error, peer_id::PeerId};

use bytes::{Bytes, BytesMut};
use futures::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite};

use std::{
    collections::HashMap,
    fmt::Debug,
    pin::Pin,
    task::{Context, Poll},
};

pub mod mock;

/// Raw substream received from one of the enabled transports.
pub trait RawSubstream: AsyncRead + AsyncWrite + Unpin + Send + Debug + 'static {}

/// Blanket implementation for [`RawSubstream`].
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Debug + 'static> RawSubstream for T {}

/// Trait which describes the behavior of a substream.
pub trait Substream:
    Debug + Stream<Item = crate::Result<BytesMut>> + Sink<Bytes, Error = Error> + Send + Unpin + 'static
{
}

/// Blanket implementation for [`Substream`].
impl<
        T: Debug
            + Stream<Item = crate::Result<BytesMut>>
            + Sink<Bytes, Error = Error>
            + Send
            + Unpin
            + 'static,
    > Substream for T
{
}

pub struct SubstreamSet<S: Substream> {
    substreams: HashMap<PeerId, S>,
}

impl<S: Substream> SubstreamSet<S> {
    /// Create new [`SubstreamSet`].
    pub fn new() -> Self {
        Self {
            substreams: HashMap::new(),
        }
    }
}

impl<S: Substream> Default for SubstreamSet<S> {
    fn default() -> Self {
        Self::new()
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
