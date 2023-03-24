#![allow(unused)]

use anyhow::Result;
use asynchronous_codec::{Framed, FramedParts};
use byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use bytes::BytesMut;
use ed25519_dalek::{Keypair, Signer};
use futures::{
    prelude::*,
    ready,
    stream::{Stream, StreamExt},
};
use futures_util::{
    io::AsyncReadExt as FuturesAsyncReadExt, io::AsyncWriteExt as FuturesAsyncWriteExt, SinkExt,
    TryStreamExt,
};
use multiaddr::Multiaddr;
use multihash::{Code, Multihash, MultihashDigest};
use multistream_select::*;
use prost::Message;
use rand::{
    rngs::{OsRng, StdRng},
    Rng, SeedableRng,
};
use snow::{params::NoiseParams, Builder};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use unsigned_varint::codec::UviBytes;
use unsigned_varint::{
    decode::{self, Error},
    encode,
};
use zeroize::*;

use std::io::{self, Read};
use std::{net::SocketAddr, pin::Pin, sync::Arc, task::Poll, time::Duration};

use crate::error::{DecodingError, SigningError};

mod ed25519;
mod error;

// TODO: start refactoring the code into an actual library
// TODO: think about how to use yamux efficiently?
// TODO   - how to handle each peer connection?
// TODO   - how to handle each protocol (possibly multiple substreams)?
// TODO   - how to handle each substream of a protocol?
// TODO: try establishing outbound substream and verify that it works
// TODO: add proper support for ping
// TODO: add support for kamdemlia
// TODO: add support for identify

/// Noise pattern.
static PATTERN: &str = "Noise_XX_25519_ChaChaPoly_SHA256";

/// Public keys with byte-lengths smaller than `MAX_INLINE_KEY_LENGTH` will be
/// automatically used as the peer id using an identity multihash.
const MAX_INLINE_KEY_LENGTH: usize = 42;

static SECRET: &[u8] = b"i don't care for fidget spinners";

/// Prefix of static key signatures for domain separation.
const STATIC_KEY_DOMAIN: &str = "noise-libp2p-static-key:";

/// Noise decrypt buffer size.
const NOISE_DECRYPT_BUFFER_SIZE: usize = 65536;

// pub(crate) mod v1 {
//     include!(concat!(env!("OUT_DIR"), "/types.rs"));
// }
lazy_static::lazy_static! {
    static ref PARAMS: NoiseParams = "Noise_XX_25519_ChaChaPoly_SHA256".parse().unwrap();
}

// TODO: fix this
mod types {
    include!(concat!(env!("OUT_DIR"), "/_.rs"));
}

mod keys_proto {
    include!(concat!(env!("OUT_DIR"), "/keys_proto.rs"));
}

impl std::fmt::Debug for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PeerId").field(&self.to_base58()).finish()
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.to_base58().fmt(f)
    }
}

impl PeerId {
    /// Builds a `PeerId` from a public key.
    pub fn from_public_key(key: &PublicKey) -> PeerId {
        let key_enc = key.to_protobuf_encoding();

        let hash_algorithm = if key_enc.len() <= MAX_INLINE_KEY_LENGTH {
            Code::Identity
        } else {
            Code::Sha2_256
        };

        let multihash = hash_algorithm.digest(&key_enc);

        PeerId { multihash }
    }

    // /// Parses a `PeerId` from bytes.
    pub fn from_bytes(data: &[u8]) -> Result<PeerId, Error> {
        // TODO: fix
        Ok(PeerId::from_multihash(Multihash::from_bytes(data).unwrap()).unwrap())
    }

    // /// Tries to turn a `Multihash` into a `PeerId`.
    // ///
    // /// If the multihash does not use a valid hashing algorithm for peer IDs,
    // /// or the hash value does not satisfy the constraints for a hashed
    // /// peer ID, it is returned as an `Err`.
    pub fn from_multihash(multihash: Multihash) -> Result<PeerId, Multihash> {
        match Code::try_from(multihash.code()) {
            Ok(Code::Sha2_256) => Ok(PeerId { multihash }),
            Ok(Code::Identity) if multihash.digest().len() <= MAX_INLINE_KEY_LENGTH => {
                Ok(PeerId { multihash })
            }
            _ => Err(multihash),
        }
    }

    // /// Tries to extract a [`PeerId`] from the given [`Multiaddr`].
    // ///
    // /// In case the given [`Multiaddr`] ends with `/p2p/<peer-id>`, this function
    // /// will return the encapsulated [`PeerId`], otherwise it will return `None`.
    // pub fn try_from_multiaddr(address: &Multiaddr) -> Option<PeerId> {
    //     address.iter().last().and_then(|p| match p {
    //         Protocol::P2p(hash) => PeerId::from_multihash(hash).ok(),
    //         _ => None,
    //     })
    // }

    // /// Generates a random peer ID from a cryptographically secure PRNG.
    // ///
    // /// This is useful for randomly walking on a DHT, or for testing purposes.
    // pub fn random() -> PeerId {
    //     let peer_id = rand::thread_rng().gen::<[u8; 32]>();
    //     PeerId {
    //         multihash: Multihash::wrap(Code::Identity.into(), &peer_id)
    //             .expect("The digest size is never too large"),
    //     }
    // }

    /// Returns a raw bytes representation of this `PeerId`.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.multihash.to_bytes()
    }

    /// Returns a base-58 encoded string of this `PeerId`.
    pub fn to_base58(&self) -> String {
        bs58::encode(self.to_bytes()).into_string()
    }
}

impl From<PublicKey> for PeerId {
    fn from(key: PublicKey) -> PeerId {
        PeerId::from_public_key(&key)
    }
}

impl From<&PublicKey> for PeerId {
    fn from(key: &PublicKey) -> PeerId {
        PeerId::from_public_key(key)
    }
}

impl TryFrom<Vec<u8>> for PeerId {
    type Error = Vec<u8>;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        PeerId::from_bytes(&value).map_err(|_| value)
    }
}

impl TryFrom<Multihash> for PeerId {
    type Error = Multihash;

    fn try_from(value: Multihash) -> Result<Self, Self::Error> {
        PeerId::from_multihash(value)
    }
}

impl AsRef<Multihash> for PeerId {
    fn as_ref(&self) -> &Multihash {
        &self.multihash
    }
}

impl From<PeerId> for Multihash {
    fn from(peer_id: PeerId) -> Self {
        peer_id.multihash
    }
}

impl From<PeerId> for Vec<u8> {
    fn from(peer_id: PeerId) -> Self {
        peer_id.to_bytes()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("base-58 decode error: {0}")]
    B58(#[from] bs58::decode::Error),
    #[error("decoding multihash failed")]
    MultiHash,
}

impl std::str::FromStr for PeerId {
    type Err = ParseError;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = bs58::decode(s).into_vec()?;
        PeerId::from_bytes(&bytes).map_err(|_| ParseError::MultiHash)
    }
}

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

enum WriteState {
    Ready,
    WriteLen { buf: [u8; 2], off: usize },
    WriteData { len: usize, off: usize },
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

/// Write 2 bytes as frame length from the given buffer into the given sink.
///
/// Panics if `off >= 2`.
///
/// When [`Poll::Pending`] is returned, the given offset
/// may have been updated (i.e. a byte may have been written) and must
/// be preserved for the next invocation.
///
/// Returns `false` if EOF has been encountered.
fn write_frame_len<W: AsyncWrite + Unpin>(
    mut io: &mut W,
    cx: &mut std::task::Context<'_>,
    buf: &[u8; 2],
    off: &mut usize,
) -> Poll<io::Result<bool>> {
    loop {
        match ready!(Pin::new(&mut io).poll_write(cx, &buf[*off..])) {
            Ok(n) => {
                if n == 0 {
                    return Poll::Ready(Ok(false));
                }
                *off += n;
                if *off == 2 {
                    return Poll::Ready(Ok(true));
                }
            }
            Err(e) => {
                return Poll::Ready(Err(e));
            }
        }
    }
}

struct EncryptedConnection<S: AsyncRead + AsyncWrite + Unpin> {
    /// Underlying connection.
    socket: S,

    /// Noise transport state.
    noise: snow::TransportState,

    /// Buffer used by `snow` as destination for encrypted data.
    write_buffer: Vec<u8>,

    /// Buffer used by `snow` as destination for decrypted data.
    read_buffer: Vec<u8>,

    /// Buffer used by `snow` as destination for decrypted data.
    decrypt_buffer: Vec<u8>,

    /// Read state of the stream.
    read_state: ReadState,

    /// Write state of the stream.
    write_state: WriteState,
}

impl<S: AsyncRead + AsyncWrite + Unpin> EncryptedConnection<S> {
    pub fn new(socket: S, noise: snow::TransportState) -> Self {
        Self {
            socket,
            noise,
            write_buffer: Vec::new(),
            read_buffer: Vec::with_capacity(NOISE_DECRYPT_BUFFER_SIZE),
            decrypt_buffer: Vec::with_capacity(NOISE_DECRYPT_BUFFER_SIZE),
            read_state: ReadState::Ready,
            write_state: WriteState::Ready,
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> futures::AsyncRead for EncryptedConnection<S> {
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
                        Poll::Ready(Ok(Some(n))) => dbg!(n),
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
                    println!("read: {}/{} bytes", *off + n, len);
                    if n == 0 {
                        println!("read: eof");
                        this.read_state = ReadState::Eof(Err(()));
                        return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
                    }
                    *off += n;
                    if len == *off {
                        println!(
                            "read: decrypting {} bytes, buffer size {}, {} {}",
                            len,
                            buf.len(),
                            this.read_buffer.len(),
                            this.decrypt_buffer.len(),
                        );
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
                                    println!("buffer read");
                                    this.read_state = ReadState::ReadBuffered { unread: new };
                                }

                                return Poll::Ready(Ok(amount));
                            }
                            Err(err) => {
                                println!("read: decryption error: {err:?}");
                                this.read_state = ReadState::DecErr;
                                return Poll::Ready(Err(io::ErrorKind::InvalidData.into()));
                            }
                        }
                    }
                }
                ReadState::ReadBuffered { ref mut unread } => {
                    println!(
                        "read buffered data, unread {}, buffer {}",
                        unread.len(),
                        buf.len()
                    );
                    let amount = std::cmp::min(buf.len(), unread.len());
                    let new = unread.split_off(amount);
                    buf[..amount].copy_from_slice(&unread);

                    if new.is_empty() {
                        println!("read done");
                        this.read_state = ReadState::Ready
                    } else {
                        println!("buffer read");
                        println!("{:?}", std::str::from_utf8(&new));
                        this.read_state = ReadState::ReadBuffered { unread: new };
                    }

                    return Poll::Ready(Ok(amount));
                }
                ReadState::Eof(res) => {
                    println!("read: eof");
                    todo!();
                    // return Poll::Ready(None);
                }
                ReadState::Eof(Err(())) => {
                    println!("read: eof (unexpected)");
                    todo!();
                    // return Poll::Ready(Some(Err(io::ErrorKind::UnexpectedEof.into())));
                }
                ReadState::DecErr => {
                    todo!();
                    // return Poll::Ready(Some(Err(io::ErrorKind::InvalidData.into())))
                }
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> futures::AsyncWrite for EncryptedConnection<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let mut this = Pin::into_inner(self);
        this.write_buffer.resize(buf.len() + 1024, 0u8); // TODO: remove magic number

        match this.noise.write_message(&buf, &mut this.write_buffer) {
            Ok(nwritten) => {
                let size = u16::to_be_bytes(nwritten as u16);
                // println!("encrypted buffer, size {nwritten}");
                // println!("size in be {size:?} data buffer {:?}", this.write_buffer);
                Pin::new(&mut this.socket).poll_write(cx, &size);
                Pin::new(&mut this.socket).poll_write(cx, &this.write_buffer[..nwritten]);
                return Poll::Ready(Ok(buf.len()));
            }
            Err(err) => {
                println!("failed to encrypt buffer: {err:?}");
                return Poll::Ready(Err(io::ErrorKind::InvalidData.into()));
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

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let (stream, _) = TcpListener::bind("[::1]:8888".parse::<SocketAddr>().unwrap())
        .await?
        .accept()
        .await?;
    let mut buf = vec![0u8; 1024];
    let mut in_buf = vec![0u8; 1024];

    // TODO: generate peer identity

    let stream = TokioAsyncReadCompatExt::compat(stream).into_inner();
    let mut stream = TokioAsyncWriteCompatExt::compat_write(stream);

    let keypair = ed25519::Keypair::generate();
    let public_key = keypair.public();
    let wrapper = PublicKey::Ed25519(public_key.clone());
    let peer_id = wrapper.to_peer_id();

    println!("peer id {}", peer_id);

    // Initialize our responder using a builder.
    let builder: Builder<'_> = Builder::new(PARAMS.clone());
    let dh_keypair = builder.generate_keypair().unwrap();
    let static_key = dh_keypair.private;
    let mut noise = builder
        .local_private_key(&static_key)
        .build_responder()
        .unwrap();

    let protos = Vec::from(["/multistream/1.0.0", "/noise"]);
    let (protocol, mut io) = listener_select_proto(stream, protos).await?;

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let len1 = io.read(&mut in_buf).await?;

    println!("read bytes {len1}");

    // <- e
    let len = noise.read_message(&in_buf[2..len1], &mut buf)?;

    println!("ephemeral key read {len}");

    let noise_payload = types::NoiseHandshakePayload {
        identity_key: Some(PublicKey::Ed25519(public_key).to_protobuf_encoding()),
        identity_sig: Some(
            keypair.sign(&[STATIC_KEY_DOMAIN.as_bytes(), dh_keypair.public.as_ref()].concat()),
        ),
        ..Default::default()
    };

    let mut msg = Vec::with_capacity(noise_payload.encoded_len());
    noise_payload
        .encode(&mut msg)
        .expect("Vec<u8> provides capacity as needed");

    // -> e, ee, s, es
    let len = noise.write_message(&msg, &mut buf[2..])?;

    // TODO: zzz
    assert!(len <= 255);
    buf[0] = 0;
    buf[1] = len as u8;

    io.write_all(&buf[..len + 2]).await?;

    println!("write size {len}");

    println!("payload sent");
    // tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // <- s, se
    let read = io.read(&mut in_buf).await?;
    println!("read {read} bytes");
    noise.read_message(&in_buf[2..read], &mut buf)?;

    println!("handshake part 2 done");

    // Transition the state machine into transport mode now that the handshake is complete.
    let noise = noise.into_transport_mode()?;

    // println!("noise done");

    // let mut buffer = vec![0u8; 52];

    // let read = io.read_exact(&mut buffer).await?;

    // println!("52 bytes read");
    // println!("{:?}", &buffer[2..]);

    // let mut decrypt_buffer = vec![0u8; 1024];
    // noise.read_message(&in_buf[2..], &mut decrypt_buffer)?;

    // println!("all done");

    let encrypted = EncryptedConnection::new(io, noise);
    let protos = Vec::from(["/yamux/1.0.0"]);
    let (protocol, mut io) = listener_select_proto(encrypted, protos).await?;

    let mut connection = yamux::Connection::new(io, yamux::Config::default(), yamux::Mode::Server);
    let (mut control, mut connection) = yamux::Control::new(connection);

    while let Some(event) = connection.next().await {
        match event {
            Ok(mut substream) => {
                tokio::spawn(async move {
                    // TODO: add all supported protocols.
                    let protos = Vec::from(["/ipfs/ping/1.0.0"]);
                    let (protocol, mut socket) =
                        listener_select_proto(substream, protos).await.unwrap();

                    // TODO: start correct protocol handler based on the value of `protocol`
                    println!("selected protocol {protocol:?}");

                    // TODO: answer to pings
                    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                });
            }
            Err(err) => {
                println!("failed to receive inbound substream: {err:?}");
            }
        }
    }

    // loop {
    //     tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    // }

    // let protos = Vec::from(["/multistream/1.0.0", "/plaintext/2.0.0"]);
    // let (protocol, mut io) = listener_select_proto(stream, protos).await?;

    // println!("negotiated protocols {protocol:?}");

    // let mut decoder = Framed::new(io, UviBytes::<bytes::Bytes>::default());

    // let handshake = match decoder.next().await {
    //     Some(p) => types::Exchange::decode(p.unwrap()).unwrap(),
    //     None => {
    //         panic!("failed to receive handshake")
    //     }
    // };

    // println!("received handshake: {handshake:?}");

    // let keypair = ed25519::Keypair::generate();
    // let public_key = keypair.public();
    // let wrapper = PublicKey::Ed25519(public_key);
    // let peer_id = wrapper.to_peer_id();

    // println!("peer id {}", peer_id);

    // let exchange = types::Exchange {
    //     id: Some(wrapper.to_peer_id().to_bytes()),
    //     pubkey: Some(wrapper.to_protobuf_encoding()),
    // };
    // let mut buf = Vec::with_capacity(exchange.encoded_len());
    // exchange
    //     .encode(&mut buf)
    //     .expect("Vec<u8> provides capacity as needed");

    // decoder.send(BytesMut::from(&buf[..]).into()).await?;

    // let FramedParts {
    //     mut io,
    //     read_buffer,
    //     write_buffer,
    //     ..
    // } = decoder.into_parts();
    // let protos = Vec::from(["/multistream/1.0.0", "/yamux/1.0.0"]);
    // let (protocol, mut io) = listener_select_proto(io, protos).await?;

    // println!("yamux negotiated");

    // let mut connection = yamux::Connection::new(io, yamux::Config::default(), yamux::Mode::Server);
    // let (mut control, mut connection) = yamux::Control::new(connection);

    // while let Some(event) = connection.next().await {
    //     match event {
    //         Ok(mut substream) => {
    //             tokio::spawn(async move {
    //                 // TODO: add all supported protocols.
    //                 let protos = Vec::from(["/ipfs/ping/1.0.0"]);
    //                 let (protocol, mut socket) =
    //                     listener_select_proto(substream, protos).await.unwrap();

    //                 // TODO: start correct protocol handler based on the value of `protocol`
    //                 println!("selected protocol {protocol:?}");

    //                 // TODO: answer to pings
    //                 tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    //             });
    //         }
    //         Err(err) => {
    //             println!("failed to receive inbound substream: {err:?}");
    //         }
    //     }
    // }

    Ok(())
}
