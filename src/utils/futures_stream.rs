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

use futures::{stream::FuturesUnordered, Stream, StreamExt};

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

/// Wrapper around [`FuturesUnordered`] that wakes a task up automatically.
/// The [`Stream`] implemented by [`FuturesStream`] never terminates and can be
/// polled when contains no futures.
#[derive(Default)]
pub struct FuturesStream<F> {
    futures: FuturesUnordered<F>,
    waker: Option<Waker>,
}

impl<F> FuturesStream<F> {
    /// Create new [`FuturesStream`].
    pub fn new() -> Self {
        Self {
            futures: FuturesUnordered::new(),
            waker: None,
        }
    }

    /// Number of futures in the stream.
    pub fn len(&self) -> usize {
        self.futures.len()
    }

    /// Check if the stream is empty.
    pub fn is_empty(&self) -> bool {
        self.futures.is_empty()
    }

    /// Push a future for processing.
    pub fn push(&mut self, future: F) {
        self.futures.push(future);

        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

impl<F: Future> Stream for FuturesStream<F> {
    type Item = <F as Future>::Output;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Poll::Ready(Some(result)) = self.futures.poll_next_unpin(cx) else {
            // We must save the current waker to wake up the task when new futures are inserted.
            //
            // Otherwise, simply returning `Poll::Pending` here would cause the task to never be
            // woken up again.
            //
            // We were previously relying on some other task from the `loop tokio::select!` to
            // finish.
            self.waker = Some(cx.waker().clone());

            return Poll::Pending;
        };

        Poll::Ready(Some(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        error::SubstreamError, mock::substream::MockSubstream, utils::futures_stream::FuturesStream,
    };
    use bytes::BytesMut;
    use futures::StreamExt;

    async fn get_data_from_substream(
        mut substream: MockSubstream,
    ) -> (Result<BytesMut, SubstreamError>, MockSubstream) {
        let request = match substream.next().await {
            Some(Ok(request)) => Ok(request),
            Some(Err(error)) => Err(error),
            None => Err(SubstreamError::ConnectionClosed),
        };

        (request, substream)
    }

    #[test]
    fn add_substream() {
        let mut set = FuturesStream::new();

        let substream = MockSubstream::new();
        set.push(get_data_from_substream(substream));

        let substream = MockSubstream::new();
        set.push(get_data_from_substream(substream));
    }

    #[tokio::test]
    async fn poll_data_from_substream() {
        let mut set = FuturesStream::new();

        let mut substream = MockSubstream::new();
        substream
            .expect_poll_next()
            .times(1)
            .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"hello"[..])))));
        substream
            .expect_poll_next()
            .times(1)
            .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"world"[..])))));
        substream.expect_poll_next().returning(|_| Poll::Pending);

        set.push(get_data_from_substream(substream));
        let (value, substream) = set.next().await.unwrap();
        assert_eq!(value.unwrap(), BytesMut::from(&b"hello"[..]));

        set.push(get_data_from_substream(substream));
        let (value, _substream) = set.next().await.unwrap();
        assert_eq!(value.unwrap(), BytesMut::from(&b"world"[..]));

        assert!(futures::poll!(set.next()).is_pending());
    }

    #[tokio::test]
    async fn substream_closed() {
        let mut set = FuturesStream::new();

        let mut substream = MockSubstream::new();
        substream
            .expect_poll_next()
            .times(1)
            .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"hello"[..])))));
        substream.expect_poll_next().times(1).return_once(|_| Poll::Ready(None));
        substream.expect_poll_next().returning(|_| Poll::Pending);

        set.push(get_data_from_substream(substream));

        let (value, substream) = set.next().await.unwrap();
        assert_eq!(value.unwrap(), BytesMut::from(&b"hello"[..]));

        set.push(get_data_from_substream(substream));
        let (value, _substream) = set.next().await.unwrap();
        assert_eq!(value, Err(SubstreamError::ConnectionClosed));
    }

    #[tokio::test]
    async fn poll_data_from_two_substreams() {
        let mut set = FuturesStream::new();

        // prepare first substream
        let mut substream1 = MockSubstream::new();
        substream1
            .expect_poll_next()
            .times(1)
            .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"hello"[..])))));
        substream1
            .expect_poll_next()
            .times(1)
            .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"world"[..])))));
        substream1.expect_poll_next().returning(|_| Poll::Pending);
        set.push(get_data_from_substream(substream1));

        // prepare second substream
        let mut substream2 = MockSubstream::new();
        substream2
            .expect_poll_next()
            .times(1)
            .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"siip"[..])))));
        substream2
            .expect_poll_next()
            .times(1)
            .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"huup"[..])))));
        substream2.expect_poll_next().returning(|_| Poll::Pending);
        set.push(get_data_from_substream(substream2));

        let expected: Vec<Vec<BytesMut>> = vec![
            vec![
                BytesMut::from(&b"hello"[..]),
                BytesMut::from(&b"world"[..]),
                BytesMut::from(&b"siip"[..]),
                BytesMut::from(&b"huup"[..]),
            ],
            vec![
                BytesMut::from(&b"hello"[..]),
                BytesMut::from(&b"siip"[..]),
                BytesMut::from(&b"world"[..]),
                BytesMut::from(&b"huup"[..]),
            ],
            vec![
                BytesMut::from(&b"siip"[..]),
                BytesMut::from(&b"huup"[..]),
                BytesMut::from(&b"hello"[..]),
                BytesMut::from(&b"world"[..]),
            ],
            vec![
                BytesMut::from(&b"hello"[..]),
                BytesMut::from(&b"siip"[..]),
                BytesMut::from(&b"huup"[..]),
                BytesMut::from(&b"world"[..]),
            ],
        ];

        // poll values
        let mut values = Vec::new();

        for _ in 0..4 {
            let (value, substream) = set.next().await.unwrap();
            values.push(value.unwrap());
            set.push(get_data_from_substream(substream));
        }

        let mut correct_found = false;

        for set in expected {
            if values == set {
                correct_found = true;
                break;
            }
        }

        if !correct_found {
            panic!("invalid set generated");
        }

        // rest of the calls return `Poll::Pending`
        for _ in 0..10 {
            assert!(futures::poll!(set.next()).is_pending());
        }
    }
}
