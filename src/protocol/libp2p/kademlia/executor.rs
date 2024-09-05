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

use crate::{protocol::libp2p::kademlia::query::QueryId, substream::Substream, PeerId};

use bytes::{Bytes, BytesMut};
use futures::{future::BoxFuture, stream::FuturesUnordered, Stream, StreamExt};

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
    time::Duration,
};

/// Read timeout for inbound messages.
const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Query result.
#[derive(Debug)]
pub enum QueryResult {
    /// Message was sent to remote peer successfully.
    SendSuccess {
        /// Substream.
        substream: Substream,
    },

    /// Message was read from the remote peer successfully.
    ReadSuccess {
        /// Substream.
        substream: Substream,

        /// Read message.
        message: BytesMut,
    },

    /// Timeout while reading a response from the substream.
    Timeout,

    /// Substream was closed wile reading/writing message to remote peer.
    SubstreamClosed,
}

/// Query result.
#[derive(Debug)]
pub struct QueryContext {
    /// Peer ID.
    pub peer: PeerId,

    /// Query ID.
    pub query_id: Option<QueryId>,

    /// Query result.
    pub result: QueryResult,
}

/// Wrapper around [`FuturesUnordered`] that wakes a task up automatically.
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

/// Query executor.
pub struct QueryExecutor {
    /// Pending futures.
    futures: FuturesStream<BoxFuture<'static, QueryContext>>,
}

impl QueryExecutor {
    /// Create new [`QueryExecutor`]
    pub fn new() -> Self {
        Self {
            futures: FuturesStream::new(),
        }
    }

    /// Send message to remote peer.
    pub fn send_message(&mut self, peer: PeerId, message: Bytes, mut substream: Substream) {
        self.futures.push(Box::pin(async move {
            match substream.send_framed(message).await {
                Ok(_) => QueryContext {
                    peer,
                    query_id: None,
                    result: QueryResult::SendSuccess { substream },
                },
                Err(_) => QueryContext {
                    peer,
                    query_id: None,
                    result: QueryResult::SubstreamClosed,
                },
            }
        }));
    }

    /// Read message from remote peer with timeout.
    pub fn read_message(
        &mut self,
        peer: PeerId,
        query_id: Option<QueryId>,
        mut substream: Substream,
    ) {
        self.futures.push(Box::pin(async move {
            match tokio::time::timeout(READ_TIMEOUT, substream.next()).await {
                Err(_) => QueryContext {
                    peer,
                    query_id,
                    result: QueryResult::Timeout,
                },
                Ok(Some(Ok(message))) => QueryContext {
                    peer,
                    query_id,
                    result: QueryResult::ReadSuccess { substream, message },
                },
                Ok(None) | Ok(Some(Err(_))) => QueryContext {
                    peer,
                    query_id,
                    result: QueryResult::SubstreamClosed,
                },
            }
        }));
    }

    /// Send request to remote peer and read response.
    pub fn send_request_read_response(
        &mut self,
        peer: PeerId,
        query_id: Option<QueryId>,
        message: Bytes,
        mut substream: Substream,
    ) {
        self.futures.push(Box::pin(async move {
            if let Err(_) = substream.send_framed(message).await {
                let _ = substream.close().await;
                return QueryContext {
                    peer,
                    query_id,
                    result: QueryResult::SubstreamClosed,
                };
            }

            match tokio::time::timeout(READ_TIMEOUT, substream.next()).await {
                Err(_) => QueryContext {
                    peer,
                    query_id,
                    result: QueryResult::Timeout,
                },
                Ok(Some(Ok(message))) => QueryContext {
                    peer,
                    query_id,
                    result: QueryResult::ReadSuccess { substream, message },
                },
                Ok(None) | Ok(Some(Err(_))) => QueryContext {
                    peer,
                    query_id,
                    result: QueryResult::SubstreamClosed,
                },
            }
        }));
    }
}

impl Stream for QueryExecutor {
    type Item = QueryContext;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.futures.poll_next_unpin(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{mock::substream::MockSubstream, types::SubstreamId};

    #[tokio::test]
    async fn substream_read_timeout() {
        let mut executor = QueryExecutor::new();
        let peer = PeerId::random();
        let mut substream = MockSubstream::new();
        substream.expect_poll_next().returning(|_| Poll::Pending);
        let substream = Substream::new_mock(peer, SubstreamId::from(0usize), Box::new(substream));

        executor.read_message(peer, None, substream);

        match tokio::time::timeout(Duration::from_secs(20), executor.next()).await {
            Ok(Some(QueryContext {
                peer: queried_peer,
                query_id,
                result,
            })) => {
                assert_eq!(peer, queried_peer);
                assert!(query_id.is_none());
                assert!(std::matches!(result, QueryResult::Timeout));
            }
            result => panic!("invalid result received: {result:?}"),
        }
    }

    #[tokio::test]
    async fn substream_read_substream_closed() {
        let mut executor = QueryExecutor::new();
        let peer = PeerId::random();
        let mut substream = MockSubstream::new();
        substream.expect_poll_next().times(1).return_once(|_| {
            Poll::Ready(Some(Err(crate::error::SubstreamError::ConnectionClosed)))
        });

        executor.read_message(
            peer,
            Some(QueryId(1338)),
            Substream::new_mock(peer, SubstreamId::from(0usize), Box::new(substream)),
        );

        match tokio::time::timeout(Duration::from_secs(20), executor.next()).await {
            Ok(Some(QueryContext {
                peer: queried_peer,
                query_id,
                result,
            })) => {
                assert_eq!(peer, queried_peer);
                assert_eq!(query_id, Some(QueryId(1338)));
                assert!(std::matches!(result, QueryResult::SubstreamClosed));
            }
            result => panic!("invalid result received: {result:?}"),
        }
    }

    #[tokio::test]
    async fn send_succeeds_no_message_read() {
        let mut executor = QueryExecutor::new();
        let peer = PeerId::random();

        // prepare substream which succeeds in sending the message but closes right after
        let mut substream = MockSubstream::new();
        substream.expect_poll_ready().times(1).return_once(|_| Poll::Ready(Ok(())));
        substream.expect_start_send().times(1).return_once(|_| Ok(()));
        substream.expect_poll_flush().times(1).return_once(|_| Poll::Ready(Ok(())));
        substream.expect_poll_next().times(1).return_once(|_| {
            Poll::Ready(Some(Err(crate::error::SubstreamError::ConnectionClosed)))
        });

        executor.send_request_read_response(
            peer,
            Some(QueryId(1337)),
            Bytes::from_static(b"hello, world"),
            Substream::new_mock(peer, SubstreamId::from(0usize), Box::new(substream)),
        );

        match tokio::time::timeout(Duration::from_secs(20), executor.next()).await {
            Ok(Some(QueryContext {
                peer: queried_peer,
                query_id,
                result,
            })) => {
                assert_eq!(peer, queried_peer);
                assert_eq!(query_id, Some(QueryId(1337)));
                assert!(std::matches!(result, QueryResult::SubstreamClosed));
            }
            result => panic!("invalid result received: {result:?}"),
        }
    }

    #[tokio::test]
    async fn send_fails_no_message_read() {
        let mut executor = QueryExecutor::new();
        let peer = PeerId::random();

        // prepare substream which succeeds in sending the message but closes right after
        let mut substream = MockSubstream::new();
        substream
            .expect_poll_ready()
            .times(1)
            .return_once(|_| Poll::Ready(Err(crate::error::SubstreamError::ConnectionClosed)));
        substream.expect_poll_close().times(1).return_once(|_| Poll::Ready(Ok(())));

        executor.send_request_read_response(
            peer,
            Some(QueryId(1337)),
            Bytes::from_static(b"hello, world"),
            Substream::new_mock(peer, SubstreamId::from(0usize), Box::new(substream)),
        );

        match tokio::time::timeout(Duration::from_secs(20), executor.next()).await {
            Ok(Some(QueryContext {
                peer: queried_peer,
                query_id,
                result,
            })) => {
                assert_eq!(peer, queried_peer);
                assert_eq!(query_id, Some(QueryId(1337)));
                assert!(std::matches!(result, QueryResult::SubstreamClosed));
            }
            result => panic!("invalid result received: {result:?}"),
        }
    }

    #[tokio::test]
    async fn read_message_timeout() {
        let mut executor = QueryExecutor::new();
        let peer = PeerId::random();

        // prepare substream which succeeds in sending the message but closes right after
        let mut substream = MockSubstream::new();
        substream.expect_poll_next().returning(|_| Poll::Pending);

        executor.read_message(
            peer,
            Some(QueryId(1336)),
            Substream::new_mock(peer, SubstreamId::from(0usize), Box::new(substream)),
        );

        match tokio::time::timeout(Duration::from_secs(20), executor.next()).await {
            Ok(Some(QueryContext {
                peer: queried_peer,
                query_id,
                result,
            })) => {
                assert_eq!(peer, queried_peer);
                assert_eq!(query_id, Some(QueryId(1336)));
                assert!(std::matches!(result, QueryResult::Timeout));
            }
            result => panic!("invalid result received: {result:?}"),
        }
    }

    #[tokio::test]
    async fn read_message_substream_closed() {
        let mut executor = QueryExecutor::new();
        let peer = PeerId::random();

        // prepare substream which succeeds in sending the message but closes right after
        let mut substream = MockSubstream::new();
        substream
            .expect_poll_next()
            .times(1)
            .return_once(|_| Poll::Ready(Some(Err(crate::error::SubstreamError::ChannelClogged))));

        executor.read_message(
            peer,
            Some(QueryId(1335)),
            Substream::new_mock(peer, SubstreamId::from(0usize), Box::new(substream)),
        );

        match tokio::time::timeout(Duration::from_secs(20), executor.next()).await {
            Ok(Some(QueryContext {
                peer: queried_peer,
                query_id,
                result,
            })) => {
                assert_eq!(peer, queried_peer);
                assert_eq!(query_id, Some(QueryId(1335)));
                assert!(std::matches!(result, QueryResult::SubstreamClosed));
            }
            result => panic!("invalid result received: {result:?}"),
        }
    }
}
