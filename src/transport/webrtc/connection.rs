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

use std::collections::HashMap;

use crate::{
    codec::unsigned_varint::UnsignedVarint,
    crypto::{ed25519::Keypair, noise::STATIC_KEY_DOMAIN, PublicKey},
    error::{Error, NegotiationError},
    multistream_select::Message as MultiStreamMessage,
    peer_id::PeerId,
    transport::{webrtc::Propagated, TransportContext},
    types::{protocol::ProtocolName, ConnectionId},
};

use bytes::BytesMut;
use multiaddr::multihash::Multihash;
use prost::Message;
use str0m::{
    change::Fingerprint,
    channel::{ChannelData, ChannelId},
    Event, IceConnectionState, Input, Output, Rtc,
};
use tokio::net::UdpSocket;
use tokio_util::codec::{Decoder, Encoder};

/// Logging target for the file.
const LOG_TARGET: &str = "webrtc::connection";

mod schema {
    pub(super) mod webrtc {
        include!(concat!(env!("OUT_DIR"), "/webrtc.rs"));
    }

    pub(super) mod noise {
        include!(concat!(env!("OUT_DIR"), "/noise.rs"));
    }
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

// TODO: no unwraps
fn new_read_remote_reply(data: &[u8]) -> crate::Result<schema::webrtc::Message> {
    let mut codec = UnsignedVarint::new();
    let mut data = bytes::BytesMut::from(data);
    let result = codec.decode(&mut data)?.ok_or(Error::Unknown)?; // TODO: bad error

    schema::webrtc::Message::decode(result).map_err(From::from)
}

/// Read remote reply and extract remote's `PeerId` from it.
fn read_remote_reply(data: &[u8], noise: &mut snow::HandshakeState) -> crate::Result<PeerId> {
    let mut codec = UnsignedVarint::new();
    let mut data = bytes::BytesMut::from(data);
    let result = codec.decode(&mut data)?.ok_or(Error::Unknown)?; // TODO: bad error

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

fn create_webrtc_message(payload: Vec<u8>) -> Vec<u8> {
    let protobuf_payload = schema::webrtc::Message {
        message: Some(payload),
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

    out_buf.into()
}

/// WebRTC connection state.
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

/// WebRTC channel state.
enum ChannelState {
    /// Confirmation sent for a proposed protocol.
    ConfirmationSent {
        /// Confirmed protocol.
        protocol: ProtocolName,
    },
}

/// WebRTC connection.
pub(super) struct WebRtcConnection {
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
    channels: HashMap<ChannelId, ChannelState>,
}

impl WebRtcConnection {
    pub(super) fn new(
        rtc: Rtc,
        connection_id: ConnectionId,
        _noise_channel_id: ChannelId,
        id_keypair: Keypair,
        context: TransportContext,
    ) -> WebRtcConnection {
        WebRtcConnection {
            connection_id,
            _noise_channel_id,
            rtc,
            id_keypair,
            state: State::Closed,
            context,
            channels: HashMap::new(),
        }
    }

    pub(super) fn accepts(&self, input: &Input) -> bool {
        self.rtc.accepts(input)
    }

    pub(super) fn handle_input(&mut self, input: Input) {
        if !self.rtc.is_alive() {
            return;
        }

        if let Err(error) = self.rtc.handle_input(input) {
            tracing::warn!(
                target: LOG_TARGET,
                ?error,
                connection_id = ?self.connection_id,
                "peer disconnected"
            );
            self.rtc.disconnect();
        }
    }

    pub(super) async fn poll_output(&mut self, socket: &UdpSocket) -> Propagated {
        if !self.rtc.is_alive() {
            return Propagated::Noop;
        }

        match self.rtc.poll_output() {
            Ok(output) => self.handle_output(output, socket).await,
            Err(e) => {
                println!(
                    "WebRtcConnection ({:?}) poll_output failed: {:?}",
                    self.connection_id, e
                );
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

        let State::Opened { mut noise, keypair } =
            std::mem::replace(&mut self.state, State::Poisoned)
        else {
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

        // TODO: check if we can accept a channel?
    }

    fn on_noise_channel_data(&mut self, data: Vec<u8>) -> Propagated {
        tracing::trace!(target: LOG_TARGET, "handle noise handshake reply");

        let State::HandshakeSent { mut noise, keypair } =
            std::mem::replace(&mut self.state, State::Poisoned)
        else {
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

        // TODO: emit information to all protocols

        self.state = State::Open { noise, keypair };

        Propagated::Noop
    }

    /// Negotiate protocol for the channel
    fn negotiate_protocol(&mut self, d: ChannelData) -> Propagated {
        tracing::error!(target: LOG_TARGET, "negotiate protocol for the channel");

        // TODO: no unwraps
        let message = new_read_remote_reply(&d.data).unwrap();
        let payload = message.message.unwrap();

        match MultiStreamMessage::decode(payload.into()) {
            Ok(MultiStreamMessage::Protocols(protocols)) => {
                tracing::debug!(target: LOG_TARGET, ?protocols, "negotiate protocol");

                for protocol in protocols {
                    for supported in self.context.protocols.keys() {
                        // TODO: this is so ugly
                        let proto = protocol.clone();
                        let proto1: &[u8] = protocol.as_ref();
                        let proto2: &[u8] = supported.as_bytes();

                        // TODO: no unwraps
                        if proto1 == proto2 {
                            let mut bytes = BytesMut::with_capacity(128);
                            let message = MultiStreamMessage::Header(
                                crate::multistream_select::HeaderLine::V1,
                            );
                            let _ = message.encode(&mut bytes).unwrap();

                            // TODO: no unwraps
                            let mut header = UnsignedVarint::encode(bytes).unwrap();

                            let mut proto_bytes = BytesMut::with_capacity(128);
                            let message = MultiStreamMessage::Protocol(proto);
                            let _ = message.encode(&mut proto_bytes).unwrap();
                            let proto_bytes = UnsignedVarint::encode(proto_bytes).unwrap();

                            header.append(&mut proto_bytes.into());

                            let message = create_webrtc_message(header.into());

                            self.rtc
                                .channel(d.id)
                                .expect("channel to exist")
                                .write(true, message.as_ref())
                                .unwrap();

                            self.channels.insert(
                                d.id,
                                ChannelState::ConfirmationSent {
                                    protocol: supported.clone(),
                                },
                            );
                            return Propagated::Noop;
                        }
                    }
                }

                // return Propagated::Noop;
                todo!("no supported protocol found");
            }
            result => todo!("unexpected result: {result:?}"),
        }
    }

    fn process_multistream_select_confirmation(&mut self, d: ChannelData) -> Propagated {
        tracing::debug!(
            target: LOG_TARGET,
            channel_id = ?d.id,
            "process protocol event",
        );

        // TODO: no unwraps
        let message = new_read_remote_reply(&d.data).unwrap();

        if let Some(_message) = message.message {}

        Propagated::Noop
    }

    fn on_channel_data(&mut self, d: ChannelData) -> Propagated {
        match &self.state {
            State::HandshakeSent { .. } => return self.on_noise_channel_data(d.data),
            State::Open { .. } => match self.channels.get_mut(&d.id) {
                Some(ChannelState::ConfirmationSent { .. }) => {
                    return self.process_multistream_select_confirmation(d)
                }
                None => return self.negotiate_protocol(d),
            },
            _ => panic!("invalid state for connection"),
        }
    }
}
