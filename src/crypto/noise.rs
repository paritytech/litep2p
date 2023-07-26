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

//! Noise handshake and transport implementations.

// TODO: benchmark reads and writes
// TODO: optimize read and writes
// TODO: this is really ugly code

use crate::{
    config::Role,
    crypto::{ed25519::Keypair, PublicKey},
    error,
    peer_id::PeerId,
};

use futures::{
    io::{AsyncRead, AsyncWrite},
    ready, AsyncReadExt, AsyncWriteExt, FutureExt,
};
use prost::Message;
use snow::{Builder, Error, HandshakeState, TransportState};

use std::{io, pin::Pin, task::Poll};

mod handshake_schema {
    include!(concat!(env!("OUT_DIR"), "/noise.rs"));
}

/// Noise parameters.
const NOISE_PARAMETERS: &str = "Noise_XX_25519_ChaChaPoly_SHA256";

/// Prefix of static key signatures for domain separation.
pub(crate) const STATIC_KEY_DOMAIN: &str = "noise-libp2p-static-key:";

/// Noise decrypt buffer size.
const NOISE_DECRYPT_BUFFER_SIZE: usize = 65536;

/// Noise decrypt buffer extra allocation size.
const NOISE_DECRYPT_EXTRA_ALLOC: usize = 1024;

/// Logging target for the file.
const LOG_TARGET: &str = "crypto::noise";

/// Logging target for the messages.
const LOG_TARGET_MSG: &str = "crypto::noise::message";

#[derive(Debug)]
pub struct NoiseConfiguration {
    /// Noise handshake state.
    pub noise: HandshakeState,

    /// Role of the node.
    pub role: Role,

    /// Payload that's sent as part of the libp2p Noise handshake.
    pub payload: Vec<u8>,
}

impl NoiseConfiguration {
    /// Create new Noise configuration.
    pub fn new(keypair: &Keypair, role: Role) -> Self {
        tracing::trace!(target: LOG_TARGET, ?role, "create new noise configuration");

        let builder: Builder<'_> =
            Builder::new(NOISE_PARAMETERS.parse().expect("valid Noise pattern"));
        let dh_keypair = builder
            .generate_keypair()
            .expect("keypair generation to succeed");
        let static_key = dh_keypair.private;

        let noise = match role {
            Role::Dialer => builder
                .local_private_key(&static_key)
                .build_initiator()
                .expect("initialization to succeed"),
            Role::Listener => builder
                .local_private_key(&static_key)
                .build_responder()
                .expect("initialization to succeed"),
        };

        let noise_payload = handshake_schema::NoiseHandshakePayload {
            identity_key: Some(PublicKey::Ed25519(keypair.public()).to_protobuf_encoding()),
            identity_sig: Some(
                keypair.sign(&[STATIC_KEY_DOMAIN.as_bytes(), dh_keypair.public.as_ref()].concat()),
            ),
            ..Default::default()
        };
        let mut payload = Vec::with_capacity(noise_payload.encoded_len());
        noise_payload
            .encode(&mut payload)
            .expect("Vec<u8> provides capacity as needed");

        Self {
            payload,
            noise,
            role,
        }
    }
}

trait Noise {
    fn write_message(&mut self, payload: &[u8], message: &mut [u8]) -> Result<usize, Error>;
    fn read_message(&mut self, message: &[u8], payload: &mut [u8]) -> Result<usize, Error>;
    fn into_transport_mode(self) -> Result<TransportState, Error>;
}

#[derive(Debug)]
struct NoiseHandshakeState(HandshakeState);

#[derive(Debug)]
struct NoiseTransportState(TransportState);

impl Noise for NoiseHandshakeState {
    fn write_message(&mut self, payload: &[u8], message: &mut [u8]) -> Result<usize, Error> {
        tracing::trace!(
            target: LOG_TARGET_MSG,
            payload_length = payload.len(),
            "handshake: write noise message",
        );

        self.0.write_message(payload, message)
    }

    fn read_message(&mut self, message: &[u8], payload: &mut [u8]) -> Result<usize, Error> {
        tracing::trace!(
            target: LOG_TARGET_MSG,
            message_length = message.len(),
            "handshake: read noise message",
        );

        self.0.read_message(message, payload)
    }

    fn into_transport_mode(self) -> Result<TransportState, Error> {
        self.0.into_transport_mode()
    }
}

impl Noise for NoiseTransportState {
    fn write_message(&mut self, payload: &[u8], message: &mut [u8]) -> Result<usize, Error> {
        tracing::trace!(
            target: LOG_TARGET_MSG,
            payload_length = payload.len(),
            "transport: write noise message",
        );

        self.0.write_message(payload, message)
    }

    fn read_message(&mut self, message: &[u8], payload: &mut [u8]) -> Result<usize, Error> {
        tracing::trace!(
            target: LOG_TARGET_MSG,
            message_length = message.len(),
            "transport: read noise message",
        );

        self.0.read_message(message, payload)
    }

    fn into_transport_mode(self) -> Result<TransportState, Error> {
        unimplemented!("`TransportState` does not implement `into_transport_mode()`");
    }
}

#[derive(Debug)]
enum ReadState {
    Ready,
    ReadLen {
        buf: [u8; 2],
        off: usize,
    },
    ReadData {
        len: usize,
        off: usize,
    },
    ReadBuffered {
        unread: Vec<u8>,
    },
    /// EOF has been reached (terminal state).
    ///
    /// The associated result signals if the EOF was unexpected or not.
    Eof(Result<(), ()>),
    /// A decryption error occurred (terminal state).
    DecErr,
}

#[derive(Debug)]
enum WriteState {
    Ready,
    _WriteLen { buf: [u8; 2], off: usize },
    _WriteData { len: usize, off: usize },
}

// TODO: documentation
#[derive(Debug)]
struct NoiseSocket<S: AsyncRead + AsyncWrite + Unpin, T: Unpin> {
    io: S,

    /// Noise state.
    noise: T,

    /// Buffer used by `snow` as destination for encrypted data.
    write_buffer: Vec<u8>,

    /// Buffer used by `snow` as destination for decrypted data.
    read_buffer: Vec<u8>,

    /// Buffer used by `snow` as destination for decrypted data.
    decrypt_buffer: Vec<u8>,

    /// Read state of the stream.
    read_state: ReadState,

    /// Write state of the stream.
    _write_state: WriteState,
}

impl<S: AsyncRead + AsyncWrite + Unpin, T: Unpin> NoiseSocket<S, T> {
    fn new(io: S, noise: T) -> Self {
        Self {
            io,
            noise,
            write_buffer: Vec::new(),
            read_buffer: Vec::with_capacity(NOISE_DECRYPT_BUFFER_SIZE),
            decrypt_buffer: Vec::with_capacity(NOISE_DECRYPT_BUFFER_SIZE),
            read_state: ReadState::Ready,
            _write_state: WriteState::Ready,
        }
    }
}

/// Read 2 bytes as frame length from the given source into the given buffer.
///
/// Panics if `off >= 2`.
///
/// When [`Poll::Pending`] is returned, the given buffer and offset
/// may have been updated (i.e. a byte may have been read) and must be preserved
/// for the next invocation.
///
/// Returns `None` if EOF has been encountered.
fn read_frame_len<R: AsyncRead + Unpin>(
    mut io: &mut R,
    cx: &mut std::task::Context<'_>,
    buf: &mut [u8; 2],
    off: &mut usize,
) -> Poll<io::Result<Option<u16>>> {
    loop {
        match ready!(Pin::new(&mut io).poll_read(cx, &mut buf[*off..])) {
            Ok(n) => {
                if n == 0 {
                    return Poll::Ready(Ok(None));
                }
                *off += n;
                if *off == 2 {
                    return Poll::Ready(Ok(Some(u16::from_be_bytes(*buf))));
                }
            }
            Err(e) => {
                return Poll::Ready(Err(e));
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin, T: Noise + Unpin> AsyncRead for NoiseSocket<S, T> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let mut this = Pin::into_inner(self);

        loop {
            match this.read_state {
                ReadState::Ready => {
                    this.read_state = ReadState::ReadLen {
                        buf: [0, 0],
                        off: 0,
                    };
                }
                ReadState::ReadLen { mut buf, mut off } => {
                    let n = match read_frame_len(&mut this.io, cx, &mut buf, &mut off) {
                        Poll::Ready(Ok(Some(n))) => n,
                        Poll::Ready(Ok(None)) => {
                            this.read_state = ReadState::Eof(Ok(()));
                            todo!();
                            // return Poll::Ready(None);
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Pending => {
                            this.read_state = ReadState::ReadLen { buf, off };
                            return Poll::Pending;
                        }
                    };

                    if n == 0 {
                        this.read_state = ReadState::Ready;
                        continue;
                    }

                    this.read_buffer.resize(usize::from(n), 0u8);
                    this.read_state = ReadState::ReadData {
                        len: usize::from(n),
                        off: 0,
                    }
                }
                ReadState::ReadData { len, ref mut off } => {
                    let n = {
                        let f =
                            Pin::new(&mut this.io).poll_read(cx, &mut this.read_buffer[*off..len]);
                        match ready!(f) {
                            Ok(n) => n,
                            Err(e) => return Poll::Ready(Err(e)),
                        }
                    };
                    tracing::trace!(target: LOG_TARGET, "read: {}/{} bytes", *off + n, len);
                    if n == 0 {
                        tracing::trace!(target: LOG_TARGET, "read: eof");
                        this.read_state = ReadState::Eof(Err(()));
                        return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
                    }
                    *off += n;
                    if len == *off {
                        tracing::trace!(target: LOG_TARGET, "read: decrypting {} bytes", len,);
                        this.decrypt_buffer.resize(len, 0u8);

                        match this
                            .noise
                            .read_message(&this.read_buffer, &mut this.decrypt_buffer)
                        {
                            Ok(nread) => {
                                this.decrypt_buffer.resize(nread, 0u8);
                                let amount = std::cmp::min(buf.len(), nread);
                                let new = this.decrypt_buffer.split_off(amount);
                                buf[..amount].copy_from_slice(&this.decrypt_buffer);

                                if amount >= nread {
                                    this.read_state = ReadState::Ready
                                } else {
                                    this.read_state = ReadState::ReadBuffered { unread: new };
                                }

                                return Poll::Ready(Ok(amount));
                            }
                            Err(error) => {
                                tracing::error!(
                                    target: LOG_TARGET,
                                    ?error,
                                    "read: decryption error",
                                );
                                this.read_state = ReadState::DecErr;
                                return Poll::Ready(Err(io::ErrorKind::InvalidData.into()));
                            }
                        }
                    }
                }
                ReadState::ReadBuffered { ref mut unread } => {
                    let amount = std::cmp::min(buf.len(), unread.len());
                    let new = unread.split_off(amount);
                    buf[..amount].copy_from_slice(unread);

                    if new.is_empty() {
                        this.read_state = ReadState::Ready
                    } else {
                        this.read_state = ReadState::ReadBuffered { unread: new };
                    }

                    return Poll::Ready(Ok(amount));
                }
                ReadState::Eof(_res) => {
                    tracing::trace!(target: LOG_TARGET, "read: eof");
                    todo!();
                    // return Poll::Ready(None);
                }
                ReadState::DecErr => {
                    tracing::trace!(target: LOG_TARGET, "read: decryption error");
                    todo!();
                    // return Poll::Ready(Some(Err(io::ErrorKind::InvalidData.into())))
                }
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin, T: Noise + Unpin> AsyncWrite for NoiseSocket<S, T> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let this = Pin::into_inner(self);
        this.write_buffer
            .resize(buf.len() + NOISE_DECRYPT_EXTRA_ALLOC, 0u8);

        match this.noise.write_message(buf, &mut this.write_buffer) {
            Ok(nwritten) => {
                tracing::trace!(
                    target: LOG_TARGET,
                    size = ?nwritten,
                    "write: send message",
                );
                tracing::event!(
                    target: LOG_TARGET_MSG,
                    tracing::Level::TRACE,
                    buffer =? this.write_buffer[..nwritten],
                );

                let _ = Pin::new(&mut this.io).poll_write(cx, &u16::to_be_bytes(nwritten as u16));
                let _ = Pin::new(&mut this.io).poll_write(cx, &this.write_buffer[..nwritten]);

                Poll::Ready(Ok(buf.len()))
            }
            Err(error) => {
                tracing::error!(target: LOG_TARGET, ?error, "write: encryption error");
                Poll::Ready(Err(io::ErrorKind::InvalidData.into()))
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::into_inner(self).io.flush().poll_unpin(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::into_inner(self).io.close().poll_unpin(cx)
    }
}

/// Try to parse `PeerId` from received `NoiseHandshakePayload`
fn parse_peer_id(buf: &[u8]) -> crate::Result<PeerId> {
    match handshake_schema::NoiseHandshakePayload::decode(buf) {
        Ok(payload) => {
            let public_key = PublicKey::from_protobuf_encoding(&payload.identity_key.ok_or(
                error::Error::NegotiationError(error::NegotiationError::PeerIdMissing),
            )?)?;
            Ok(PeerId::from_public_key(&public_key))
        }
        Err(err) => Err(From::from(err)),
    }
}

/// Noise-encrypted connection.
#[derive(Debug)]
pub struct Encrypted<S: AsyncRead + AsyncWrite + Unpin> {
    /// Underlying socket.
    socket: S,

    /// Noise transport state.
    noise: TransportState,

    /// Buffer used by `snow` as destination for encrypted data.
    write_buffer: Vec<u8>,

    /// Buffer used by `snow` as destination for decrypted data.
    read_buffer: Vec<u8>,

    /// Buffer used by `snow` as destination for decrypted data.
    decrypt_buffer: Vec<u8>,

    /// Read state of the stream.
    read_state: ReadState,

    /// Write state of the stream.
    _write_state: WriteState,
}

// TODO: optimize
impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for Encrypted<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let mut this = Pin::into_inner(self);

        loop {
            match this.read_state {
                ReadState::Ready => {
                    this.read_state = ReadState::ReadLen {
                        buf: [0, 0],
                        off: 0,
                    };
                }
                ReadState::ReadLen { mut buf, mut off } => {
                    let n = match read_frame_len(&mut this.socket, cx, &mut buf, &mut off) {
                        Poll::Ready(Ok(Some(n))) => n,
                        Poll::Ready(Ok(None)) => {
                            this.read_state = ReadState::Eof(Ok(()));
                            todo!();
                            // return Poll::Ready(None);
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Pending => {
                            this.read_state = ReadState::ReadLen { buf, off };
                            return Poll::Pending;
                        }
                    };

                    if n == 0 {
                        this.read_state = ReadState::Ready;
                        continue;
                    }

                    this.read_buffer.resize(usize::from(n), 0u8);
                    this.read_state = ReadState::ReadData {
                        len: usize::from(n),
                        off: 0,
                    }
                }
                ReadState::ReadData { len, ref mut off } => {
                    let n = {
                        let f = Pin::new(&mut this.socket)
                            .poll_read(cx, &mut this.read_buffer[*off..len]);
                        match ready!(f) {
                            Ok(n) => n,
                            Err(e) => return Poll::Ready(Err(e)),
                        }
                    };
                    tracing::trace!(target: LOG_TARGET, "read: {}/{} bytes", *off + n, len);
                    if n == 0 {
                        tracing::trace!(target: LOG_TARGET, "read: eof");
                        this.read_state = ReadState::Eof(Err(()));
                        return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
                    }
                    *off += n;
                    if len == *off {
                        tracing::trace!(target: LOG_TARGET, "read: decrypting {} bytes", len,);
                        this.decrypt_buffer.resize(len, 0u8);

                        match this
                            .noise
                            .read_message(&this.read_buffer, &mut this.decrypt_buffer)
                        {
                            Ok(nread) => {
                                this.decrypt_buffer.resize(nread, 0u8);
                                let amount = std::cmp::min(buf.len(), nread);
                                let new = this.decrypt_buffer.split_off(amount);
                                buf[..amount].copy_from_slice(&this.decrypt_buffer);

                                if amount >= nread {
                                    this.read_state = ReadState::Ready
                                } else {
                                    this.read_state = ReadState::ReadBuffered { unread: new };
                                }

                                return Poll::Ready(Ok(amount));
                            }
                            Err(error) => {
                                tracing::error!(
                                    target: LOG_TARGET,
                                    ?error,
                                    "read: decryption error",
                                );
                                this.read_state = ReadState::DecErr;
                                return Poll::Ready(Err(io::ErrorKind::InvalidData.into()));
                            }
                        }
                    }
                }
                ReadState::ReadBuffered { ref mut unread } => {
                    let amount = std::cmp::min(buf.len(), unread.len());
                    let new = unread.split_off(amount);
                    buf[..amount].copy_from_slice(unread);

                    if new.is_empty() {
                        this.read_state = ReadState::Ready
                    } else {
                        this.read_state = ReadState::ReadBuffered { unread: new };
                    }

                    return Poll::Ready(Ok(amount));
                }
                ReadState::Eof(_res) => {
                    tracing::trace!(target: LOG_TARGET, "read: eof");
                    todo!();
                    // return Poll::Ready(None);
                }
                ReadState::DecErr => {
                    tracing::trace!(target: LOG_TARGET, "read: decryption error");
                    todo!();
                    // return Poll::Ready(Some(Err(io::ErrorKind::InvalidData.into())))
                }
            }
        }
    }
}

// TODO: optimize
impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for Encrypted<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let this = Pin::into_inner(self);
        this.write_buffer
            .resize(buf.len() + NOISE_DECRYPT_EXTRA_ALLOC, 0u8);

        match this.noise.write_message(buf, &mut this.write_buffer) {
            Ok(nwritten) => {
                tracing::trace!(
                    target: LOG_TARGET,
                    size = ?nwritten,
                    "write: send message",
                );
                tracing::trace!(
                    target: LOG_TARGET_MSG,
                    buffer =? this.write_buffer[..nwritten],
                );

                let _ =
                    Pin::new(&mut this.socket).poll_write(cx, &u16::to_be_bytes(nwritten as u16));
                let _ = Pin::new(&mut this.socket).poll_write(cx, &this.write_buffer[..nwritten]);

                Poll::Ready(Ok(buf.len()))
            }
            Err(error) => {
                tracing::error!(target: LOG_TARGET, ?error, "write: encryption error");
                Poll::Ready(Err(io::ErrorKind::InvalidData.into()))
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::into_inner(self).socket.flush().poll_unpin(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::into_inner(self).socket.close().poll_unpin(cx)
    }
}

/// Perform Noise handshake.
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    io: S,
    config: NoiseConfiguration,
) -> crate::Result<(Encrypted<S>, PeerId)> {
    tracing::trace!(target: LOG_TARGET, ?config, "start noise handshake");

    let role = config.role;
    let mut socket = NoiseSocket::new(io, NoiseHandshakeState(config.noise));
    let mut buf = vec![0u8; 2048];

    let peer = match role {
        Role::Dialer => {
            let _ = socket.write(&[]).await?;
            let _ = socket.flush().await?;
            let read = socket.read(&mut buf).await?;
            let _ = socket.write(&config.payload).await?;
            let _ = socket.flush().await?;
            parse_peer_id(&buf[..read])?
        }
        Role::Listener => {
            let _ = socket.read(&mut buf).await?;
            let _ = socket.write(&config.payload).await?;
            let _ = socket.flush().await?;
            let read = socket.read(&mut buf).await?;
            parse_peer_id(&buf[..read])?
        }
    };

    Ok((
        Encrypted {
            socket: socket.io,
            noise: socket.noise.into_transport_mode()?,
            write_buffer: Vec::new(),
            read_buffer: Vec::with_capacity(NOISE_DECRYPT_BUFFER_SIZE),
            decrypt_buffer: Vec::with_capacity(NOISE_DECRYPT_BUFFER_SIZE),
            read_state: ReadState::Ready,
            _write_state: WriteState::Ready,
        },
        peer,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    #[tokio::test]
    async fn noise_handshake() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair1 = Keypair::generate();
        let _keypair2 = Keypair::generate();

        let listener = TcpListener::bind("[::1]:0".parse::<SocketAddr>().unwrap())
            .await
            .unwrap();

        let (stream1, stream2) = tokio::join!(
            TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept()
        );
        let (io1, io2) = {
            let io1 = TokioAsyncReadCompatExt::compat(stream1.unwrap()).into_inner();
            let io1 = Box::new(TokioAsyncWriteCompatExt::compat_write(io1));
            let io2 = TokioAsyncReadCompatExt::compat(stream2.unwrap().0).into_inner();
            let io2 = Box::new(TokioAsyncWriteCompatExt::compat_write(io2));

            (io1, io2)
        };

        let config1 = NoiseConfiguration::new(&keypair1, Role::Dialer);
        let config2 = NoiseConfiguration::new(&keypair1, Role::Listener);

        let (res1, res2) = tokio::join!(handshake(io1, config1), handshake(io2, config2));
        let (mut res1, mut res2) = (res1.unwrap(), res2.unwrap());

        // verify the connection works by reading a string
        let mut buf = vec![0u8; 512];
        let sent = res1.0.write(b"hello, world").await.unwrap();
        res2.0.read_exact(&mut buf[..sent]).await.unwrap();

        assert_eq!(std::str::from_utf8(&buf[..sent]), Ok("hello, world"),);
    }
}
