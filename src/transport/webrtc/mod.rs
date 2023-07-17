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

use crate::{
    codec::unsigned_varint::UnsignedVarint,
    crypto::{ed25519::Keypair, noise::STATIC_KEY_DOMAIN, PublicKey},
    error::{AddressError, Error, NegotiationError},
    peer_id::PeerId,
    transport::{Transport, TransportCommand, TransportContext},
};

use multiaddr::{multihash::Multihash, Multiaddr, Protocol};
use prost::Message;
use str0m::{
    change::{DtlsCert, Fingerprint, IceCreds},
    channel::{ChannelConfig, ChannelData, ChannelId},
    net::{self, DatagramRecv, Receive},
    Candidate, Event, IceConnectionState, Input, Output, Rtc,
};
use tokio::{net::UdpSocket, sync::mpsc::Receiver};
use tokio_util::codec::{Decoder, Encoder};

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    ops::Deref,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

mod schema {
    pub(super) mod webrtc {
        include!(concat!(env!("OUT_DIR"), "/webrtc.rs"));
    }

    pub(super) mod noise {
        include!(concat!(env!("OUT_DIR"), "/noise.rs"));
    }
}

enum State {
    /// Connection state is poisoned.
    Poisoned,

    /// Connection state is closed.
    Closed,

    /// Connection state is opened.
    Opened {
        /// Noise handshake state.
        noise: snow::HandshakeState,

        /// Noise keypair.
        keypair: snow::Keypair,
    },

    /// Handshake has been sent
    HandshakeSent {
        /// Noise handshake state.
        noise: snow::HandshakeState,

        /// Noise keypair.
        keypair: snow::Keypair,
    },

    /// Connection is open.
    Open {
        /// Noise handshake state.
        noise: snow::HandshakeState,

        /// Noise keypair.
        keypair: snow::Keypair,
    },
}

/// Logging target for the file.
const LOG_TARGET: &str = "webrtc";

#[derive(Debug)]
pub struct WebRtcConfig {
    listen_address: Multiaddr,
}

/// Create Noise handshake state and keypair.
fn noise_prologue(
    local_fingerprint: Fingerprint,
    remote_fingerprint: Fingerprint,
) -> (snow::HandshakeState, snow::Keypair) {
    let noise = snow::Builder::new("Noise_XX_25519_ChaChaPoly_SHA256".parse().unwrap());
    let keypair = noise.generate_keypair().unwrap();

    const MULTIHASH_SHA256_CODE: u64 = 0x12;
    let remote_fingerprint = Multihash::wrap(MULTIHASH_SHA256_CODE, &remote_fingerprint.bytes)
        .expect("fingerprint's len to be 32 bytes")
        .to_bytes();
    let local_fingerprint = Multihash::wrap(MULTIHASH_SHA256_CODE, &local_fingerprint.bytes)
        .expect("fingerprint's len to be 32 bytes")
        .to_bytes();

    const PREFIX: &[u8] = b"libp2p-webrtc-noise:";
    let mut prologue =
        Vec::with_capacity(PREFIX.len() + local_fingerprint.len() + remote_fingerprint.len());
    prologue.extend_from_slice(PREFIX);
    prologue.extend_from_slice(&remote_fingerprint);
    prologue.extend_from_slice(&local_fingerprint);

    (
        noise
            .local_private_key(&keypair.private)
            .prologue(&prologue)
            .build_initiator()
            .unwrap(),
        keypair,
    )
}

/// Create first Noise handshake message.
fn noise_first_message(noise: &mut snow::HandshakeState) -> Vec<u8> {
    let mut buffer = vec![0u8; 256];

    let nwritten = noise.write_message(&[], &mut buffer).unwrap();
    buffer.truncate(nwritten);

    let size = nwritten as u16;
    let mut size = size.to_be_bytes().to_vec();
    size.append(&mut buffer);

    let protobuf_payload = schema::webrtc::Message {
        message: Some(size),
        ..Default::default()
    };
    let mut payload = Vec::with_capacity(protobuf_payload.encoded_len());
    protobuf_payload
        .encode(&mut payload)
        .expect("Vec<u8> to provide needed capacity");

    let payload: bytes::Bytes = payload.into();

    let mut out_buf = bytes::BytesMut::with_capacity(1024);
    let mut codec = UnsignedVarint::new();
    let _ = codec.encode(payload, &mut out_buf);

    out_buf.into()
}

/// Read remote reply and extract remote's `PeerId` from it.
fn read_remote_reply(data: &[u8], noise: &mut snow::HandshakeState) -> crate::Result<PeerId> {
    let mut codec = UnsignedVarint::new();
    let mut stuff = bytes::BytesMut::from(data);
    let result = codec.decode(&mut stuff)?.ok_or(Error::Unknown)?; // TODO: bad error

    let payload = schema::webrtc::Message::decode(result)?;
    let size: Result<[u8; 2], _> = payload.message.clone().unwrap()[0..2].try_into();
    let _size = u16::from_be_bytes(size.unwrap());

    let mut inner = vec![0u8; 1024];

    // TODO: don't panic
    let res = noise.read_message(&payload.message.unwrap()[2..], &mut inner)?;
    inner.truncate(res);

    // TODO: refactor this code
    let payload = schema::noise::NoiseHandshakePayload::decode(inner.as_slice())?;

    let public_key = PublicKey::from_protobuf_encoding(
        &payload
            .identity_key
            .ok_or(Error::NegotiationError(NegotiationError::PeerIdMissing))?,
    )?;

    Ok(PeerId::from_public_key(&public_key))
}

/// Create second Noise handshake message.
fn noise_second_message(
    noise: &mut snow::HandshakeState,
    id_keypair: &Keypair,
    keypair: &snow::Keypair,
) -> crate::Result<Vec<u8>> {
    // TODO: no unwraps
    let noise_payload = schema::noise::NoiseHandshakePayload {
        identity_key: Some(PublicKey::Ed25519(id_keypair.public()).to_protobuf_encoding()),
        identity_sig: Some(
            id_keypair.sign(&[STATIC_KEY_DOMAIN.as_bytes(), keypair.public.as_ref()].concat()),
        ),
        ..Default::default()
    };

    let mut payload = Vec::with_capacity(noise_payload.encoded_len());
    noise_payload
        .encode(&mut payload)
        .expect("Vec<u8> to provide needed capacity");

    let mut buffer = vec![0u8; 2048];

    let nwritten = noise.write_message(&payload, &mut buffer).unwrap();
    buffer.truncate(nwritten);

    let size = nwritten as u16;
    let mut size = size.to_be_bytes().to_vec();
    size.append(&mut buffer);

    let protobuf_payload = schema::webrtc::Message {
        message: Some(size),
        ..Default::default()
    };
    let mut payload = Vec::with_capacity(protobuf_payload.encoded_len());
    protobuf_payload
        .encode(&mut payload)
        .expect("Vec<u8> to provide needed capacity");

    let payload: bytes::Bytes = payload.into();

    let mut out_buf = bytes::BytesMut::with_capacity(1024);
    let mut codec = UnsignedVarint::new();
    let _result = codec.encode(payload, &mut out_buf);

    Ok(out_buf.into())
}

/// WebRTC transport.
pub(crate) struct WebRtcTransport {
    /// Transport context.
    context: TransportContext,

    /// UDP socket.
    socket: UdpSocket,

    /// DTLS certificate.
    dtls_cert: DtlsCert,

    /// Assigned listen addresss.
    listen_address: SocketAddr,

    /// RX channel for receiving commands from `Litep2p`.
    _rx: Receiver<TransportCommand>,
}

impl WebRtcTransport {
    /// Extract socket address and `PeerId`, if found, from `address`.
    fn get_socket_address(address: &Multiaddr) -> crate::Result<(SocketAddr, Option<PeerId>)> {
        tracing::trace!(target: LOG_TARGET, ?address, "parse multi address");

        let mut iter = address.iter();
        let socket_address = match iter.next() {
            Some(Protocol::Ip6(address)) => match iter.next() {
                Some(Protocol::Udp(port)) => SocketAddr::new(IpAddr::V6(address), port),
                protocol => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?protocol,
                        "invalid transport protocol, expected `Upd`",
                    );
                    return Err(Error::AddressError(AddressError::InvalidProtocol));
                }
            },
            Some(Protocol::Ip4(address)) => match iter.next() {
                Some(Protocol::Udp(port)) => SocketAddr::new(IpAddr::V4(address), port),
                protocol => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?protocol,
                        "invalid transport protocol, expected `Udp`",
                    );
                    return Err(Error::AddressError(AddressError::InvalidProtocol));
                }
            },
            protocol => {
                tracing::error!(target: LOG_TARGET, ?protocol, "invalid transport protocol");
                return Err(Error::AddressError(AddressError::InvalidProtocol));
            }
        };

        let maybe_peer = match iter.next() {
            Some(Protocol::P2p(multihash)) => Some(PeerId::from_multihash(multihash)?),
            None => None,
            protocol => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `P2p` or `None`"
                );
                return Err(Error::AddressError(AddressError::InvalidProtocol));
            }
        };

        Ok((socket_address, maybe_peer))
    }
}

#[async_trait::async_trait]
impl Transport for WebRtcTransport {
    type Config = WebRtcConfig;

    /// Create new [`Transport`] object.
    async fn new(
        context: TransportContext,
        config: Self::Config,
        _rx: Receiver<TransportCommand>,
    ) -> crate::Result<Self>
    where
        Self: Sized,
    {
        tracing::info!(
            target: LOG_TARGET,
            listen_address = ?config.listen_address,
            "start webrtc transport",
        );

        let (listen_address, _) = Self::get_socket_address(&config.listen_address)?;
        let socket = UdpSocket::bind(listen_address).await?;
        let listen_address = socket.local_addr()?;
        let dtls_cert = DtlsCert::new();

        Ok(Self {
            _rx,
            context,
            socket,
            dtls_cert,
            listen_address,
        })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr {
        let fingerprint = self.dtls_cert.fingerprint().bytes;

        const MULTIHASH_SHA256_CODE: u64 = 0x12;
        let certificate = Multihash::wrap(MULTIHASH_SHA256_CODE, &fingerprint)
            .expect("fingerprint's len to be 32 bytes");

        Multiaddr::empty()
            .with(Protocol::from(self.listen_address.ip()))
            .with(Protocol::Udp(self.listen_address.port()))
            .with(Protocol::WebRTC)
            .with(Protocol::Certhash(certificate))
            .with(Protocol::P2p(
                PeerId::from(PublicKey::Ed25519(self.context.keypair.public())).into(),
            ))
    }

    /// Start transport event loop.
    async fn start(mut self) -> crate::Result<()> {
        // let mut clients: Vec<Client> = vec![];
        let mut clients: HashMap<SocketAddr, Client> = HashMap::new();
        let mut buf = vec![0; 2000];

        loop {
            // Clean out disconnected clients
            clients.retain(|_, c| c.rtc.is_alive());

            let mut to_propagate = Vec::new();
            for (_, client) in clients.iter_mut() {
                let value = client.poll_output(&self.socket).await;
                to_propagate.push(value);
            }

            let timeouts: Vec<_> = to_propagate.iter().filter_map(|p| p.as_timeout()).collect();

            // We keep propagating client events until all clients respond with a timeout.
            if to_propagate.len() > timeouts.len() {
                propagate(&mut clients, to_propagate);
                // Start over to propagate more client data until all are timeouts.
                continue;
            }

            // Timeout in case we have no clients. We can't wait forever since we need to keep
            // polling the spawn_new_clients to discover a client.
            fn default_timeout() -> Instant {
                Instant::now() + Duration::from_millis(100)
            }

            // All poll_output resulted in timeouts, figure out the shortest timeout.
            let timeout = timeouts.into_iter().min().unwrap_or_else(default_timeout);

            // The read timeout is not allowed to be 0. In case it is 0, we set 1 millisecond.
            let duration = (timeout - Instant::now()).max(Duration::from_millis(1));

            if let Some(input) = read_socket_input(&self.socket, &mut buf, duration).await {
                match input {
                    Input::Timeout(_) => {}
                    Input::Receive(
                        _,
                        net::Receive {
                            source,
                            destination,
                            contents: DatagramRecv::Stun(message),
                        },
                    ) => {
                        if let Some((u, p)) = message.split_username() {
                            tracing::error!(target: LOG_TARGET, "Received STUN from {}:{}", u, p);

                            if !clients.contains_key(&source) {
                                let mut rtc = Rtc::builder()
                                    .set_ice_lite(true)
                                    .set_dtls_certification(self.dtls_cert.clone())
                                    .set_certificate_fingerprint_verification(false)
                                    .build();
                                rtc.add_local_candidate(Candidate::host(destination).unwrap());
                                rtc.add_remote_candidate(Candidate::host(source).unwrap());
                                rtc.direct_api().set_remote_fingerprint("sha-256 FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF".parse().unwrap());
                                rtc.direct_api().set_remote_ice_credentials(IceCreds {
                                    ufrag: u.to_owned(),
                                    pass: p.to_owned(),
                                });
                                rtc.direct_api().set_local_ice_credentials(IceCreds {
                                    ufrag: u.to_owned(),
                                    pass: p.to_owned(),
                                });
                                rtc.direct_api().set_ice_controlling(false);
                                rtc.direct_api().start_dtls(false).unwrap();
                                rtc.direct_api().start_sctp(false);

                                let noise_channel_id =
                                    rtc.direct_api().create_data_channel(ChannelConfig {
                                        label: "noise".to_string(),
                                        ordered: false,
                                        reliability: Default::default(),
                                        negotiated: Some(0),
                                        protocol: "".to_string(),
                                    });

                                clients.insert(
                                    source,
                                    Client::new(
                                        rtc,
                                        noise_channel_id,
                                        self.context.keypair.clone(),
                                    ),
                                );
                            }

                            match clients.get_mut(&source) {
                                Some(client) => {
                                    if client.rtc.accepts(&Input::Receive(
                                        Instant::now(),
                                        net::Receive {
                                            source,
                                            destination,
                                            contents: DatagramRecv::Stun(message.clone()),
                                        },
                                    )) {
                                        client
                                            .rtc
                                            .handle_input(Input::Receive(
                                                Instant::now(),
                                                net::Receive {
                                                    source,
                                                    destination,
                                                    contents: DatagramRecv::Stun(message),
                                                },
                                            ))
                                            .unwrap();
                                    }
                                }
                                None => panic!("client does not exist"),
                            }
                        }
                    }
                    other => {
                        for (_, c) in clients.iter_mut() {
                            if c.accepts(&other) {
                                c.handle_input(other);
                                break;
                            }
                        }
                    }
                }
            }

            // Drive time forward in all clients.
            let now = Instant::now();
            for (_, client) in &mut clients {
                client.handle_input(Input::Timeout(now));
            }
        }
    }
}

fn propagate(clients: &mut HashMap<SocketAddr, Client>, to_propagate: Vec<Propagated>) {
    for p in to_propagate {
        let Some(client_id) = p.client_id() else {
            // If the event doesn't have a client id, it can't be propagated,
            // (it's either a noop or a timeout).
            continue;
        };

        for (_, client) in &mut *clients {
            if client.id == client_id {
                // Do not propagate to originating client.
                continue;
            }

            match &p {
                Propagated::Noop | Propagated::Timeout(_) => {}
            }
        }
    }
}

async fn read_socket_input<'a>(
    socket: &UdpSocket,
    buf: &'a mut Vec<u8>,
    duration: Duration,
) -> Option<Input<'a>> {
    buf.resize(2000, 0);

    tokio::select! {
        result = socket.recv_from(buf) => match result {
            Ok((n, source)) => {
                buf.truncate(n);

                // Parse data to a DatagramRecv, which help preparse network data to
                // figure out the multiplexing of all protocols on one UDP port.
                let Ok(contents) = buf.as_slice().try_into() else {
                    return None;
                };

                Some(Input::Receive(
                    Instant::now(),
                    Receive {
                        source,
                        destination: socket.local_addr().unwrap(),
                        contents,
                    },
                ))
            }
            Err(error) => panic!("error: {error:?}"),
        },
        _ = tokio::time::sleep(duration) => {
            None
        }
    }
}

struct Client {
    id: ClientId,
    rtc: Rtc,
    _noise_channel_id: ChannelId,
    id_keypair: Keypair,
    state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientId(u64);

impl Deref for ClientId {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Client {
    fn new(rtc: Rtc, _noise_channel_id: ChannelId, id_keypair: Keypair) -> Client {
        static ID_COUNTER: AtomicU64 = AtomicU64::new(0);
        let next_id = ID_COUNTER.fetch_add(1, Ordering::SeqCst);

        Client {
            id: ClientId(next_id),
            _noise_channel_id,
            rtc,
            id_keypair,
            state: State::Closed,
        }
    }

    fn accepts(&self, input: &Input) -> bool {
        self.rtc.accepts(input)
    }

    fn handle_input(&mut self, input: Input) {
        if !self.rtc.is_alive() {
            return;
        }

        if let Err(error) = self.rtc.handle_input(input) {
            tracing::warn!(target: LOG_TARGET, ?error, id = ?self.id, "peer disconnected");
            self.rtc.disconnect();
        }
    }

    async fn poll_output(&mut self, socket: &UdpSocket) -> Propagated {
        if !self.rtc.is_alive() {
            return Propagated::Noop;
        }

        match self.rtc.poll_output() {
            Ok(output) => self.handle_output(output, socket).await,
            Err(e) => {
                println!("Client ({}) poll_output failed: {:?}", *self.id, e);
                self.rtc.disconnect();
                Propagated::Noop
            }
        }
    }

    async fn handle_output(&mut self, output: Output, socket: &UdpSocket) -> Propagated {
        match output {
            Output::Transmit(transmit) => {
                dbg!(&transmit);

                socket
                    .send_to(&transmit.contents, transmit.destination)
                    .await
                    .expect("sending UDP data");
                Propagated::Noop
            }
            Output::Timeout(t) => Propagated::Timeout(t),
            Output::Event(e) => match e {
                Event::IceConnectionStateChange(v) => {
                    if v == IceConnectionState::Disconnected {
                        tracing::debug!(target: LOG_TARGET, "connection closed");
                        // Ice disconnect could result in trying to establish a new connection,
                        // but this impl just disconnects directly.
                        self.rtc.disconnect();
                    }
                    Propagated::Noop
                }
                Event::ChannelOpen(cid, name) => {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    self.on_channel_open(cid, name);
                    Propagated::Noop
                }
                Event::ChannelData(data) => self.on_channel_data(data),
                Event::ChannelClose(_) => {
                    tracing::warn!("channel closed");
                    Propagated::Noop
                }
                Event::Connected => {
                    match std::mem::replace(&mut self.state, State::Poisoned) {
                        State::Closed => {
                            let remote_fingerprint = self
                                .rtc
                                .direct_api()
                                .remote_dtls_fingerprint()
                                .clone()
                                .expect("fingerprint to exist");
                            let local_fingerprint = self.rtc.direct_api().local_dtls_fingerprint();

                            let (noise, keypair) =
                                noise_prologue(local_fingerprint, remote_fingerprint);

                            self.state = State::Opened { noise, keypair };
                        }
                        _ => panic!("invalid state for connection"),
                    }
                    Propagated::Noop
                }
                event => {
                    tracing::warn!(target: LOG_TARGET, ?event, "unhandled event");
                    Propagated::Noop
                }
            },
        }
    }

    fn on_noise_channel_open(&mut self) {
        tracing::trace!(target: LOG_TARGET, "send initial noise handshake");

        let State::Opened { mut noise, keypair } = std::mem::replace(&mut self.state, State::Poisoned) else {
            panic!("invalid state for connection, expected `Opened`");
        };

        // create first noise handshake and send it to remote peer
        let payload = noise_first_message(&mut noise);

        // TODO: no unwrap
        self.rtc
            .channel(self._noise_channel_id)
            .expect("channel for noise to exist")
            .write(true, payload.as_slice())
            .unwrap();

        self.state = State::HandshakeSent { noise, keypair };
    }

    fn on_channel_open(&mut self, id: ChannelId, name: String) {
        tracing::debug!(target: LOG_TARGET, channel_id = ?id, channel_name = ?name, "channel opened");

        if id == self._noise_channel_id {
            return self.on_noise_channel_open();
        }

        panic!("support for other channels not supported");
    }

    fn on_noise_channel_data(&mut self, data: Vec<u8>) -> Propagated {
        tracing::trace!(target: LOG_TARGET, "handle noise handshake reply");

        let State::HandshakeSent { mut noise, keypair } = std::mem::replace(&mut self.state, State::Poisoned) else {
            panic!("invalid state for connection, expected `HandshakeSent`");
        };

        // TODO: no unwraps
        let remote_peer_id = read_remote_reply(data.as_slice(), &mut noise).unwrap();

        tracing::trace!(
            target: LOG_TARGET,
            ?remote_peer_id,
            "remote reply parsed successfully"
        );

        // create second noise handshake message and send it to remote
        let payload = noise_second_message(&mut noise, &self.id_keypair, &keypair).unwrap();

        let mut channel = self
            .rtc
            .channel(self._noise_channel_id)
            .expect("channel to exist");

        // TODO: no unwraps
        channel.write(true, payload.as_slice()).unwrap();

        self.state = State::Open { noise, keypair };

        Propagated::Noop
    }

    fn on_channel_data(&mut self, d: ChannelData) -> Propagated {
        if d.id == self._noise_channel_id {
            return self.on_noise_channel_data(d.data);
        }

        panic!("support for other channels not supported");
    }
}

/// Events propagated between client.
#[allow(clippy::large_enum_variant)]
enum Propagated {
    /// When we have nothing to propagate.
    Noop,

    /// Poll client has reached timeout.
    Timeout(Instant),
}

impl Propagated {
    /// Get client id, if the propagated event has a client id.
    fn client_id(&self) -> Option<ClientId> {
        None
    }

    /// If the propagated data is a timeout, returns the instant.
    fn as_timeout(&self) -> Option<Instant> {
        if let Self::Timeout(v) = self {
            Some(*v)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codec::ProtocolCodec, crypto::ed25519::Keypair, protocol::ProtocolInfo,
        types::protocol::ProtocolName,
    };
    use tokio::sync::mpsc::channel;

    #[tokio::test]
    async fn create_transport() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair = Keypair::generate();
        let (tx, _rx) = channel(64);
        let (event_tx, _event_rx) = channel(64);
        let (_command_tx, command_rx) = channel(64);

        let context = TransportContext {
            tx: event_tx,
            keypair: keypair.clone(),
            protocols: HashMap::from_iter([(
                ProtocolName::from("/notif/1"),
                ProtocolInfo {
                    tx,
                    codec: ProtocolCodec::Identity(32),
                },
            )]),
        };
        let transport_config = WebRtcConfig {
            listen_address: "/ip4/192.168.1.173/udp/8888".parse().unwrap(),
        };

        let transport = WebRtcTransport::new(context, transport_config, command_rx)
            .await
            .unwrap();

        tracing::error!("listen address: {}", transport.listen_address());

        transport.start().await.unwrap();
    }
}
