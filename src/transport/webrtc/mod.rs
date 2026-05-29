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

//! WebRTC transport.

use crate::{
    error::Error,
    transport::{
        manager::TransportHandle,
        webrtc::{
            config::Config, connection::WebRtcConnection, listener::WebRtcListener,
            opening::OpeningWebRtcConnection,
        },
        Endpoint, Transport, TransportBuilder, TransportEvent,
    },
    types::ConnectionId,
    PeerId,
};

use futures::{future::BoxFuture, Future, Stream};
use futures_timer::Delay;
use hickory_resolver::TokioResolver;
use multiaddr::{multihash::Multihash, Multiaddr};
use str0m::{
    channel::{ChannelConfig, ChannelId},
    config::DtlsCert,
    crypto::CryptoError,
    error::DtlsError,
    ice::IceCreds,
    net::{DatagramRecv, Protocol as Str0mProtocol, Receive},
    Candidate, Input, Rtc, RtcError,
};

use tokio::{
    io::ReadBuf,
    sync::mpsc::{channel, error::TrySendError, Sender},
};

use std::{
    collections::{hash_map::Entry, HashMap, VecDeque},
    io::{ErrorKind, Read, Write},
    net::SocketAddr,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

pub(crate) use substream::Substream;

mod connection;
mod listener;
mod opening;
mod substream;
mod util;

pub mod config;

pub(super) mod schema {
    pub(super) mod webrtc {
        include!(concat!(env!("OUT_DIR"), "/webrtc.rs"));
    }

    pub(super) mod noise {
        include!(concat!(env!("OUT_DIR"), "/noise.rs"));
    }
}

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::webrtc";

/// Hardcoded remote fingerprint.
const REMOTE_FINGERPRINT: &str =
    "sha-256 FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF";

/// Connection context.
struct ConnectionContext {
    /// Remote peer ID.
    peer: PeerId,

    /// Connection ID.
    connection_id: ConnectionId,

    /// TX channel for sending datagrams to the connection event loop.
    tx: Sender<Vec<u8>>,
}

/// Events received from opening connections that are handled
/// by the [`WebRtcTransport`] event loop.
enum ConnectionEvent {
    /// Connection established.
    ConnectionEstablished {
        /// Remote peer ID.
        peer: PeerId,

        /// Endpoint.
        endpoint: Endpoint,
    },

    /// Connection to peer closed.
    ConnectionClosed,

    /// Timeout.
    Timeout {
        /// Timeout duration.
        duration: Duration,
    },
}

/// Endpoints of a received UDP datagram: which local socket received it,
/// and the remote socket that sent it.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
struct AddressPair {
    /// Address of the local listening socket that received the datagram.
    local: SocketAddr,
    /// Address of the remote peer that sent the datagram.
    source: SocketAddr,
}

/// WebRTC transport.
pub(crate) struct WebRtcTransport {
    /// Transport context.
    context: TransportHandle,

    /// DTLS certificate.
    dtls_cert: DtlsCert,

    /// WebRtc Listener.
    listener: WebRtcListener,

    /// Datagram buffer size.
    datagram_buffer_size: usize,

    /// Connected peers.
    open: HashMap<AddressPair, ConnectionContext>,

    /// OpeningWebRtc connections.
    opening: HashMap<AddressPair, OpeningWebRtcConnection>,

    /// `ConnectionId -> (peer id, address_pair, endpoint)` mappings.
    connections: HashMap<ConnectionId, (PeerId, AddressPair, Endpoint)>,

    /// Pending timeouts.
    timeouts: HashMap<AddressPair, BoxFuture<'static, ()>>,

    /// Pending events.
    pending_events: VecDeque<TransportEvent>,
}

impl WebRtcTransport {
    /// Create RTC client and open channel for Noise handshake.
    fn make_rtc_client(
        &self,
        ufrag: &str,
        pass: &str,
        source: SocketAddr,
        destination: SocketAddr,
    ) -> (Rtc, ChannelId) {
        let mut rtc = Rtc::builder()
            .set_ice_lite(true)
            .set_dtls_cert(self.dtls_cert.clone())
            .set_fingerprint_verification(false)
            .build(std::time::Instant::now());
        rtc.add_local_candidate(Candidate::host(destination, Str0mProtocol::Udp).unwrap());
        rtc.add_remote_candidate(Candidate::host(source, Str0mProtocol::Udp).unwrap());
        rtc.direct_api()
            .set_remote_fingerprint(REMOTE_FINGERPRINT.parse().expect("parse() to succeed"));
        rtc.direct_api().set_remote_ice_credentials(IceCreds {
            ufrag: ufrag.to_owned(),
            pass: pass.to_owned(),
        });
        rtc.direct_api().set_local_ice_credentials(IceCreds {
            ufrag: ufrag.to_owned(),
            pass: pass.to_owned(),
        });
        rtc.direct_api().set_ice_controlling(false);
        rtc.direct_api().start_dtls(false).unwrap();
        rtc.direct_api().start_sctp(false);

        let noise_channel_id = rtc.direct_api().create_data_channel(ChannelConfig {
            label: "noise".to_string(),
            ordered: false,
            reliability: Default::default(),
            negotiated: Some(0),
            protocol: "".to_string(),
        });

        (rtc, noise_channel_id)
    }

    /// Poll opening connection.
    fn poll_connection(&mut self, addrs: &AddressPair) -> ConnectionEvent {
        let Some(connection) = self.opening.get_mut(addrs) else {
            tracing::warn!(
                target: LOG_TARGET,
                ?addrs,
                "connection doesn't exist",
            );
            return ConnectionEvent::ConnectionClosed;
        };

        loop {
            match connection.poll_process() {
                opening::WebRtcEvent::Timeout { timeout } => {
                    let duration = timeout - Instant::now();

                    match duration.is_zero() {
                        true => match connection.on_timeout() {
                            Ok(()) => continue,
                            Err(error) => {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?addrs,
                                    ?error,
                                    "failed to handle timeout",
                                );

                                return ConnectionEvent::ConnectionClosed;
                            }
                        },
                        false => return ConnectionEvent::Timeout { duration },
                    }
                }
                opening::WebRtcEvent::Transmit {
                    destination,
                    datagram,
                } => {
                    // UNWRAP: local addr originated from self.listener, must be present.
                    let socket = self.listener.socket(&addrs.local).unwrap();
                    if let Err(error) = socket.try_send_to(&datagram, destination) {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?addrs,
                            ?error,
                            "failed to send datagram",
                        );
                    }
                }
                opening::WebRtcEvent::ConnectionClosed => return ConnectionEvent::ConnectionClosed,
                opening::WebRtcEvent::ConnectionOpened { peer, endpoint } => {
                    return ConnectionEvent::ConnectionEstablished { peer, endpoint };
                }
            }
        }
    }

    /// Handle socket input.
    ///
    /// If the datagram was received from an active client, it's dispatched to the connection
    /// handler, if there is space in the queue. If the datagram opened a new connection or it
    /// belonged to a client who is opening, the event loop is instructed to poll the client
    /// until it timeouts.
    ///
    /// Returns `true` if the client should be polled.
    fn on_socket_input(&mut self, addrs: AddressPair, buffer: Vec<u8>) -> crate::Result<bool> {
        if let Entry::Occupied(mut entry) = self.open.entry(addrs) {
            let ConnectionContext {
                peer,
                connection_id,
                tx,
            } = entry.get_mut();

            match tx.try_send(buffer) {
                Ok(_) => return Ok(false),
                Err(TrySendError::Full(_)) => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        ?addrs,
                        ?peer,
                        ?connection_id,
                        "channel full, dropping datagram",
                    );

                    return Ok(false);
                }
                Err(TrySendError::Closed(_)) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?addrs,
                        ?peer,
                        ?connection_id,
                        "connection closed, removing stale entry",
                    );

                    entry.remove();
                    return Ok(false);
                }
            }
        }

        if buffer.is_empty() {
            // str0m crate panics if the buffer doesn't contain at least one byte:
            // https://github.com/algesten/str0m/blob/2c5dc8ee8ddead08699dd6852a27476af6992a5c/src/io/mod.rs#L222
            return Err(Error::InvalidData);
        }

        // if the peer doesn't exist, decode the message and expect to receive `Stun`
        // so that a new connection can be initialized
        let contents: DatagramRecv =
            buffer.as_slice().try_into().map_err(|_| Error::InvalidData)?;

        // If an opening connection already exists for this source, route all packets to it
        if let Some(opening_conn) = self.opening.get_mut(&addrs) {
            tracing::trace!(
                target: LOG_TARGET,
                ?addrs,
                is_stun = is_stun_packet(&buffer),
                "routing packet to existing opening connection"
            );

            if let Err(error) = opening_conn.on_input(contents) {
                tracing::error!(
                    target: LOG_TARGET,
                    ?error,
                    ?addrs,
                    "failed to handle inbound datagram"
                );
            }
            return Ok(true);
        }

        // No existing connection - this should be a STUN packet to create a new connection
        if !is_stun_packet(&buffer) {
            tracing::warn!(
                target: LOG_TARGET,
                ?addrs,
                "received non-stun packet without existing connection, ignoring"
            );
            return Ok(false);
        }

        let stun_message =
            str0m::ice::StunMessage::parse(&buffer).map_err(|_| Error::InvalidData)?;
        let Some((ufrag, pass)) = stun_message.split_username() else {
            tracing::warn!(
                target: LOG_TARGET,
                ?addrs,
                "failed to split username/password",
            );
            return Err(Error::InvalidData);
        };

        tracing::debug!(
            target: LOG_TARGET,
            ?addrs,
            ?ufrag,
            ?pass,
            "received stun message"
        );

        // create new `Rtc` object for the peer and give it the received STUN message
        let (mut rtc, noise_channel_id) =
            self.make_rtc_client(ufrag, pass, addrs.source, addrs.local);

        rtc.handle_input(Input::Receive(
            Instant::now(),
            Receive {
                source: addrs.source,
                proto: Str0mProtocol::Udp,
                destination: addrs.local,
                contents,
            },
        ))
        .expect("client to handle input successfully");

        let connection_id = self.context.next_connection_id();
        let connection = OpeningWebRtcConnection::new(
            rtc,
            connection_id,
            noise_channel_id,
            self.context.keypair.clone(),
            addrs.source,
            addrs.local,
        );
        self.opening.insert(addrs, connection);

        Ok(true)
    }
}

impl TransportBuilder for WebRtcTransport {
    type Config = Config;
    type Transport = WebRtcTransport;

    /// Create new [`Transport`] object.
    fn new(
        context: TransportHandle,
        config: Self::Config,
        _resolver: Arc<TokioResolver>,
    ) -> crate::Result<(Self, Vec<Multiaddr>)>
    where
        Self: Sized,
    {
        if config.listen_addresses.is_empty() {
            return Err(Error::Other(
                "WebRTC transport requires at least one listen address but none were configured"
                    .to_string(),
            ));
        }

        tracing::info!(
            target: LOG_TARGET,
            listen_addresses = ?config.listen_addresses,
            "start webrtc transport",
        );

        // OpenSsl as crypto provider is specified through the 'openssl' str0m feature flag.
        let crypto_provider = str0m::crypto::from_feature_flags();
        let generate_cert = |crypto: &str0m::crypto::CryptoProvider| {
            crypto.dtls_provider.generate_certificate().ok_or(crate::error::Error::WebRtc(
                RtcError::Dtls(DtlsError::CryptoError(CryptoError::Other(
                    "OpenSsl failed to generate certificate".to_string(),
                ))),
            ))
        };

        let dtls_cert = if let Some(dtls_cert_path) = config.dtdl_cert_persistent_path {
            match decode_dtls_cert(dtls_cert_path.clone()) {
                Ok(dtls_cert) => dtls_cert,
                Err(false) => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        dtls_cert_path = ?dtls_cert_path.clone(),
                        "failed to decode dtls cert, not overwriting it");
                    generate_cert(&crypto_provider)?
                }
                Err(true) => {
                    tracing::info!(
                        target: LOG_TARGET,
                        dtls_cert_path = ?dtls_cert_path.clone(),
                        "dtls cert didn't exist, generating new one");
                    let dtls_cert = generate_cert(&crypto_provider)?;
                    if let Err(e) = encode_dtls_cert(dtls_cert_path.clone(), &dtls_cert) {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?dtls_cert_path,
                            ?e,
                            "failed to encode dtls cert to the specified path");
                    }
                    dtls_cert
                }
            }
        } else {
            generate_cert(&crypto_provider)?
        };

        let fingerprint = crypto_provider.sha256_provider.sha256(&dtls_cert.certificate);

        const MULTIHASH_SHA256_CODE: u64 = 0x12;
        let certificate = Multihash::wrap(MULTIHASH_SHA256_CODE, &fingerprint)
            .expect("fingerprint's len to be 32 bytes");

        let (listener, listen_multi_addresses) =
            WebRtcListener::new(config.listen_addresses, certificate)?;

        Ok((
            Self {
                context,
                dtls_cert,
                listener,
                open: HashMap::new(),
                opening: HashMap::new(),
                connections: HashMap::new(),
                timeouts: HashMap::new(),
                pending_events: VecDeque::new(),
                datagram_buffer_size: config.datagram_buffer_size,
            },
            listen_multi_addresses,
        ))
    }
}

impl Transport for WebRtcTransport {
    fn dial(&mut self, connection_id: ConnectionId, address: Multiaddr) -> crate::Result<()> {
        tracing::warn!(
            target: LOG_TARGET,
            ?connection_id,
            ?address,
            "webrtc cannot dial",
        );

        debug_assert!(false);
        Err(Error::NotSupported("webrtc cannot dial peers".to_string()))
    }

    fn accept_pending(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?connection_id,
            "webrtc cannot accept pending connections",
        );

        debug_assert!(false);
        Err(Error::NotSupported(
            "webrtc cannot accept pending connections".to_string(),
        ))
    }

    fn reject_pending(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?connection_id,
            "webrtc cannot reject pending connections",
        );

        debug_assert!(false);
        Err(Error::NotSupported(
            "webrtc cannot reject pending connections".to_string(),
        ))
    }

    fn accept(
        &mut self,
        connection_id: ConnectionId,
    ) -> crate::Result<BoxFuture<'static, crate::Result<()>>> {
        tracing::trace!(
            target: LOG_TARGET,
            ?connection_id,
            "inbound connection accepted",
        );

        let (peer, addrs, endpoint) = self.connections.remove(&connection_id).ok_or_else(|| {
            tracing::warn!(
                target: LOG_TARGET,
                ?connection_id,
                "pending connection doens't exist",
            );

            Error::InvalidState
        })?;

        let connection = self.opening.remove(&addrs).ok_or_else(|| {
            tracing::warn!(
                target: LOG_TARGET,
                ?connection_id,
                "pending connection doens't exist",
            );

            Error::InvalidState
        })?;

        let rtc = connection.on_accept()?;
        let (tx, rx) = channel(self.datagram_buffer_size);
        let mut protocol_set = self.context.protocol_set(connection_id);
        let connection_id = endpoint.connection_id();
        let endpoint_clone = endpoint.clone();
        let executor = self.context.executor.clone();

        self.open.insert(
            addrs,
            ConnectionContext {
                tx,
                peer,
                connection_id,
            },
        );

        // UNWRAP: local addr originated from this.listener, must be present.
        let socket = self.listener.socket(&addrs.local).unwrap();
        Ok(Box::pin(async move {
            // First, notify all protocols about the connection establishment
            protocol_set.report_connection_established(peer, endpoint_clone).await?;

            // After protocols are notified, create connection and spawn event loop
            let connection = WebRtcConnection::new(
                rtc,
                peer,
                addrs.source,
                addrs.local,
                socket,
                protocol_set,
                endpoint,
                rx,
            );

            executor.run(Box::pin(async move {
                connection.run_event_loop().await;
            }));

            Ok(())
        }))
    }

    fn reject(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?connection_id,
            "inbound connection rejected",
        );

        let (_, addrs, _) = self.connections.remove(&connection_id).ok_or_else(|| {
            tracing::warn!(
                target: LOG_TARGET,
                ?connection_id,
                "pending connection doens't exist",
            );

            Error::InvalidState
        })?;

        self.opening
            .remove(&addrs)
            .ok_or_else(|| {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?connection_id,
                    "pending connection doens't exist",
                );

                Error::InvalidState
            })
            .map(|_| ())
    }

    fn open(
        &mut self,
        _connection_id: ConnectionId,
        _addresses: Vec<Multiaddr>,
    ) -> crate::Result<()> {
        Ok(())
    }

    fn negotiate(&mut self, _connection_id: ConnectionId) -> crate::Result<()> {
        Ok(())
    }

    fn cancel(&mut self, _connection_id: ConnectionId) {}
}

impl Stream for WebRtcTransport {
    type Item = TransportEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);

        if let Some(event) = this.pending_events.pop_front() {
            return Poll::Ready(Some(event));
        }

        loop {
            let mut buf = vec![0u8; 16384];
            let mut read_buf = ReadBuf::new(&mut buf);

            let addrs = match this.listener.poll_recv_from(cx, &mut read_buf) {
                // No error is expected to be returned by the listener.
                Poll::Ready(Err(error)) => {
                    tracing::info!(
                        target: LOG_TARGET,
                        ?error,
                        "webrtc udp socket closed",
                    );
                    return Poll::Ready(None);
                }
                Poll::Pending => break,
                Poll::Ready(Ok(addrs)) => addrs,
            };

            let nread = read_buf.filled().len();
            buf.truncate(nread);

            match this.on_socket_input(addrs, buf) {
                Ok(false) => {}
                Ok(true) => loop {
                    match this.poll_connection(&addrs) {
                        ConnectionEvent::ConnectionEstablished { peer, endpoint } => {
                            this.connections
                                .insert(endpoint.connection_id(), (peer, addrs, endpoint.clone()));

                            // keep polling the connection until it registers a timeout
                            this.pending_events.push_back(TransportEvent::ConnectionEstablished {
                                peer,
                                endpoint,
                            });
                        }
                        ConnectionEvent::ConnectionClosed => {
                            this.opening.remove(&addrs);
                            this.timeouts.remove(&addrs);

                            break;
                        }
                        ConnectionEvent::Timeout { duration } => {
                            this.timeouts
                                .insert(addrs, Box::pin(async move { Delay::new(duration).await }));

                            break;
                        }
                    }
                },
                Err(error) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?addrs,
                        ?error,
                        "failed to handle datagram",
                    );
                }
            }
        }

        // go over all pending timeouts to see if any of them have expired
        // and if any of them have, poll the connection until it registers another timeout
        let filtered_timeouts: Vec<_> = this
            .timeouts
            .iter_mut()
            .filter_map(|(source, mut delay)| match Pin::new(&mut delay).poll(cx) {
                Poll::Pending => None,
                Poll::Ready(_) => Some(*source),
            })
            .collect();

        for addrs in filtered_timeouts {
            loop {
                match this.poll_connection(&addrs) {
                    ConnectionEvent::ConnectionEstablished { peer, endpoint } => {
                        this.connections
                            .insert(endpoint.connection_id(), (peer, addrs, endpoint.clone()));
                        this.pending_events
                            .push_back(TransportEvent::ConnectionEstablished { peer, endpoint });
                        // keep polling the connection until it registers a timeout
                    }
                    ConnectionEvent::ConnectionClosed => {
                        this.opening.remove(&addrs);
                        break;
                    }
                    ConnectionEvent::Timeout { duration } => {
                        this.timeouts.insert(addrs, Box::pin(Delay::new(duration)));
                        break;
                    }
                }
            }
        }

        this.timeouts.retain(|addrs, _| this.opening.contains_key(addrs));
        this.pending_events
            .pop_front()
            .map_or(Poll::Pending, |event| Poll::Ready(Some(event)))
    }
}

/// Check if the packet received is STUN.
///
/// Extracted from the STUN RFC 5389 (<https://datatracker.ietf.org/doc/html/rfc5389#page-10>):
///  All STUN messages MUST start with a 20-byte header followed by zero
///  or more Attributes.  The STUN header contains a STUN message type,
///  magic cookie, transaction ID, and message length.
///
/// ```ignore
///      0                   1                   2                   3
///      0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
///     +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///     |0 0|     STUN Message Type     |         Message Length        |
///     +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///     |                         Magic Cookie                          |
///     +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///     |                                                               |
///     |                     Transaction ID (96 bits)                  |
///     |                                                               |
///     +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
fn is_stun_packet(bytes: &[u8]) -> bool {
    const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
    // 20 bytes for the header, then follows attributes.
    bytes.len() >= 20 && bytes[0] < 2 && bytes[4..8] == STUN_MAGIC_COOKIE
}

/// Encodes `dtls_cert` into the `webrtc_certhash` file inside `path`.
///
/// Layout, lengths as big-endian `u64`:
/// `cert len ++ cert bytes ++ key len ++ key bytes`.
///
/// Errors if `path` does not exist.
fn encode_dtls_cert(path: PathBuf, dtls_cert: &DtlsCert) -> std::io::Result<()> {
    if !path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "dtls cert folder doesn't exist",
        ));
    }

    let path = path.join("webrtc_certhash");

    let mut file =
        std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(path)?;

    file.write_all(&(dtls_cert.certificate.len() as u64).to_be_bytes()[..])?;
    file.write_all(&dtls_cert.certificate[..])?;
    file.write_all(&(dtls_cert.private_key.len() as u64).to_be_bytes()[..])?;
    file.write_all(&dtls_cert.private_key[..])?;

    Ok(())
}

/// Decodes a [`DtlsCert`] from the `webrtc_certhash` file inside `path`.
///
/// Layout, lengths as big-endian `u64`:
/// `cert len ++ cert bytes ++ key len ++ key bytes`.
///
/// The `bool` in the error says whether the caller may persist a fresh cert:
/// - true: file absent, generate and save a new one.
/// - false: file present but unreadable/corrupt, do NOT overwrite it
fn decode_dtls_cert(path: PathBuf) -> Result<DtlsCert, bool> {
    let path = path.join("webrtc_certhash");

    let mut file = std::fs::OpenOptions::new().read(true).open(path.clone()).map_err(|e| {
        tracing::debug!(?path, ?e, "failed opening dtls cert file");
        // Create the new dtls cert file only if the file was not existing before.
        match e.kind() {
            ErrorKind::NotFound => true,
            _ => false,
        }
    })?;

    let file_len = file
        .metadata()
        .map_err(|e| {
            tracing::debug!(?path, ?e, "failed access to dtls cert file metadata");
            false
        })
        .map(|metadata| metadata.len() as usize)?;

    let mut read = |buf: &mut [u8]| {
        file.read_exact(buf).map_err(|e| {
            tracing::debug!(?path, ?e, "failed reading buffer from file");
            false
        })
    };

    let mut buf = [0; 8];
    read(&mut buf)?;
    let certificate_len = u64::from_be_bytes(buf) as usize;
    if certificate_len > file_len {
        tracing::debug!(?path, "decoded certificate_len is too big");
        return Err(false);
    }
    let mut certificate = vec![0; certificate_len];
    read(&mut certificate)?;

    read(&mut buf)?;
    let private_key_len = u64::from_be_bytes(buf) as usize;
    // 16 = 2 length u64 prefixes
    if private_key_len != file_len - certificate_len - 16 {
        tracing::debug!(?path, "decoded private key len doesn't match expected one");
        return Err(false);
    }
    let mut private_key = vec![0; private_key_len];
    read(&mut private_key)?;

    Ok(DtlsCert {
        certificate,
        private_key,
    })
}
