// Copyright 2019 Parity Technologies (UK) Ltd.
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

use crate::{
    config::Role,
    crypto::{ed25519::Keypair, PublicKey},
    error, PeerId,
};

use bytes::{Buf, Bytes, BytesMut};
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use prost::Message;
use snow::{Builder, HandshakeState, TransportState};

use std::{
    collections::VecDeque,
    fmt,
    io::{self, IoSlice},
    pin::Pin,
    task::{Context, Poll},
};

mod handshake_schema {
    include!(concat!(env!("OUT_DIR"), "/noise.rs"));
}

/// Noise parameters.
const NOISE_PARAMETERS: &str = "Noise_XX_25519_ChaChaPoly_SHA256";

/// Prefix of static key signatures for domain separation.
pub(crate) const STATIC_KEY_DOMAIN: &str = "noise-libp2p-static-key:";

/// Maximum Noise message size.
const MAX_NOISE_MSG_LEN: usize = 65536;

/// Space given to the encryption buffer to hold key material.
const NOISE_EXTRA_ENCRYPT_SPACE: usize = 1024;

/// Max read ahead factor for the noise socket.
///
/// Specifies how many multiples of `MAX_NOISE_MESSAGE_LEN` are read from the socket
/// using one call to `poll_read()`.
const MAX_READ_AHEAD_FACTOR: usize = 50;

/// Max. length for Noise protocol message payloads.
pub const MAX_FRAME_LEN: usize = MAX_NOISE_MSG_LEN - NOISE_EXTRA_ENCRYPT_SPACE;

/// Logging target for the file.
const LOG_TARGET: &str = "crypto::noise";

#[derive(Debug)]
enum NoiseState {
    Handshake(HandshakeState),
    Transport(TransportState),
}

pub struct NoiseContext {
    keypair: snow::Keypair,
    noise: NoiseState,
    role: Role,
    pub payload: Vec<u8>,
}

impl fmt::Debug for NoiseContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NoiseContext")
            .field("public", &self.noise)
            .field("payload", &self.payload)
            .field("role", &self.role)
            .finish()
    }
}

impl NoiseContext {
    /// Assemble Noise payload and return [`NoiseContext`].
    fn assemble(
        noise: snow::HandshakeState,
        keypair: snow::Keypair,
        id_keys: &Keypair,
        role: Role,
    ) -> Self {
        let noise_payload = handshake_schema::NoiseHandshakePayload {
            identity_key: Some(PublicKey::Ed25519(id_keys.public()).to_protobuf_encoding()),
            identity_sig: Some(
                id_keys.sign(&[STATIC_KEY_DOMAIN.as_bytes(), keypair.public.as_ref()].concat()),
            ),
            ..Default::default()
        };

        let mut payload = Vec::with_capacity(noise_payload.encoded_len());
        noise_payload.encode(&mut payload).expect("Vec<u8> to provide needed capacity");

        Self {
            noise: NoiseState::Handshake(noise),
            keypair,
            payload,
            role,
        }
    }

    // fn new(role: Role) -> Self {
    pub fn new(keypair: &Keypair, role: Role) -> Self {
        tracing::trace!(target: LOG_TARGET, ?role, "create new noise configuration");

        let builder: Builder<'_> =
            Builder::new(NOISE_PARAMETERS.parse().expect("valid Noise pattern"));
        let dh_keypair = builder.generate_keypair().expect("keypair generation to succeed");
        let static_key = &dh_keypair.private;

        let noise = match role {
            Role::Dialer => builder
                .local_private_key(static_key)
                .build_initiator()
                .expect("initialization to succeed"),
            Role::Listener => builder
                .local_private_key(static_key)
                .build_responder()
                .expect("initialization to succeed"),
        };

        Self::assemble(noise, dh_keypair, keypair, role)
    }

    /// Create new [`NoiseContext`] with prologue.
    pub fn with_prologue(id_keys: &Keypair, prologue: Vec<u8>) -> Self {
        let noise = snow::Builder::new(NOISE_PARAMETERS.parse().expect("valid Noise patterns"));
        let keypair = noise.generate_keypair().unwrap();

        let noise = noise
            .local_private_key(&keypair.private)
            .prologue(&prologue)
            .build_initiator()
            .expect("to succeed");

        Self::assemble(noise, keypair, id_keys, Role::Dialer)
    }

    /// Get remote public key from the received Noise payload.
    // TODO: refactor
    pub fn get_remote_public_key(&mut self, reply: &Vec<u8>) -> crate::Result<PublicKey> {
        if reply.len() <= 2 {
            return Err(error::Error::InvalidData);
        }

        // TODO: no unwraps
        let size: Result<[u8; 2], _> = reply[0..2].try_into();
        let _size = u16::from_be_bytes(size.unwrap());

        // TODO: buffer size
        let mut inner = vec![0u8; 1024];

        let NoiseState::Handshake(ref mut noise) = self.noise else {
            panic!("invalid state to read the second handshake message");
        };

        let res = noise.read_message(&reply[2..], &mut inner)?;
        inner.truncate(res);

        let payload = handshake_schema::NoiseHandshakePayload::decode(inner.as_slice())?;

        Ok(PublicKey::from_protobuf_encoding(
            &payload.identity_key.ok_or(error::Error::NegotiationError(
                error::NegotiationError::PeerIdMissing,
            ))?,
        )?)
    }

    /// Get first message.
    ///
    /// Listener only sends one message (the payload)
    pub fn first_message(&mut self, role: Role) -> Vec<u8> {
        match role {
            Role::Dialer => {
                tracing::trace!(target: LOG_TARGET, "get noise dialer first message");

                let NoiseState::Handshake(ref mut noise) = self.noise else {
                    panic!("invalid state to read the second handshake message");
                };

                let mut buffer = vec![0u8; 256];
                let nwritten = noise.write_message(&[], &mut buffer).expect("to succeed");
                buffer.truncate(nwritten);

                let size = nwritten as u16;
                let mut size = size.to_be_bytes().to_vec();
                size.append(&mut buffer);

                size
            }
            Role::Listener => self.second_message(),
        }
    }

    /// Get second message.
    ///
    /// Only the dialer sends the second message.
    pub fn second_message(&mut self) -> Vec<u8> {
        tracing::trace!(target: LOG_TARGET, "get noise paylod message");

        let NoiseState::Handshake(ref mut noise) = self.noise else {
            panic!("invalid state to read the second handshake message");
        };

        let mut buffer = vec![0u8; 2048];
        let nwritten = noise.write_message(&self.payload, &mut buffer).expect("to succeed");
        buffer.truncate(nwritten);

        let size = nwritten as u16;
        let mut size = size.to_be_bytes().to_vec();
        size.append(&mut buffer);

        size
    }

    /// Read handshake message.
    async fn read_handshake_message<T: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        io: &mut T,
    ) -> crate::Result<Bytes> {
        let mut size = BytesMut::zeroed(2);
        io.read_exact(&mut size).await?;
        let size = size.get_u16();

        let mut message = BytesMut::zeroed(size as usize);
        io.read_exact(&mut message).await?;

        let mut out = BytesMut::new();
        out.resize(message.len() + 200, 0u8); // TODO: correct overhead

        let NoiseState::Handshake(ref mut noise) = self.noise else {
            panic!("invalid state to read handshake message");
        };

        let nread = noise.read_message(&message, &mut out)?;
        out.truncate(nread);

        Ok(out.freeze())
    }

    /// Read Noise message
    fn read_message(&mut self, message: &[u8]) -> Result<BytesMut, snow::Error> {
        let mut out = BytesMut::new();
        out.resize(message.len() + 200, 0u8); // TODO: correct overhead

        let nread = match self.noise {
            NoiseState::Handshake(ref mut noise) => noise.read_message(message, &mut out)?,
            NoiseState::Transport(ref mut noise) => noise.read_message(message, &mut out)?,
        };

        out.truncate(nread);
        Ok(out)
    }

    /// Write Noise message.
    fn write_message(&mut self, message: &[u8]) -> Result<BytesMut, snow::Error> {
        let mut out = BytesMut::new();
        out.resize(message.len() + 256, 0u8); // TODO: correct overhead

        let nread = match self.noise {
            NoiseState::Handshake(ref mut noise) => noise.write_message(message, &mut out)?,
            NoiseState::Transport(ref mut noise) => noise.write_message(message, &mut out)?,
        };

        out.truncate(nread);
        Ok(out)
    }

    /// Convert Noise into transport mode.
    fn into_transport(self) -> NoiseContext {
        let transport = match self.noise {
            NoiseState::Handshake(noise) => noise.into_transport_mode().unwrap(),
            NoiseState::Transport(_) => panic!("invalid state"),
        };

        NoiseContext {
            keypair: self.keypair,
            payload: self.payload,
            role: self.role,
            noise: NoiseState::Transport(transport),
        }
    }
}

pub struct NoiseSocket<S: AsyncRead + AsyncWrite + Unpin> {
    io: S,
    noise: NoiseContext,
    read_buffer: BytesMut,
    offset: usize,
    pending_frame: Option<BytesMut>,
    pending_frames: VecDeque<BytesMut>,
    current_frame_size: Option<usize>,
    write_state: WriteState,
}

impl<S: AsyncRead + AsyncWrite + Unpin> NoiseSocket<S> {
    fn new(io: S, noise: NoiseContext) -> Self {
        let mut read_buffer = BytesMut::new();
        read_buffer.resize(MAX_READ_AHEAD_FACTOR * MAX_NOISE_MSG_LEN, 0u8);

        Self {
            io,
            noise,
            read_buffer,
            offset: 0usize,
            pending_frame: None,
            pending_frames: VecDeque::new(),
            current_frame_size: None,
            write_state: WriteState::Ready,
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for NoiseSocket<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = Pin::into_inner(self);

        loop {
            if let None = this.pending_frame {
                this.pending_frame = this.pending_frames.pop_front();
            }

            // if there is already a pending frame, try returning that first
            if let Some(ref mut pending) = this.pending_frame {
                tracing::trace!(target: LOG_TARGET, pending_len = ?pending.len(), "process pending frame");

                let (len, remaining) = match pending.len() <= buf.len() {
                    true => (pending.len(), None),
                    false => (buf.len(), Some(pending.split_off(buf.len()))),
                };

                buf[..len].copy_from_slice(pending.as_ref());
                this.pending_frame = remaining;
                return Poll::Ready(Ok(len));
            }

            tracing::trace!(target: LOG_TARGET, "try to read bytes from socket");

            // if there is no pending frame, try to read data from the socket
            let nread =
                match Pin::new(&mut this.io).poll_read(cx, &mut this.read_buffer[this.offset..]) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Ready(Ok(nread)) => match nread == 0 {
                        true => return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into())),
                        false => nread,
                    },
                };

            tracing::trace!(target: LOG_TARGET, ?nread, "read bytes from socket");

            this.offset += nread;
            this.read_buffer.truncate(this.offset);

            // try to split the read bytes into noise frames and decrypt them
            loop {
                if this.read_buffer.len() < 2 {
                    break;
                }

                // get frame size, either from current or previous iteration
                let frame_size = match this.current_frame_size.take() {
                    Some(frame_size) => frame_size,
                    None => {
                        this.offset -= 2;
                        this.read_buffer.get_u16() as usize
                    }
                };

                if this.read_buffer.len() < frame_size {
                    this.current_frame_size = Some(frame_size);
                    break;
                }

                let frame = this.read_buffer.split_to(frame_size);
                this.offset -= frame.len();

                match this.noise.read_message(&frame) {
                    Ok(message) => this.pending_frames.push_back(message),
                    Err(error) => {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?error,
                            role = ?this.noise.role,
                            "failed to read noise message"
                        );
                        return Poll::Ready(Err(io::ErrorKind::InvalidData.into()));
                    }
                }
            }
            this.read_buffer.resize(
                this.read_buffer.len() + MAX_READ_AHEAD_FACTOR * MAX_NOISE_MSG_LEN,
                0u8,
            );

            continue;
        }
    }
}

enum WriteState {
    Ready,
    WriteFrame {
        size: usize,
        frame: bytes::buf::Chain<Bytes, Bytes>,
    },
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for NoiseSocket<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = Pin::into_inner(self);

        tracing::info!("send data");

        loop {
            match &mut this.write_state {
                WriteState::Ready => {
                    let (frame, size): (bytes::buf::Chain<Bytes, Bytes>, usize) =
                        match buf.chunks(MAX_FRAME_LEN).next() {
                            None => return Poll::Ready(Ok(0usize)),
                            Some(chunk) => match this.noise.write_message(chunk) {
                                Ok(frame) => {
                                    let original_size = chunk.len();
                                    // TODO: `#![feature(slice_flatten)]`
                                    let size: Vec<u8> = u16::to_be_bytes(frame.len() as u16)
                                        .try_into()
                                        .expect("to succeed");
                                    let size: Bytes = size.into();
                                    (size.chain(frame.freeze().into()), original_size)
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        target: LOG_TARGET,
                                        ?error,
                                        role = ?this.noise.role,
                                        "failed to write noise message"
                                    );
                                    return Poll::Ready(Err(io::ErrorKind::InvalidData.into()));
                                }
                            },
                        };

                    this.write_state = WriteState::WriteFrame { frame, size };
                }
                WriteState::WriteFrame { frame, size } => {
                    let mut bufs = [IoSlice::new(&[]); 2];
                    frame.chunks_vectored(&mut bufs);

                    match futures::ready!(Pin::new(&mut this.io).poll_write_vectored(cx, &bufs)) {
                        Ok(nwritten) => {
                            frame.advance(nwritten);

                            tracing::trace!(target: LOG_TARGET, ?nwritten, "wrote noise message");

                            if frame.remaining() == 0 {
                                let original_len = *size;
                                this.write_state = WriteState::Ready;
                                return Poll::Ready(Ok(original_len));
                            }
                        }
                        Err(error) => return Poll::Ready(Err(error)),
                    }
                }
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_close(cx)
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

/// Perform Noise handshake.
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    mut io: S,
    keypair: &Keypair,
    role: Role,
) -> crate::Result<(NoiseSocket<S>, PeerId)> {
    tracing::debug!(target: LOG_TARGET, ?role, "start noise handshake");

    let mut noise = NoiseContext::new(keypair, role);
    let peer = match role {
        Role::Dialer => {
            // write initial message
            let first_message = noise.first_message(Role::Dialer);
            let _ = io.write(&first_message).await?;
            let _ = io.flush().await?;

            // read back response which contains the remote peer id
            let message = noise.read_handshake_message(&mut io).await?;

            // send the final message which contains local peer id
            let second_message = noise.second_message();
            let _ = io.write(&second_message).await?;
            let _ = io.flush().await?;

            parse_peer_id(&message)?
        }
        Role::Listener => {
            // read remote's first message
            let _ = noise.read_handshake_message(&mut io).await?;

            // send local peer id.
            let second_message = noise.second_message();
            let _ = io.write(&second_message).await?;
            let _ = io.flush().await?;

            // read remote's second message which contains their peer id
            let message = noise.read_handshake_message(&mut io).await?;
            parse_peer_id(&message)?
        }
    };

    Ok((NoiseSocket::new(io, noise.into_transport()), peer))
}

// TODO: add more tests
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
        let keypair2 = Keypair::generate();

        let peer1_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair1.public()));
        let peer2_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair2.public()));

        let listener = TcpListener::bind("[::1]:0".parse::<SocketAddr>().unwrap()).await.unwrap();

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

        let (res1, res2) = tokio::join!(
            handshake(io1, &keypair1, Role::Dialer),
            handshake(io2, &keypair2, Role::Listener)
        );
        let (mut res1, mut res2) = (res1.unwrap(), res2.unwrap());

        assert_eq!(res1.1, peer2_id);
        assert_eq!(res2.1, peer1_id);

        // verify the connection works by reading a string
        let mut buf = vec![0u8; 512];
        let sent = res1.0.write(b"hello, world").await.unwrap();
        res2.0.read_exact(&mut buf[..sent]).await.unwrap();

        assert_eq!(std::str::from_utf8(&buf[..sent]), Ok("hello, world"),);
    }
}
