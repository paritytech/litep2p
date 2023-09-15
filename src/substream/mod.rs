// Copyright 2020 Parity Technologies (UK) Ltd.
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

//! Substream-related helper code.

use crate::{
    codec::ProtocolCodec,
    error::{Error, SubstreamError},
    transport::{quic, tcp, websocket},
};

use bytes::{Buf, Bytes, BytesMut};
use futures::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use unsigned_varint::{decode, encode};

use std::{
    collections::{hash_map::Entry, HashMap},
    fmt::Debug,
    hash::Hash,
    pin::Pin,
    task::{Context, Poll},
};

/// Logging target for the file.
const LOG_TARGET: &str = "substream";

/// Raw substream received from one of the enabled transports.
// TODO: remove
pub trait RawSubstream: AsyncRead + AsyncWrite + Unpin + Send + Debug + 'static {}

/// Blanket implementation for [`RawSubstream`].
// TODO: remove
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Debug + 'static> RawSubstream for T {}

/// Trait which describes the behavior of a substream.
// TODO: remove
pub trait Substream:
    Debug + Stream<Item = crate::Result<BytesMut>> + Sink<Bytes, Error = Error> + Send + Unpin + 'static
{
}

/// Blanket implementation for [`Substream`].
// TODO: remove
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

/// Substream type.
#[derive(Debug)]
enum SubstreamType {
    Tcp(tcp::Substream),
    WebSocket(websocket::Substream),
    Quic(quic::Substream),
}

/// `Litep2p` substream type.
///
/// Implements `tokio::io::AsyncRead`/`tokio::io::AsyncWrite` traits which can be wrapped
/// in a `Framed` to implement a custom codec.
///
/// In case a codec for the protocol was specified, `Substream::send()`/Substream::next()`
/// are also provided which implement the necessary framing to read/write codec-encoded messages
/// from the underlying socket.
#[derive(Debug)]
pub struct NewSubstream {
    // Inner substream.
    substream: SubstreamType,

    /// Protocol codec.
    codec: ProtocolCodec,
}

impl NewSubstream {
    /// Create new [`Substream`] for TCP.
    pub(crate) fn new_tcp(substream: tcp::Substream, codec: ProtocolCodec) -> Self {
        tracing::trace!(target: LOG_TARGET, ?codec, "create new substream for tcp");

        Self {
            codec,
            substream: SubstreamType::Tcp(substream),
        }
    }
    /// Create new [`Substream`] for WebSocket.
    pub(crate) fn new_websocket(substream: websocket::Substream, codec: ProtocolCodec) -> Self {
        tracing::trace!(target: LOG_TARGET, ?codec, "create new substream for websocket");

        Self {
            codec,
            substream: SubstreamType::WebSocket(substream),
        }
    }
    /// Create new [`Substream`] for QUIC.
    pub(crate) fn new_quic(substream: quic::Substream, codec: ProtocolCodec) -> Self {
        tracing::trace!(target: LOG_TARGET, ?codec, "create new substream for quic");

        Self {
            codec,
            substream: SubstreamType::Quic(substream),
        }
    }

    /// Send framed data to remote peer.
    ///
    /// Panics if no codec is provided.
    pub async fn send(&mut self, bytes: Bytes) -> crate::Result<()> {
        match &mut self.substream {
            SubstreamType::Tcp(ref mut substream) => match self.codec {
                ProtocolCodec::Generic => todo!(),
                ProtocolCodec::Identity(payload_size) => {
                    if bytes.len() != payload_size {
                        return Err(Error::InvalidData);
                    }

                    substream
                        .write_all(&bytes)
                        .await
                        .map_err(|_| Error::SubstreamError(SubstreamError::ConnectionClosed))
                }
                ProtocolCodec::UnsignedVarint(max_size) => {
                    if let Some(max_size) = max_size {
                        if bytes.len() > max_size {
                            return Err(Error::InvalidData);
                        }
                    }

                    let len_bytes = BytesMut::zeroed(10);
                    let mut buffer: [u8; 10] = len_bytes.as_ref().try_into().expect("to succeed");
                    unsigned_varint::encode::usize(bytes.len(), &mut buffer);

                    let size = len_bytes.len() + bytes.len();
                    let mut len_bytes = len_bytes.chain(bytes).copy_to_bytes(size);

                    substream
                        .write_all_buf(&mut len_bytes)
                        .await
                        .map_err(|_| Error::SubstreamError(SubstreamError::ConnectionClosed))
                }
            },
            SubstreamType::WebSocket(ref mut substream) => match self.codec {
                ProtocolCodec::Generic => todo!(),
                ProtocolCodec::Identity(payload_size) => {
                    if bytes.len() != payload_size {
                        return Err(Error::InvalidData);
                    }

                    substream
                        .write_all(&bytes)
                        .await
                        .map_err(|_| Error::SubstreamError(SubstreamError::ConnectionClosed))
                }
                ProtocolCodec::UnsignedVarint(max_size) => {
                    if let Some(max_size) = max_size {
                        if bytes.len() > max_size {
                            return Err(Error::InvalidData);
                        }
                    }

                    let len_bytes = BytesMut::zeroed(10);
                    let mut buffer: [u8; 10] = len_bytes.as_ref().try_into().expect("to succeed");
                    unsigned_varint::encode::usize(bytes.len(), &mut buffer);

                    let size = len_bytes.len() + bytes.len();
                    let mut len_bytes = len_bytes.chain(bytes).copy_to_bytes(size);

                    substream
                        .write_all_buf(&mut len_bytes)
                        .await
                        .map_err(|_| Error::SubstreamError(SubstreamError::ConnectionClosed))
                }
            },
            SubstreamType::Quic(ref mut substream) => match self.codec {
                ProtocolCodec::Generic => todo!(),
                ProtocolCodec::Identity(payload_size) => {
                    if bytes.len() != payload_size {
                        return Err(Error::InvalidData);
                    }

                    substream.write_all(bytes).await
                }
                ProtocolCodec::UnsignedVarint(max_size) => {
                    if let Some(max_size) = max_size {
                        if bytes.len() > max_size {
                            return Err(Error::InvalidData);
                        }
                    }

                    let len_bytes = BytesMut::zeroed(10);
                    let mut buffer: [u8; 10] = len_bytes.as_ref().try_into().expect("to succeed");
                    unsigned_varint::encode::usize(bytes.len(), &mut buffer);

                    substream.write_all_chunks(&mut [len_bytes.freeze(), bytes]).await
                }
            },
        }
    }

    /// Read `unsigned-varint` payload size from `io`.
    async fn read_unsigned_varint_payload_size<T: AsyncRead + Unpin>(io: &mut T) -> Option<usize> {
        let mut b = encode::usize_buffer();
        for i in 0..b.len() {
            let n = io.read(&mut b[i..i + 1]).await.ok()?;
            if n == 0 {
                tracing::trace!(target: LOG_TARGET, "substream closed while reading `unsigned-varint` payload size");
                return None;
            }

            if decode::is_last(b[i]) {
                return Some(decode::usize(&b[..=i]).ok()?.0);
            }
        }

        tracing::debug!(target: LOG_TARGET, "overflow in `unsigned-varint` payload size");
        None
    }

    /// Read `unsigned-varint` payload from `io`.
    async fn read_unsigned_varint_payload<T: AsyncRead + Unpin>(
        io: &mut T,
        max_size: Option<usize>,
    ) -> Option<Bytes> {
        let payload_size = Self::read_unsigned_varint_payload_size(io).await?;

        if let Some(max_size) = max_size {
            if payload_size > max_size {
                tracing::debug!(target: LOG_TARGET, ?payload_size, ?max_size, "too big payload");
                return None;
            }
        }

        let mut payload = BytesMut::zeroed(payload_size);
        io.read_exact(&mut payload).await.ok()?;

        Some(payload.freeze())
    }

    /// Read `Identity` payload from `io`.
    async fn read_identity_payload<T: AsyncRead + Unpin>(
        io: &mut T,
        payload_size: usize,
    ) -> Option<Bytes> {
        let mut bytes = BytesMut::zeroed(payload_size);
        io.read_exact(&mut bytes).await.ok()?;
        Some(bytes.freeze())
    }

    /// Read next frame from the underlying socket.
    ///
    /// Panics if no codec is provided.
    pub async fn next(&mut self) -> Option<Bytes> {
        match &mut self.substream {
            SubstreamType::Tcp(ref mut substream) => match self.codec {
                ProtocolCodec::Generic => todo!(),
                ProtocolCodec::Identity(payload_size) =>
                    Self::read_identity_payload(substream, payload_size).await,
                ProtocolCodec::UnsignedVarint(max_size) =>
                    Self::read_unsigned_varint_payload(substream, max_size).await,
            },
            SubstreamType::WebSocket(ref mut substream) => match self.codec {
                ProtocolCodec::Generic => todo!(),
                ProtocolCodec::Identity(payload_size) =>
                    Self::read_identity_payload(substream, payload_size).await,
                ProtocolCodec::UnsignedVarint(max_size) =>
                    Self::read_unsigned_varint_payload(substream, max_size).await,
            },
            SubstreamType::Quic(ref mut substream) => match self.codec {
                ProtocolCodec::Generic => todo!(),
                ProtocolCodec::Identity(payload_size) =>
                    Self::read_identity_payload(substream, payload_size).await,
                ProtocolCodec::UnsignedVarint(max_size) =>
                    Self::read_unsigned_varint_payload(substream, max_size).await,
            },
        }
    }
}

impl tokio::io::AsyncRead for NewSubstream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut self.substream {
            SubstreamType::Tcp(ref mut substream) => Pin::new(substream).poll_read(cx, buf),
            SubstreamType::WebSocket(ref mut substream) => Pin::new(substream).poll_read(cx, buf),
            SubstreamType::Quic(ref mut substream) => Pin::new(substream).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for NewSubstream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        match &mut self.substream {
            SubstreamType::Tcp(ref mut substream) => Pin::new(substream).poll_write(cx, buf),
            SubstreamType::WebSocket(ref mut substream) => Pin::new(substream).poll_write(cx, buf),
            SubstreamType::Quic(ref mut substream) => Pin::new(substream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match &mut self.substream {
            SubstreamType::Tcp(ref mut substream) => Pin::new(substream).poll_flush(cx),
            SubstreamType::WebSocket(ref mut substream) => Pin::new(substream).poll_flush(cx),
            SubstreamType::Quic(ref mut substream) => Pin::new(substream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match &mut self.substream {
            SubstreamType::Tcp(ref mut substream) => Pin::new(substream).poll_shutdown(cx),
            SubstreamType::WebSocket(ref mut substream) => Pin::new(substream).poll_shutdown(cx),
            SubstreamType::Quic(ref mut substream) => Pin::new(substream).poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        match &mut self.substream {
            SubstreamType::Tcp(ref mut substream) =>
                Pin::new(substream).poll_write_vectored(cx, bufs),
            SubstreamType::WebSocket(ref mut substream) =>
                Pin::new(substream).poll_write_vectored(cx, bufs),
            SubstreamType::Quic(ref mut substream) =>
                Pin::new(substream).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match &self.substream {
            SubstreamType::Tcp(substream) => substream.is_write_vectored(),
            SubstreamType::WebSocket(substream) => substream.is_write_vectored(),
            SubstreamType::Quic(substream) => substream.is_write_vectored(),
        }
    }
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
                Poll::Ready(None) =>
                    return Poll::Ready(Some((
                        *key,
                        Err(Error::SubstreamError(SubstreamError::ConnectionClosed)),
                    ))),
            }
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{mock::substream::MockSubstream, PeerId};
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
        substream.expect_poll_next().times(1).return_once(|_| Poll::Ready(None));
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
        substream.expect_poll_ready().times(1).return_once(|_| Poll::Ready(Ok(())));
        substream.expect_start_send().times(1).return_once(|_| Ok(()));
        substream.expect_poll_flush().times(1).return_once(|_| Poll::Ready(Ok(())));
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
