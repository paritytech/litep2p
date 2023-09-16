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
    mock::substream::Substream as MockSubstream,
    transport::{quic, tcp, websocket},
    PeerId,
};

use bytes::{Buf, Bytes, BytesMut};
use futures::{Sink, SinkExt, Stream};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use unsigned_varint::{decode, encode};

use std::{
    collections::{hash_map::Entry, HashMap, VecDeque},
    fmt::Debug,
    hash::Hash,
    io::ErrorKind,
    pin::Pin,
    task::{Context, Poll},
};

/// Logging target for the file.
const LOG_TARGET: &str = "substream";

macro_rules! poll_flush {
    ($substream:expr, $cx:ident) => {{
        match $substream {
            SubstreamType::Tcp(substream) => Pin::new(substream).poll_flush($cx),
            SubstreamType::WebSocket(substream) => Pin::new(substream).poll_flush($cx),
            SubstreamType::Quic(substream) => Pin::new(substream).poll_flush($cx),
            #[cfg(test)]
            SubstreamType::Mock(_) => unreachable!(),
        }
    }};
}

macro_rules! poll_write {
    ($substream:expr, $cx:ident, $frame:expr) => {{
        match $substream {
            SubstreamType::Tcp(substream) => Pin::new(substream).poll_write($cx, $frame),
            SubstreamType::WebSocket(substream) => Pin::new(substream).poll_write($cx, $frame),
            SubstreamType::Quic(substream) => Pin::new(substream).poll_write($cx, $frame),
            #[cfg(test)]
            SubstreamType::Mock(_) => unreachable!(),
        }
    }};
}

macro_rules! poll_read {
    ($substream:expr, $cx:ident, $buffer:expr) => {{
        match $substream {
            SubstreamType::Tcp(substream) => Pin::new(substream).poll_read($cx, $buffer),
            SubstreamType::WebSocket(substream) => Pin::new(substream).poll_read($cx, $buffer),
            SubstreamType::Quic(substream) => Pin::new(substream).poll_read($cx, $buffer),
            #[cfg(test)]
            SubstreamType::Mock(_) => unreachable!(),
        }
    }};
}

macro_rules! poll_shutdown {
    ($substream:expr, $cx:ident) => {{
        match $substream {
            SubstreamType::Tcp(substream) => Pin::new(substream).poll_shutdown($cx),
            SubstreamType::WebSocket(substream) => Pin::new(substream).poll_shutdown($cx),
            SubstreamType::Quic(substream) => Pin::new(substream).poll_shutdown($cx),
            #[cfg(test)]
            SubstreamType::Mock(substream) => {
                let _ = Pin::new(substream).poll_close($cx);
                todo!();
            }
        }
    }};
}

macro_rules! delegate_poll_next {
    ($substream:expr, $cx:ident) => {{
        #[cfg(test)]
        if let SubstreamType::Mock(inner) = $substream {
            return Pin::new(inner).poll_next($cx);
        }
    }};
}

macro_rules! delegate_poll_ready {
    ($substream:expr, $cx:ident) => {{
        #[cfg(test)]
        if let SubstreamType::Mock(inner) = $substream {
            return Pin::new(inner).poll_ready($cx);
        }
    }};
}

macro_rules! delegate_start_send {
    ($substream:expr, $item:ident) => {{
        #[cfg(test)]
        if let SubstreamType::Mock(inner) = $substream {
            return Pin::new(inner).start_send($item);
        }
    }};
}

macro_rules! delegate_poll_flush {
    ($substream:expr, $cx:ident) => {{
        #[cfg(test)]
        if let SubstreamType::Mock(inner) = $substream {
            return Pin::new(inner).poll_flush($cx);
        }
    }};
}

/// Substream type.
#[derive(Debug)]
enum SubstreamType {
    Tcp(tcp::Substream),
    WebSocket(websocket::Substream),
    Quic(quic::Substream),
    #[cfg(test)]
    Mock(Box<dyn MockSubstream>),
}

/// Backpressure boundary for `Sink`.
const BACKPRESSURE_BOUNDARY: usize = 65536;

/// `Litep2p` substream type.
///
/// Implements `tokio::io::AsyncRead`/`tokio::io::AsyncWrite` traits which can be wrapped
/// in a `Framed` to implement a custom codec.
///
/// In case a codec for the protocol was specified, `Substream::send()`/Substream::next()`
/// are also provided which implement the necessary framing to read/write codec-encoded messages
/// from the underlying socket.
#[derive(Debug)]
pub struct Substream {
    /// Remote peer ID.
    peer: PeerId,

    // Inner substream.
    substream: SubstreamType,

    /// Protocol codec.
    codec: ProtocolCodec,

    pending_out_frames: VecDeque<Bytes>,
    pending_out_bytes: usize,
    pending_out_frame: Option<Bytes>,

    read_buffer: BytesMut,
    offset: usize,
    pending_frames: VecDeque<BytesMut>,
    current_frame_size: Option<usize>,
}

impl Substream {
    /// Create new [`Substream`] for TCP.
    pub(crate) fn new_tcp(peer: PeerId, substream: tcp::Substream, codec: ProtocolCodec) -> Self {
        tracing::trace!(target: LOG_TARGET, ?peer, ?codec, "create new substream for tcp");

        Self {
            peer,
            codec,
            substream: SubstreamType::Tcp(substream),
            read_buffer: BytesMut::zeroed(1024),
            offset: 0usize,
            pending_frames: VecDeque::new(),
            current_frame_size: None,
            pending_out_bytes: 0usize,
            pending_out_frames: VecDeque::new(),
            pending_out_frame: None,
        }
    }
    /// Create new [`Substream`] for WebSocket.
    pub(crate) fn new_websocket(
        peer: PeerId,
        substream: websocket::Substream,
        codec: ProtocolCodec,
    ) -> Self {
        tracing::trace!(target: LOG_TARGET, ?peer, ?codec, "create new substream for websocket");

        Self {
            peer,
            codec,
            substream: SubstreamType::WebSocket(substream),
            read_buffer: BytesMut::zeroed(1024),
            offset: 0usize,
            pending_frames: VecDeque::new(),
            current_frame_size: None,
            pending_out_bytes: 0usize,
            pending_out_frames: VecDeque::new(),
            pending_out_frame: None,
        }
    }
    /// Create new [`Substream`] for QUIC.
    pub(crate) fn new_quic(peer: PeerId, substream: quic::Substream, codec: ProtocolCodec) -> Self {
        tracing::trace!(target: LOG_TARGET, ?peer, ?codec, "create new substream for quic");

        Self {
            peer,
            codec,
            substream: SubstreamType::Quic(substream),
            read_buffer: BytesMut::zeroed(1024),
            offset: 0usize,
            pending_frames: VecDeque::new(),
            current_frame_size: None,
            pending_out_bytes: 0usize,
            pending_out_frames: VecDeque::new(),
            pending_out_frame: None,
        }
    }

    /// Create new [`Substream`] for mocking.
    #[cfg(test)]
    pub(crate) fn new_mock(peer: PeerId, substream: Box<dyn MockSubstream>) -> Self {
        tracing::trace!(target: LOG_TARGET, ?peer, "create new substream for mocking");

        Self {
            peer,
            codec: ProtocolCodec::Generic,
            substream: SubstreamType::Mock(substream),
            read_buffer: BytesMut::new(),
            offset: 0usize,
            pending_frames: VecDeque::new(),
            current_frame_size: None,
            pending_out_bytes: 0usize,
            pending_out_frames: VecDeque::new(),
            pending_out_frame: None,
        }
    }

    /// Close the substream.
    pub async fn close(self) {
        let _ = match self.substream {
            SubstreamType::Tcp(mut substream) => substream.shutdown().await,
            SubstreamType::WebSocket(mut substream) => substream.shutdown().await,
            SubstreamType::Quic(mut substream) => substream.shutdown().await,
            #[cfg(test)]
            SubstreamType::Mock(mut substream) => {
                let _ = substream.close().await;
                Ok(())
            }
        };
    }

    /// Send identity payload to remote peer.
    async fn send_identity_payload<T: AsyncWrite + Unpin>(
        io: &mut T,
        payload_size: usize,
        payload: Bytes,
    ) -> crate::Result<()> {
        if payload.len() != payload_size {
            tracing::warn!(target: LOG_TARGET, "{} vs {}", payload.len(), payload_size);
            return Err(Error::InvalidData);
        }

        io.write_all(&payload)
            .await
            .map_err(|_| Error::SubstreamError(SubstreamError::ConnectionClosed))
    }

    /// Send framed data to remote peer.
    ///
    /// This function may be faster than the provided [`futures::Sink`] implementation for
    /// [`Substream`] as it has direct access to the API of the underlying socket as opposed
    /// to going through [`tokio::io::AsyncWrite`].
    ///
    /// # Cancel safety
    ///
    /// This method is not cancellation safe. If that is required, use the provided
    /// [`futures::Sink`] implementation.
    ///
    /// # Panics
    ///
    /// Panics if no codec is provided.
    pub async fn send_framed(&mut self, bytes: Bytes) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.peer,
            codec = ?self.codec,
            frame_len = ?bytes.len(),
            "send framed"
        );

        match &mut self.substream {
            #[cfg(test)]
            SubstreamType::Mock(ref mut substream) => substream.send(bytes).await,
            SubstreamType::Tcp(ref mut substream) => match self.codec {
                ProtocolCodec::Generic => todo!(),
                ProtocolCodec::Identity(payload_size) =>
                    Self::send_identity_payload(substream, payload_size, bytes).await,
                ProtocolCodec::UnsignedVarint(max_size) => {
                    if let Some(max_size) = max_size {
                        if bytes.len() > max_size {
                            return Err(Error::IoError(ErrorKind::PermissionDenied));
                        }
                    }

                    let mut buffer = [0u8; 10];
                    let len = unsigned_varint::encode::usize(bytes.len(), &mut buffer);
                    let len = BytesMut::from(len);

                    let size = len.len() + bytes.len();
                    let mut len_bytes = len.chain(bytes).copy_to_bytes(size);

                    substream
                        .write_all_buf(&mut len_bytes)
                        .await
                        .map_err(|_| Error::SubstreamError(SubstreamError::ConnectionClosed))
                }
            },
            SubstreamType::WebSocket(ref mut substream) => match self.codec {
                ProtocolCodec::Generic => todo!(),
                ProtocolCodec::Identity(payload_size) =>
                    Self::send_identity_payload(substream, payload_size, bytes).await,
                ProtocolCodec::UnsignedVarint(max_size) => {
                    if let Some(max_size) = max_size {
                        if bytes.len() > max_size {
                            return Err(Error::IoError(ErrorKind::PermissionDenied));
                        }
                    }

                    let mut buffer = [0u8; 10];
                    let len = unsigned_varint::encode::usize(bytes.len(), &mut buffer);
                    let len = BytesMut::from(len);

                    let size = len.len() + bytes.len();
                    let mut len_bytes = len.chain(bytes).copy_to_bytes(size);

                    substream
                        .write_all_buf(&mut len_bytes)
                        .await
                        .map_err(|_| Error::SubstreamError(SubstreamError::ConnectionClosed))
                }
            },
            SubstreamType::Quic(ref mut substream) => match self.codec {
                ProtocolCodec::Generic => todo!(),
                ProtocolCodec::Identity(payload_size) =>
                    Self::send_identity_payload(substream, payload_size, bytes).await,
                ProtocolCodec::UnsignedVarint(max_size) => {
                    if let Some(max_size) = max_size {
                        if bytes.len() > max_size {
                            return Err(Error::IoError(ErrorKind::PermissionDenied));
                        }
                    }

                    let mut buffer = [0u8; 10];
                    let len = unsigned_varint::encode::usize(bytes.len(), &mut buffer);
                    let len = BytesMut::from(len);

                    substream.write_all_chunks(&mut [len.freeze(), bytes]).await
                }
            },
        }
    }
}

impl tokio::io::AsyncRead for Substream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        poll_read!(&mut self.substream, cx, buf)
    }
}

impl tokio::io::AsyncWrite for Substream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        poll_write!(&mut self.substream, cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        poll_flush!(&mut self.substream, cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        poll_shutdown!(&mut self.substream, cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        match &mut self.substream {
            #[cfg(test)]
            SubstreamType::Mock(_) => unreachable!(),
            SubstreamType::Tcp(substream) => Pin::new(substream).poll_write_vectored(cx, bufs),
            SubstreamType::WebSocket(substream) =>
                Pin::new(substream).poll_write_vectored(cx, bufs),
            SubstreamType::Quic(substream) => Pin::new(substream).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match &self.substream {
            #[cfg(test)]
            SubstreamType::Mock(_) => unreachable!(),
            SubstreamType::Tcp(substream) => substream.is_write_vectored(),
            SubstreamType::WebSocket(substream) => substream.is_write_vectored(),
            SubstreamType::Quic(substream) => substream.is_write_vectored(),
        }
    }
}

enum ReadError {
    Overflow,
    NotEnoughBytes,
    DecodeError,
}

// Return the payload size and the number of bytes it took to encode it
fn read_payload_size(buffer: &[u8]) -> Result<(usize, usize), ReadError> {
    let max_len = encode::usize_buffer().len();

    for i in 0..std::cmp::min(buffer.len(), max_len) {
        if decode::is_last(buffer[i]) {
            match decode::usize(&buffer[..=i]) {
                Err(_) => return Err(ReadError::DecodeError),
                Ok(size) => return Ok((size.0, i + 1)),
            }
        }
    }

    match buffer.len() < max_len {
        true => Err(ReadError::NotEnoughBytes),
        false => Err(ReadError::Overflow),
    }
}

impl Stream for Substream {
    // TODO: change to `type Item = BytesMut;`
    type Item = crate::Result<BytesMut>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);

        // `MockSubstream` implements `Stream` so calls to `poll_next()` must be delegated
        delegate_poll_next!(&mut this.substream, cx);

        loop {
            match this.codec {
                ProtocolCodec::Identity(payload_size) => {
                    let mut read_buf =
                        ReadBuf::new(&mut this.read_buffer[this.offset..payload_size]);

                    match futures::ready!(poll_read!(&mut this.substream, cx, &mut read_buf)) {
                        Ok(_) => {
                            let nread = read_buf.filled().len();
                            if nread == 0 {
                                tracing::trace!(
                                    target: LOG_TARGET,
                                    peer = ?this.peer,
                                    "read zero bytes, substream closed"
                                );
                                return Poll::Ready(None);
                            }

                            if nread == payload_size {
                                let mut payload = std::mem::replace(
                                    &mut this.read_buffer,
                                    BytesMut::zeroed(payload_size),
                                );
                                payload.truncate(payload_size);
                                this.offset = 0usize;

                                return Poll::Ready(Some(Ok(payload)));
                            } else {
                                this.offset += read_buf.filled().len();
                            }
                        }
                        Err(error) => return Poll::Ready(Some(Err(error.into()))),
                    }
                }
                ProtocolCodec::UnsignedVarint(max_size) => {
                    loop {
                        // return all pending frames first
                        if let Some(frame) = this.pending_frames.pop_front() {
                            return Poll::Ready(Some(Ok(frame)));
                        }

                        // if there are no pending frames, read more data from the underlying socket
                        let mut read_buf = ReadBuf::new(&mut this.read_buffer[this.offset..]);

                        let nread = match futures::ready!(poll_read!(
                            &mut this.substream,
                            cx,
                            &mut read_buf
                        )) {
                            Err(error) => return Poll::Ready(None),
                            Ok(_) => match read_buf.filled().len() {
                                0 => {
                                    tracing::trace!(
                                        target: LOG_TARGET,
                                        peer = ?this.peer,
                                        "read zero bytes, substream closed"
                                    );
                                    return Poll::Ready(None);
                                }
                                nread => nread,
                            },
                        };

                        this.offset += nread;
                        this.read_buffer.truncate(this.offset);

                        let size_hint = loop {
                            // get frame size, either from current or previous iteration
                            let frame_size = match this.current_frame_size.take() {
                                Some(frame_size) => frame_size,
                                None => match read_payload_size(&this.read_buffer) {
                                    Ok((size, num_bytes)) => {
                                        // TODO: verify `size`
                                        this.offset -= num_bytes;
                                        this.read_buffer.advance(num_bytes);
                                        size
                                    }
                                    Err(ReadError::Overflow | ReadError::DecodeError) =>
                                        return Poll::Ready(None),
                                    Err(ReadError::NotEnoughBytes) => break None,
                                },
                            };

                            if this.read_buffer.len() < frame_size {
                                this.current_frame_size = Some(frame_size);
                                break Some(frame_size);
                            }

                            let frame = this.read_buffer.split_to(frame_size);
                            this.offset -= frame.len();
                            this.pending_frames.push_back(frame);
                        };

                        // TODO: can this be optimized?
                        this.read_buffer
                            .resize(this.read_buffer.len() + size_hint.unwrap_or(1024), 0u8);
                    }
                }
                _ => todo!(),
            }
        }
    }
}

// TODO: this code can definitely be optimized
impl Sink<Bytes> for Substream {
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // `MockSubstream` implements `Sink` so calls to `poll_ready()` must be delegated
        delegate_poll_ready!(&mut self.substream, cx);

        if self.pending_out_bytes >= BACKPRESSURE_BOUNDARY {
            return poll_flush!(&mut self.substream, cx).map_err(From::from);
        }

        Poll::Ready(Ok(()))
    }

    fn start_send(mut self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        // `MockSubstream` implements `Sink` so calls to `start_send()` must be delegated
        delegate_start_send!(&mut self.substream, item);

        match self.codec {
            ProtocolCodec::Identity(payload_size) => {
                if item.len() != payload_size {
                    return Err(Error::InvalidData);
                }

                self.pending_out_bytes += item.len();
                self.pending_out_frames.push_back(item);
            }
            ProtocolCodec::UnsignedVarint(max_size) => {
                let len = {
                    let mut buffer = [0u8; 10];
                    let len = unsigned_varint::encode::usize(item.len(), &mut buffer);
                    BytesMut::from(len)
                };

                self.pending_out_bytes += len.len() + item.len();
                self.pending_out_frames.push_back(len.freeze());
                self.pending_out_frames.push_back(item);
            }
            _ => todo!(),
        }

        return Ok(());
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // `MockSubstream` implements `Sink` so calls to `poll_flush()` must be delegated
        delegate_poll_flush!(&mut self.substream, cx);

        loop {
            let mut pending_frame = match self.pending_out_frame.take() {
                Some(frame) => frame,
                None => match self.pending_out_frames.pop_front() {
                    Some(frame) => frame,
                    None => break,
                },
            };

            match poll_write!(&mut self.substream, cx, &pending_frame) {
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error.into())),
                Poll::Pending => {
                    self.pending_out_frame = Some(pending_frame);
                }
                Poll::Ready(Ok(nwritten)) => {
                    pending_frame.advance(nwritten);

                    if !pending_frame.is_empty() {
                        self.pending_out_frame = Some(pending_frame);
                    }
                }
            }
        }

        poll_flush!(&mut self.substream, cx).map_err(From::from)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        poll_shutdown!(&mut self.substream, cx).map_err(From::from)
    }
}

/// Substream set key.
pub trait SubstreamSetKey: Hash + Unpin + Debug + PartialEq + Eq + Copy {}

impl<K: Hash + Unpin + Debug + PartialEq + Eq + Copy> SubstreamSetKey for K {}

/// Substream set.
#[derive(Debug, Default)]
pub struct SubstreamSet<K, S>
where
    K: SubstreamSetKey,
    S: Stream<Item = crate::Result<BytesMut>> + Unpin,
{
    substreams: HashMap<K, S>,
}

impl<K, S> SubstreamSet<K, S>
where
    K: SubstreamSetKey,
    S: Stream<Item = crate::Result<BytesMut>> + Unpin,
{
    /// Create new [`SubstreamSet`].
    pub fn new() -> Self {
        Self {
            substreams: HashMap::new(),
        }
    }

    /// Add new substream to the set.
    pub fn insert(&mut self, key: K, substream: S) {
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
    pub fn remove(&mut self, key: &K) -> Option<S> {
        self.substreams.remove(key)
    }

    /// Get mutable reference to stored substream.
    pub fn get_mut(&mut self, key: &K) -> Option<&mut S> {
        self.substreams.get_mut(key)
    }
}

impl<K, S> Stream for SubstreamSet<K, S>
where
    K: SubstreamSetKey,
    S: Stream<Item = crate::Result<BytesMut>> + Unpin,
{
    type Item = (K, <S as Stream>::Item);

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
        let mut set = SubstreamSet::<PeerId, MockSubstream>::new();

        let peer = PeerId::random();
        let substream = MockSubstream::new();
        set.insert(peer, substream);

        let peer = PeerId::random();
        let substream = MockSubstream::new();
        set.insert(peer, substream);
    }

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn add_same_peer_twice() {
        let mut set = SubstreamSet::<PeerId, MockSubstream>::new();

        let peer = PeerId::random();
        let substream1 = MockSubstream::new();
        let substream2 = MockSubstream::new();

        set.insert(peer, substream1);
        set.insert(peer, substream2);
    }

    #[test]
    fn remove_substream() {
        let mut set = SubstreamSet::<PeerId, MockSubstream>::new();

        let peer1 = PeerId::random();
        let substream1 = MockSubstream::new();
        set.insert(peer1, substream1);

        let peer2 = PeerId::random();
        let substream2 = MockSubstream::new();
        set.insert(peer2, substream2);

        assert!(set.remove(&peer1).is_some());
        assert!(set.remove(&peer2).is_some());
        assert!(set.remove(&PeerId::random()).is_none());
    }

    #[tokio::test]
    async fn poll_data_from_substream() {
        let mut set = SubstreamSet::<PeerId, MockSubstream>::new();

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
        let mut set = SubstreamSet::<PeerId, MockSubstream>::new();

        let peer = PeerId::random();
        let mut substream = MockSubstream::new();
        substream
            .expect_poll_next()
            .times(1)
            .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"hello"[..])))));
        substream.expect_poll_next().times(1).return_once(|_| Poll::Ready(None));
        substream.expect_poll_next().returning(|_| Poll::Pending);
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

        let mut set = SubstreamSet::<PeerId, MockSubstream>::new();

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
        let mut set = SubstreamSet::<PeerId, MockSubstream>::new();

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
