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

use crate::error::{Error, SubstreamError};

use bytes::{Bytes, BytesMut};
use futures::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite};

use std::{
    collections::{hash_map::Entry, HashMap},
    fmt::Debug,
    hash::Hash,
    pin::Pin,
    task::{Context, Poll},
};

pub mod channel;

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

/// Substream set key.
pub trait SubstreamSetKey: Hash + Unpin + Debug + PartialEq + Eq + Copy {}

impl<K: Hash + Unpin + Debug + PartialEq + Eq + Copy> SubstreamSetKey for K {}

/// Substream set.
#[derive(Debug, Default)]
pub struct SubstreamSet<K: SubstreamSetKey> {
    substreams: HashMap<K, Box<dyn Substream>>,
}

impl<K: SubstreamSetKey> SubstreamSet<K> {
    /// Create new [`SubstreamSet`].
    pub fn new() -> Self {
        Self {
            substreams: HashMap::new(),
        }
    }

    /// Add new substream to the set.
    pub fn insert(&mut self, key: K, substream: Box<dyn Substream>) {
        match self.substreams.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(substream);
            }
            Entry::Occupied(_) => {
                tracing::error!(?key, "substream alraedy exists");
                debug_assert!(false);
            }
        }
    }

    /// Remove substream from the set.
    pub fn remove(&mut self, key: &K) -> Option<Box<dyn Substream>> {
        self.substreams.remove(key)
    }

    /// Get length of the [`SubstreamSet`].
    pub fn len(&self) -> usize {
        self.substreams.len()
    }

    /// Return true if the [`SubstreamSet`] is empty.
    pub fn is_empty(&mut self) -> bool {
        self.substreams.len() == 0usize
    }

    /// Get mutable reference to stored substream.
    pub fn get_mut(&mut self, key: &K) -> Option<&mut Box<dyn Substream>> {
        self.substreams.get_mut(key)
    }
}

impl<K: SubstreamSetKey> Stream for SubstreamSet<K> {
    type Item = (K, <Box<dyn Substream> as Stream>::Item);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let inner = Pin::into_inner(self);

        // TODO: poll the streams more randomly
        for (key, mut substream) in inner.substreams.iter_mut() {
            match Pin::new(&mut substream).poll_next(cx) {
                Poll::Pending => continue,
                Poll::Ready(Some(data)) => return Poll::Ready(Some((*key, data))),
                Poll::Ready(None) => {
                    return Poll::Ready(Some((
                        *key,
                        Err(Error::SubstreamError(SubstreamError::ConnectionClosed)),
                    )))
                }
            }
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{mock::substream::MockSubstream, peer_id::PeerId};
    use futures::{SinkExt, StreamExt};

    #[test]
    fn add_substream() {
        let mut set = SubstreamSet::<PeerId>::new();

        let peer = PeerId::random();
        let substream = Box::new(MockSubstream::new());
        set.insert(peer, substream);

        let peer = PeerId::random();
        let substream = Box::new(MockSubstream::new());
        set.insert(peer, substream);
    }

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn add_same_peer_twice() {
        let mut set = SubstreamSet::<PeerId>::new();

        let peer = PeerId::random();
        let substream1 = Box::new(MockSubstream::new());
        let substream2 = Box::new(MockSubstream::new());

        set.insert(peer, substream1);
        set.insert(peer, substream2);
    }

    #[test]
    fn remove_substream() {
        let mut set = SubstreamSet::<PeerId>::new();

        let peer1 = PeerId::random();
        let substream1 = Box::new(MockSubstream::new());
        set.insert(peer1, substream1);

        let peer2 = PeerId::random();
        let substream2 = Box::new(MockSubstream::new());
        set.insert(peer2, substream2);

        assert!(set.remove(&peer1).is_some());
        assert!(set.remove(&peer2).is_some());
        assert!(set.remove(&PeerId::random()).is_none());
    }

    #[tokio::test]
    async fn poll_data_from_substream() {
        let mut set = SubstreamSet::<PeerId>::new();

        let peer = PeerId::random();
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
        let substream = Box::new(substream);
        set.insert(peer, substream);

        let value = set.next().await.unwrap();
        assert_eq!(value.0, peer);
        assert_eq!(value.1.unwrap(), BytesMut::from(&b"hello"[..]));

        let value = set.next().await.unwrap();
        assert_eq!(value.0, peer);
        assert_eq!(value.1.unwrap(), BytesMut::from(&b"world"[..]));

        assert!(futures::poll!(set.next()).is_pending());
    }

    #[tokio::test]
    async fn substream_closed() {
        let mut set = SubstreamSet::<PeerId>::new();

        let peer = PeerId::random();
        let mut substream = MockSubstream::new();
        substream
            .expect_poll_next()
            .times(1)
            .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"hello"[..])))));
        substream
            .expect_poll_next()
            .times(1)
            .return_once(|_| Poll::Ready(None));
        substream.expect_poll_next().returning(|_| Poll::Pending);
        let substream = Box::new(substream);
        set.insert(peer, substream);

        let value = set.next().await.unwrap();
        assert_eq!(value.0, peer);
        assert_eq!(value.1.unwrap(), BytesMut::from(&b"hello"[..]));

        match set.next().await {
            Some((exited_peer, Err(Error::SubstreamError(SubstreamError::ConnectionClosed)))) => {
                assert_eq!(peer, exited_peer);
            }
            _ => panic!("inavlid event received"),
        }
    }

    #[tokio::test]
    async fn get_mut_substream() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let mut set = SubstreamSet::<PeerId>::new();

        let peer = PeerId::random();
        let mut substream = MockSubstream::new();
        substream
            .expect_poll_next()
            .times(1)
            .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"hello"[..])))));
        substream
            .expect_poll_ready()
            .times(1)
            .return_once(|_| Poll::Ready(Ok(())));
        substream
            .expect_start_send()
            .times(1)
            .return_once(|_| Ok(()));
        substream
            .expect_poll_flush()
            .times(1)
            .return_once(|_| Poll::Ready(Ok(())));
        substream
            .expect_poll_next()
            .times(1)
            .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"world"[..])))));
        substream.expect_poll_next().returning(|_| Poll::Pending);
        let substream = Box::new(substream);
        set.insert(peer, substream);

        let value = set.next().await.unwrap();
        assert_eq!(value.0, peer);
        assert_eq!(value.1.unwrap(), BytesMut::from(&b"hello"[..]));

        let substream = set.get_mut(&peer).unwrap();
        substream.send(vec![1, 2, 3, 4].into()).await.unwrap();

        let value = set.next().await.unwrap();
        assert_eq!(value.0, peer);
        assert_eq!(value.1.unwrap(), BytesMut::from(&b"world"[..]));

        // try to get non-existent substream
        assert!(set.get_mut(&PeerId::random()).is_none());
    }

    #[tokio::test]
    async fn poll_data_from_two_substreams() {
        let mut set = SubstreamSet::<PeerId>::new();

        // prepare first substream
        let peer1 = PeerId::random();
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
        let substream1 = Box::new(substream1);
        set.insert(peer1, substream1);

        // prepare second substream
        let peer2 = PeerId::random();
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
        let substream2 = Box::new(substream2);
        set.insert(peer2, substream2);

        let expected: Vec<Vec<(PeerId, BytesMut)>> = vec![
            vec![
                (peer1, BytesMut::from(&b"hello"[..])),
                (peer1, BytesMut::from(&b"world"[..])),
                (peer2, BytesMut::from(&b"siip"[..])),
                (peer2, BytesMut::from(&b"huup"[..])),
            ],
            vec![
                (peer1, BytesMut::from(&b"hello"[..])),
                (peer2, BytesMut::from(&b"siip"[..])),
                (peer1, BytesMut::from(&b"world"[..])),
                (peer2, BytesMut::from(&b"huup"[..])),
            ],
            vec![
                (peer2, BytesMut::from(&b"siip"[..])),
                (peer2, BytesMut::from(&b"huup"[..])),
                (peer1, BytesMut::from(&b"hello"[..])),
                (peer1, BytesMut::from(&b"world"[..])),
            ],
            vec![
                (peer1, BytesMut::from(&b"hello"[..])),
                (peer2, BytesMut::from(&b"siip"[..])),
                (peer2, BytesMut::from(&b"huup"[..])),
                (peer1, BytesMut::from(&b"world"[..])),
            ],
        ];

        // poll values
        let mut values = Vec::new();

        for _ in 0..4 {
            let value = set.next().await.unwrap();
            values.push((value.0, value.1.unwrap()));
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
