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
    config::Role,
    crypto::{
        ed25519::Keypair,
        noise::{NoiseContext, STATIC_KEY_DOMAIN},
        PublicKey,
    },
    error::{Error, NegotiationError},
    multistream_select::{listener_negotiate, Message as MultiStreamMessage},
    peer_id::PeerId,
    protocol::{Direction, ProtocolSet},
    substream::{channel::SubstreamBackend, SubstreamType},
    transport::{
        webrtc::{schema, util::WebRtcMessage, WebRtcEvent},
        TransportContext,
    },
    types::{ConnectionId, SubstreamId},
};

use bytes::BytesMut;
use multiaddr::{multihash::Multihash, Multiaddr, Protocol};
use prost::Message;
use str0m::{
    change::Fingerprint,
    channel::{ChannelData, ChannelId},
    net::Receive,
    Event, IceConnectionState, Input, Output, Rtc,
};
use tokio::{
    net::UdpSocket,
    sync::mpsc::{Receiver, Sender},
};
use tokio_util::codec::{Decoder, Encoder};

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

/// Logging target for the file.
const LOG_TARGET: &str = "webrtc::handshake";

/// Create Noise prologue.
fn noise_prologue_new(local_fingerprint: Fingerprint, remote_fingerprint: Fingerprint) -> Vec<u8> {
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

    prologue
}

/// WebRTC connection state.
enum State {
    /// Connection state is poisoned.
    Poisoned,

    /// Connection state is closed.
    Closed,

    /// Connection state is opened.
    Opened {
        /// Noise handshaker.
        handshaker: NoiseContext,
    },

    /// Handshake has been sent
    HandshakeSent {
        /// Noise handshaker.
        handshaker: NoiseContext,
    },

    /// Connection is open.
    Open {
        /// Noise handshaker.
        handshaker: NoiseContext,

        /// Protocol context.
        context: ProtocolSet,
    },
}

/// Substream context.
struct SubstreamContext {
    /// `str0m` channel id.
    channel_id: ChannelId,

    /// TX channel for sending messages to the protocol.
    tx: Sender<Vec<u8>>,
}

impl SubstreamContext {
    /// Create new [`SubstreamContext`].
    pub fn new(channel_id: ChannelId, tx: Sender<Vec<u8>>) -> Self {
        Self { channel_id, tx }
    }
}

/// WebRTC connection.
pub(super) struct WebRtcHandshake {
    /// Connection ID.
    pub(super) connection_id: ConnectionId,

    /// `str0m` WebRTC object.
    pub(super) rtc: Rtc,

    /// Noise channel ID.
    _noise_channel_id: ChannelId,

    /// Identity keypair.
    id_keypair: Keypair,

    /// Connection state.
    state: State,

    /// Transport context.
    context: TransportContext,

    /// WebRTC data channels.
    channels: HashMap<SubstreamId, SubstreamContext>,

    /// Peer address
    peer_address: SocketAddr,

    /// Local address.
    local_address: SocketAddr,

    /// Transport socket.
    socket: Arc<UdpSocket>,

    /// RX channel for receiving datagrams from the transport.
    dgram_rx: Receiver<Vec<u8>>,

    /// Substream backend.
    backend: SubstreamBackend,

    /// Next substream ID.
    substream_id: SubstreamId,

    /// ID mappings.
    id_mapping: HashMap<ChannelId, SubstreamId>,
}

impl WebRtcHandshake {
    pub(super) fn new(
        rtc: Rtc,
        connection_id: ConnectionId,
        _noise_channel_id: ChannelId,
        id_keypair: Keypair,
        context: TransportContext,
        peer_address: SocketAddr,
        local_address: SocketAddr,
        socket: Arc<UdpSocket>,
        dgram_rx: Receiver<Vec<u8>>,
    ) -> WebRtcHandshake {
        WebRtcHandshake {
            rtc,
            socket,
            dgram_rx,
            context,
            id_keypair,
            peer_address,
            local_address,
            connection_id,
            _noise_channel_id,
            state: State::Closed,
            channels: HashMap::new(),
            id_mapping: HashMap::new(),
            backend: SubstreamBackend::new(),
            substream_id: SubstreamId::new(),
        }
    }

    pub(super) async fn poll_output(&mut self) -> WebRtcEvent {
        if !self.rtc.is_alive() {
            return WebRtcEvent::Noop;
        }

        match self.rtc.poll_output() {
            Ok(output) => self.handle_output(output).await,
            Err(error) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    connection_id = ?self.connection_id,
                    ?error,
                    "`WebRtcHandshake::poll_output()` failed",
                );
                self.rtc.disconnect();
                WebRtcEvent::Noop
            }
        }
    }

    /// Handle data received from peer.
    pub(super) async fn on_input(&mut self, buffer: Vec<u8>) -> crate::Result<()> {
        let message = Input::Receive(
            Instant::now(),
            Receive {
                source: self.peer_address,
                destination: self.local_address,
                contents: buffer
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::InvalidData)?,
            },
        );

        match self.rtc.accepts(&message) {
            true => self.rtc.handle_input(message).map_err(|error| {
                tracing::debug!(target: LOG_TARGET, source = ?self.peer_address, ?error, "failed to handle data");
                Error::InputRejected
            }),
            false => return Err(Error::InputRejected),
        }
    }

    async fn handle_output(&mut self, output: Output) -> WebRtcEvent {
        match output {
            Output::Transmit(transmit) => {
                dbg!(&transmit);

                self.socket
                    .send_to(&transmit.contents, transmit.destination)
                    .await
                    .expect("send to succeed");
                WebRtcEvent::Noop
            }
            Output::Timeout(t) => WebRtcEvent::Timeout(t),
            Output::Event(e) => match e {
                Event::IceConnectionStateChange(v) => {
                    if v == IceConnectionState::Disconnected {
                        tracing::debug!(target: LOG_TARGET, "connection closed");
                        // Ice disconnect could result in trying to establish a new connection,
                        // but this impl just disconnects directly.
                        self.rtc.disconnect();
                    }
                    WebRtcEvent::Noop
                }
                Event::ChannelOpen(cid, name) => {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    self.on_channel_open(cid, name);
                    WebRtcEvent::Noop
                }
                Event::ChannelData(data) => self.on_channel_data(data).await,
                Event::ChannelClose(channel_id) => {
                    tracing::warn!("channel closed: {channel_id:?}");
                    WebRtcEvent::Noop
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

                            let handshaker = NoiseContext::with_prologue(
                                &self.id_keypair,
                                noise_prologue_new(local_fingerprint, remote_fingerprint),
                            );

                            self.state = State::Opened { handshaker };
                        }
                        _ => panic!("invalid state for connection"),
                    }
                    WebRtcEvent::Noop
                }
                event => {
                    tracing::warn!(target: LOG_TARGET, ?event, "unhandled event");
                    WebRtcEvent::Noop
                }
            },
        }
    }

    fn on_noise_channel_open(&mut self) {
        tracing::trace!(target: LOG_TARGET, "send initial noise handshake");

        let State::Opened { mut handshaker } = std::mem::replace(&mut self.state, State::Poisoned)
        else {
            panic!("invalid state for connection, expected `Opened`");
        };

        // create first noise handshake and send it to remote peer
        let payload = WebRtcMessage::encode(handshaker.first_message(Role::Dialer), None);

        // TODO: no unwrap
        self.rtc
            .channel(self._noise_channel_id)
            .expect("channel for noise to exist")
            .write(true, payload.as_slice())
            .unwrap();

        self.state = State::HandshakeSent { handshaker };
    }

    fn on_channel_open(&mut self, id: ChannelId, name: String) {
        tracing::debug!(target: LOG_TARGET, channel_id = ?id, channel_name = ?name, "channel opened");

        if id == self._noise_channel_id {
            return self.on_noise_channel_open();
        }

        // TODO: check if we can accept a channel?
    }

    async fn on_noise_channel_data(&mut self, data: Vec<u8>) -> WebRtcEvent {
        tracing::trace!(target: LOG_TARGET, "handle noise handshake reply");

        let State::HandshakeSent { mut handshaker } =
            std::mem::replace(&mut self.state, State::Poisoned)
        else {
            panic!("invalid state for connection, expected `HandshakeSent`");
        };

        let message = WebRtcMessage::decode(&data).unwrap(); // TODO: no unwraps

        // TODO: no unwraps
        let public_key = handshaker
            .get_remote_public_key(&message.payload.unwrap())
            .unwrap();
        let remote_peer_id = PeerId::from_public_key(&public_key);

        tracing::trace!(
            target: LOG_TARGET,
            ?remote_peer_id,
            "remote reply parsed successfully"
        );

        // create second noise handshake message and send it to remote
        let payload = WebRtcMessage::encode(handshaker.second_message(), None);

        let mut channel = self
            .rtc
            .channel(self._noise_channel_id)
            .expect("channel to exist");

        // TODO: no unwraps
        channel.write(true, payload.as_slice()).unwrap();

        // TODO: emit information to all protocols
        // TODO: inform protocols about the connection.
        let remote_fingerprint = self
            .rtc
            .direct_api()
            .remote_dtls_fingerprint()
            .clone()
            .expect("fingerprint to exist")
            .bytes;

        const MULTIHASH_SHA256_CODE: u64 = 0x12;
        let certificate = Multihash::wrap(MULTIHASH_SHA256_CODE, &remote_fingerprint)
            .expect("fingerprint's len to be 32 bytes");

        let address = Multiaddr::empty()
            .with(Protocol::from(self.peer_address.ip()))
            .with(Protocol::Udp(self.peer_address.port()))
            .with(Protocol::WebRTC)
            .with(Protocol::Certhash(certificate))
            .with(Protocol::P2p(PeerId::from(public_key).into()));

        let context = ProtocolSet::from_transport_context(remote_peer_id, self.context.clone())
            .await
            .unwrap();
        self.context
            .report_connection_established(remote_peer_id, address)
            .await;

        self.state = State::Open {
            handshaker,
            context,
        };

        WebRtcEvent::Noop
    }

    /// Negotiate protocol for the channel
    async fn negotiate_protocol(&mut self, d: ChannelData) -> WebRtcEvent {
        tracing::trace!(target: LOG_TARGET, channel_id = ?d.id, "negotiate protocol for the channel");

        // TODO: no unwraps
        let message = WebRtcMessage::decode(&d.data).unwrap();
        let payload = message.payload.unwrap();

        // TODO: no unwraps
        let (protocol, response) =
            listener_negotiate(&mut self.context.protocols.keys(), payload.into()).unwrap();

        let message = WebRtcMessage::encode(response, None);

        // TODO: no unwraps
        self.rtc
            .channel(d.id)
            .expect("channel to exist")
            .write(true, message.as_ref())
            .unwrap();

        let substream_id = self.substream_id.next();
        let (substream, tx) = self.backend.substream(substream_id);
        self.id_mapping.insert(d.id, substream_id);
        self.channels
            .insert(substream_id, SubstreamContext::new(d.id, tx));

        if let State::Open { context, .. } = &mut self.state {
            let _ = context
                .report_substream_open(
                    PeerId::random(),
                    protocol.clone(),
                    Direction::Inbound,
                    SubstreamType::<tokio::net::TcpStream>::ChannelBackend(substream),
                )
                .await;
        }

        return WebRtcEvent::Noop;
    }

    async fn process_multistream_select_confirmation(&mut self, d: ChannelData) -> WebRtcEvent {
        tracing::debug!(
            target: LOG_TARGET,
            channel_id = ?d.id,
            "process protocol event",
        );

        // TODO: no unwraps
        let message = WebRtcMessage::decode(&d.data).unwrap();

        tracing::warn!(target: LOG_TARGET, "MESSAGE: {message:?}");

        if let Some(message) = message.payload {
            match self.id_mapping.get(&d.id) {
                Some(id) => match self.channels.get_mut(&id) {
                    Some(context) => {
                        tracing::error!(target: LOG_TARGET, "try send data to protocol");
                        if let Err(error) = context.tx.send(message).await {
                            tracing::error!(target: LOG_TARGET, ?error, "failed to send");
                        }
                    }
                    None => panic!("channel does not exist"),
                },
                None => tracing::debug!(target: LOG_TARGET, "zzz"),
            }
        }

        WebRtcEvent::Noop
    }

    async fn on_channel_data(&mut self, d: ChannelData) -> WebRtcEvent {
        match &self.state {
            State::HandshakeSent { .. } => return self.on_noise_channel_data(d.data).await,
            State::Open { .. } => match self.id_mapping.get(&d.id) {
                Some(_) => return self.process_multistream_select_confirmation(d).await,
                None => return self.negotiate_protocol(d).await,
            },
            _ => panic!("invalid state for connection"),
        }
    }

    /// Run the event loop of a negotiated WebRTC connection.
    pub(super) async fn run(mut self) -> crate::Result<()> {
        loop {
            let duration = match self.poll_output().await {
                WebRtcEvent::Timeout(timeout) => {
                    let timeout =
                        std::cmp::min(timeout, Instant::now() + Duration::from_millis(100));
                    (timeout - Instant::now()).max(Duration::from_millis(1))
                }
                WebRtcEvent::Noop => continue,
            };

            // TODO: check if peer is still alive

            tokio::select! {
                message = self.dgram_rx.recv() => match message {
                    Some(message) => {
                        if let Err(error) = self.on_input(message).await {
                            tracing::debug!(target: LOG_TARGET, ?error, "failed to handle input");
                        }
                    }
                    None => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            source = ?self.peer_address,
                            "transport shut down, shutting down connection",
                        );
                        return Ok(());
                    }
                },
                event = self.backend.next_event() => {
                    tracing::info!(target: LOG_TARGET, "READ EVENT FROM PROTOCOL");
                    let (id, message) = event.expect("channel to stay open");
                    let channel_id = self.channels.get_mut(&id).expect("channel to exist").channel_id;

                    self.rtc
                        .channel(channel_id)
                        .expect("channel to exist")
                        .write(true, message.as_ref())
                        .unwrap();
                }
                _ = tokio::time::sleep(duration) => {}
            }

            // drive time forward in the client
            if let Err(error) = self.rtc.handle_input(Input::Timeout(Instant::now())) {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?error,
                    "failed to handle timeout for `Rtc`"
                );
                todo!();
            }
        }
    }
}