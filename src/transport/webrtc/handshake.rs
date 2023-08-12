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
fn noise_prologue_new(local_fingerprint: Vec<u8>, remote_fingerprint: Vec<u8>) -> Vec<u8> {
    const PREFIX: &[u8] = b"libp2p-webrtc-noise:";
    let mut prologue =
        Vec::with_capacity(PREFIX.len() + local_fingerprint.len() + remote_fingerprint.len());
    prologue.extend_from_slice(PREFIX);
    prologue.extend_from_slice(&remote_fingerprint);
    prologue.extend_from_slice(&local_fingerprint);

    prologue
}

/// WebRTC connection state.
#[derive(Debug)]
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

        /// Remote peer ID.
        peer: PeerId,
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

    pub(super) async fn poll_output(&mut self) -> crate::Result<WebRtcEvent> {
        match self.rtc.poll_output() {
            Ok(output) => self.handle_output(output).await,
            Err(error) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    connection_id = ?self.connection_id,
                    ?error,
                    "`WebRtcHandshake::poll_output()` failed",
                );
                return Err(Error::WebRtc(error));
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

    async fn handle_output(&mut self, output: Output) -> crate::Result<WebRtcEvent> {
        match output {
            Output::Transmit(transmit) => {
                self.socket
                    .send_to(&transmit.contents, transmit.destination)
                    .await
                    .expect("send to succeed");
                Ok(WebRtcEvent::Noop)
            }
            Output::Timeout(t) => Ok(WebRtcEvent::Timeout(t)),
            Output::Event(e) => match e {
                Event::IceConnectionStateChange(v) => {
                    if v == IceConnectionState::Disconnected {
                        tracing::debug!(target: LOG_TARGET, "ice connection closed");
                        return Err(Error::Disconnected);
                    }
                    Ok(WebRtcEvent::Noop)
                }
                Event::ChannelOpen(cid, name) => {
                    // TODO: remove
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    self.on_channel_open(cid, name).map(|_| WebRtcEvent::Noop)
                }
                Event::ChannelData(data) => self.on_channel_data(data).await,
                Event::ChannelClose(channel_id) => {
                    // TODO: notify the protocol
                    tracing::debug!(target: LOG_TARGET, ?channel_id, "channel closed");
                    Ok(WebRtcEvent::Noop)
                }
                Event::Connected => {
                    match std::mem::replace(&mut self.state, State::Poisoned) {
                        State::Closed => {
                            let remote_fingerprint = self.remote_fingerprint();
                            let local_fingerprint = self.local_fingerprint();

                            let handshaker = NoiseContext::with_prologue(
                                &self.id_keypair,
                                noise_prologue_new(local_fingerprint, remote_fingerprint),
                            );

                            self.state = State::Opened { handshaker };
                        }
                        state => {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?state,
                                "invalid state for connection"
                            );
                            return Err(Error::InvalidState);
                        }
                    }
                    Ok(WebRtcEvent::Noop)
                }
                event => {
                    tracing::warn!(target: LOG_TARGET, ?event, "unhandled event");
                    Ok(WebRtcEvent::Noop)
                }
            },
        }
    }

    /// Get remote fingerprint to bytes.
    fn remote_fingerprint(&mut self) -> Vec<u8> {
        let fingerprint = self
            .rtc
            .direct_api()
            .remote_dtls_fingerprint()
            .clone()
            .expect("fingerprint to exist");
        Self::fingerprint_to_bytes(&fingerprint)
    }

    /// Get local fingerprint as bytes.
    fn local_fingerprint(&mut self) -> Vec<u8> {
        Self::fingerprint_to_bytes(&self.rtc.direct_api().local_dtls_fingerprint())
    }

    /// Convert `Fingerprint` to bytes.
    fn fingerprint_to_bytes(fingerprint: &Fingerprint) -> Vec<u8> {
        const MULTIHASH_SHA256_CODE: u64 = 0x12;
        Multihash::wrap(MULTIHASH_SHA256_CODE, &fingerprint.bytes)
            .expect("fingerprint's len to be 32 bytes")
            .to_bytes()
    }

    fn on_noise_channel_open(&mut self) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, "send initial noise handshake");

        let State::Opened { mut handshaker } = std::mem::replace(&mut self.state, State::Poisoned)
        else {
            return Err(Error::InvalidState);
        };

        // create first noise handshake and send it to remote peer
        let payload = WebRtcMessage::encode(handshaker.first_message(Role::Dialer), None);

        self.rtc
            .channel(self._noise_channel_id)
            .ok_or(Error::ChannelDoesntExist)?
            .write(true, payload.as_slice())
            .map_err(|error| Error::WebRtc(error))?;

        self.state = State::HandshakeSent { handshaker };
        Ok(())
    }

    fn on_channel_open(&mut self, id: ChannelId, name: String) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, channel_id = ?id, channel_name = ?name, "channel opened");

        if id == self._noise_channel_id {
            return self.on_noise_channel_open();
        }

        Ok(())
    }

    async fn on_noise_channel_data(&mut self, data: Vec<u8>) -> crate::Result<WebRtcEvent> {
        tracing::trace!(target: LOG_TARGET, "handle noise handshake reply");

        let State::HandshakeSent { mut handshaker } =
            std::mem::replace(&mut self.state, State::Poisoned)
        else {
            return Err(Error::InvalidState);
        };

        let message = WebRtcMessage::decode(&data)?
            .payload
            .ok_or(Error::InvalidData)?;
        let public_key = handshaker.get_remote_public_key(&message)?;
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
            .ok_or(Error::ChannelDoesntExist)?;

        channel
            .write(true, payload.as_slice())
            .map_err(|error| Error::WebRtc(error))?;

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

        let context =
            ProtocolSet::from_transport_context(remote_peer_id, self.context.clone()).await?;
        self.context
            .report_connection_established(remote_peer_id, address)
            .await;

        self.state = State::Open {
            handshaker,
            context,
            peer: remote_peer_id,
        };

        Ok(WebRtcEvent::Noop)
    }

    /// Negotiate protocol for the channel
    async fn negotiate_protocol(&mut self, d: ChannelData) -> crate::Result<WebRtcEvent> {
        tracing::trace!(target: LOG_TARGET, channel_id = ?d.id, "negotiate protocol for the channel");

        let payload = WebRtcMessage::decode(&d.data)?
            .payload
            .ok_or(Error::InvalidData)?;

        let (protocol, response) =
            listener_negotiate(&mut self.context.protocols.keys(), payload.into())?;

        let message = WebRtcMessage::encode(response, None);

        self.rtc
            .channel(d.id)
            .ok_or(Error::ChannelDoesntExist)?
            .write(true, message.as_ref())
            .map_err(|error| Error::WebRtc(error))?;

        let substream_id = self.substream_id.next();
        let (substream, tx) = self.backend.substream(substream_id);
        self.id_mapping.insert(d.id, substream_id);
        self.channels
            .insert(substream_id, SubstreamContext::new(d.id, tx));

        if let State::Open { context, peer, .. } = &mut self.state {
            let _ = context
                .report_substream_open(
                    *peer,
                    protocol.clone(),
                    Direction::Inbound,
                    // TODO: this is wrong
                    SubstreamType::<tokio::net::TcpStream>::ChannelBackend(substream),
                )
                .await;
        }

        Ok(WebRtcEvent::Noop)
    }

    async fn process_multistream_select_confirmation(
        &mut self,
        d: ChannelData,
    ) -> crate::Result<WebRtcEvent> {
        tracing::debug!(
            target: LOG_TARGET,
            channel_id = ?d.id,
            "process protocol event",
        );

        // TODO: might be empty message with flags
        let message = WebRtcMessage::decode(&d.data)?
            .payload
            .ok_or(Error::InvalidData)?;

        match self.id_mapping.get(&d.id) {
            Some(id) => match self.channels.get_mut(&id) {
                Some(context) => {
                    let _ = context.tx.send(message).await;
                    Ok(WebRtcEvent::Noop)
                }
                None => {
                    tracing::error!(target: LOG_TARGET, "channel doesn't exist 1");
                    return Err(Error::ChannelDoesntExist);
                }
            },
            None => {
                tracing::error!(target: LOG_TARGET, "channel doesn't exist 2");
                return Err(Error::ChannelDoesntExist);
            }
        }
    }

    async fn on_channel_data(&mut self, d: ChannelData) -> crate::Result<WebRtcEvent> {
        match &self.state {
            State::HandshakeSent { .. } => self.on_noise_channel_data(d.data).await,
            State::Open { .. } => match self.id_mapping.get(&d.id) {
                Some(_) => self.process_multistream_select_confirmation(d).await,
                None => self.negotiate_protocol(d).await,
            },
            _ => Err(Error::InvalidState),
        }
    }

    /// Run the event loop of a negotiated WebRTC connection.
    pub(super) async fn run(mut self) -> crate::Result<()> {
        loop {
            if !self.rtc.is_alive() {
                tracing::debug!(
                    target: LOG_TARGET,
                    "`Rtc` is not alive, closing `WebRtcHandshake`"
                );
                return Ok(());
            }

            let duration = match self.poll_output().await {
                Ok(WebRtcEvent::Timeout(timeout)) => {
                    let timeout =
                        std::cmp::min(timeout, Instant::now() + Duration::from_millis(100));
                    (timeout - Instant::now()).max(Duration::from_millis(1))
                }
                Ok(WebRtcEvent::Noop) => continue,
                Err(error) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?error,
                        "error occurred, closing connection"
                    );
                    self.rtc.disconnect();
                    return Ok(());
                }
            };

            tokio::select! {
                message = self.dgram_rx.recv() => match message {
                    Some(message) => match self.on_input(message).await {
                        Ok(_) | Err(Error::InputRejected) => {},
                        Err(error) => {
                            tracing::debug!(target: LOG_TARGET, ?error, "failed to handle input");
                            return Err(error)
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
                    let (id, message) = event.ok_or(Error::EssentialTaskClosed)?;
                    let channel_id = self.channels.get_mut(&id).ok_or(Error::ChannelDoesntExist)?.channel_id;

                    tracing::trace!(target: LOG_TARGET, ?id, ?message, "send message to remote peer");

                    self.rtc
                        .channel(channel_id)
                        .ok_or(Error::ChannelDoesntExist)?
                        .write(true, message.as_ref())
                        .map_err(|error| Error::WebRtc(error))?;
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

                self.rtc.disconnect();
                return Err(Error::Disconnected);
            }
        }
    }
}
