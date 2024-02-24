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

#![allow(unused)]

use crate::{
    config::Role,
    crypto::{ed25519::Keypair, noise::NoiseContext},
    error::Error,
    multistream_select::{listener_negotiate, DialerState, HandshakeResult, ListenerSelectResult},
    protocol::{Direction, Permit, ProtocolCommand, ProtocolSet},
    substream::Substream,
    transport::{
        webrtc::util::{SubstreamContext, WebRtcMessage},
        Endpoint,
    },
    types::{protocol::ProtocolName, ConnectionId, SubstreamId},
    PeerId,
};

use futures::StreamExt;
use multiaddr::{multihash::Multihash, Multiaddr, Protocol};
use str0m::{
    change::Fingerprint,
    channel::{ChannelConfig, ChannelData, ChannelId},
    net::Receive,
    Event, IceConnectionState, Input, Output, Rtc,
};
use tokio::{net::UdpSocket, sync::mpsc::Receiver};

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::webrtc::connection";

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
        /// Remote peer ID.
        peer: PeerId,
    },
}

/// Substream state.
#[derive(Debug)]
enum SubstreamState {
    /// Substream state is poisoned.
    Poisoned,

    /// Substream (outbound) is opening.
    Opening {
        /// Protocol.
        protocol: ProtocolName,

        /// Negotiated fallback.
        fallback: Option<ProtocolName>,

        /// `multistream-select` dialer state.
        dialer_state: DialerState,

        /// Substream ID,
        substream_id: SubstreamId,

        /// Connection permit.
        permit: Permit,
    },

    /// Substream is open.
    Open {
        /// Substream ID.
        substream_id: SubstreamId,

        /// Substream.
        substream: SubstreamContext,

        /// Connection permit.
        permit: Permit,
    },
}

/// WebRTC connection.
pub struct WebRtcConnection {
    /// `str0m` WebRTC object.
    rtc: Rtc,

    /// Identity keypair.
    id_keypair: Keypair,

    /// Protocol set.
    protocol_set: ProtocolSet,

    /// Remote peer ID.
    peer: PeerId,

    /// Endpoint.
    endpoint: Endpoint,

    /// Peer address
    peer_address: SocketAddr,

    /// Local address.
    local_address: SocketAddr,

    /// Transport socket.
    socket: Arc<UdpSocket>,

    /// RX channel for receiving datagrams from the transport.
    dgram_rx: Receiver<Vec<u8>>,
}

impl WebRtcConnection {
    pub fn new(
        rtc: Rtc,
        id_keypair: Keypair,
        peer: PeerId,
        peer_address: SocketAddr,
        local_address: SocketAddr,
        socket: Arc<UdpSocket>,
        protocol_set: ProtocolSet,
        endpoint: Endpoint,
        dgram_rx: Receiver<Vec<u8>>,
    ) -> Self {
        Self {
            rtc,
            id_keypair,
            protocol_set,
            peer,
            peer_address,
            local_address,
            socket,
            endpoint,
            dgram_rx,
        }
    }

    /// Negotiate protocol for the channel
    fn listener_negotiate_protocol(&mut self, d: ChannelData) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            channel_id = ?d.id,
            "negotiate protocol for the channel",
        );

        let payload = WebRtcMessage::decode(&d.data)?.payload.ok_or(Error::InvalidData)?;
        let response =
            match listener_negotiate(&mut self.protocol_set.protocols().iter(), payload.into())? {
                ListenerSelectResult::Accepted { protocol, message } => {
                    tracing::error!(target: LOG_TARGET, "negotiated {protocol:?}");
                    message
                }
                ListenerSelectResult::Rejected { message } => message,
            };

        let message = WebRtcMessage::encode(response.to_vec());

        self.rtc
            .channel(d.id)
            .ok_or(Error::ChannelDoesntExist)?
            .write(true, message.as_ref())
            .map_err(|error| Error::WebRtc(error))?;

        // TODO report substream to protocol

        Ok(())
    }

    /// Start running event loop of [`WebRtcConnection`].
    pub async fn run(mut self) {
        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.peer,
            "start webrtc connection event loop",
        );

        let _ = self
            .protocol_set
            .report_connection_established(self.peer, self.endpoint.clone())
            .await;

        loop {
            // Poll output until we get a timeout. The timeout means we
            // are either awaiting UDP socket input or the timeout to happen.
            let timeout = match self.rtc.poll_output().unwrap() {
                Output::Timeout(v) => v,
                Output::Transmit(v) => {
                    self.socket.try_send_to(&v.contents, v.destination).unwrap();
                    continue;
                }
                Output::Event(v) => match v {
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => return,
                    Event::ChannelOpen(cid, name) => {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?cid,
                            ?name,
                            "channel opened",
                        );
                        continue;
                    }
                    Event::ChannelClose(cid) => {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?cid,
                            "channel closed",
                        );
                        continue;
                    }
                    Event::ChannelData(info) => {
                        tracing::info!(
                            target: LOG_TARGET,
                            cid = ?info.id,
                            "data received",
                        );
                        self.listener_negotiate_protocol(info);
                        continue;
                    }
                    event => {
                        tracing::error!(
                            target: LOG_TARGET,
                            ?event,
                            "unhandled event",
                        );
                        continue;
                    }
                },
            };

            let duration = timeout - Instant::now();
            if duration.is_zero() {
                self.rtc.handle_input(Input::Timeout(Instant::now())).unwrap();
                continue;
            }

            tokio::select! {
                datagram = self.dgram_rx.recv() => match datagram {
                    Some(datagram) => {
                        let input = Input::Receive(
                            Instant::now(),
                            Receive {
                                source: self.peer_address,
                                destination: self.local_address,
                                contents: datagram.as_slice().try_into().unwrap(),
                            },
                        );

                        self.rtc.handle_input(input).unwrap();
                    }
                    None => return,
                },
                _ = tokio::time::sleep(duration) => {
                    self.rtc.handle_input(Input::Timeout(Instant::now())).unwrap();
                }
            }
        }
    }
}

// /// WebRTC connection.
// // TODO: too much stuff, refactor?
// pub(super) struct WebRtcConnection {
//     /// Connection ID.
//     pub(super) connection_id: ConnectionId,

//     /// `str0m` WebRTC object.
//     pub(super) rtc: Rtc,

//     /// Noise channel ID.
//     _noise_channel_id: ChannelId,

//     /// Identity keypair.
//     id_keypair: Keypair,

//     /// Connection state.
//     state: State,

//     /// Protocol set.
//     protocol_set: ProtocolSet,

//     /// Peer address
//     peer_address: SocketAddr,

//     /// Local address.
//     local_address: SocketAddr,

//     /// Transport socket.
//     socket: Arc<UdpSocket>,

//     /// RX channel for receiving datagrams from the transport.
//     dgram_rx: Receiver<Vec<u8>>,

//     /// Substream backend.
//     backend: SubstreamBackend,

//     /// Next substream ID.
//     substream_id: SubstreamId,

//     /// Pending outbound substreams.
//     pending_outbound: HashMap<ChannelId, (ProtocolName, Vec<ProtocolName>, SubstreamId, Permit)>,

//     /// Open substreams.
//     substreams: HashMap<ChannelId, SubstreamState>,
// }

// impl WebRtcConnection {
//     pub(super) fn new(
//         rtc: Rtc,
//         connection_id: ConnectionId,
//         _noise_channel_id: ChannelId,
//         id_keypair: Keypair,
//         protocol_set: ProtocolSet,
//         peer_address: SocketAddr,
//         local_address: SocketAddr,
//         socket: Arc<UdpSocket>,
//         dgram_rx: Receiver<Vec<u8>>,
//     ) -> WebRtcConnection {
//         WebRtcConnection {
//             rtc,
//             socket,
//             dgram_rx,
//             protocol_set,
//             id_keypair,
//             peer_address,
//             local_address,
//             connection_id,
//             _noise_channel_id,
//             state: State::Closed,
//             substreams: HashMap::new(),
//             backend: SubstreamBackend::new(),
//             substream_id: SubstreamId::new(),
//             pending_outbound: HashMap::new(),
//         }
//     }

//     pub(super) async fn poll_output(&mut self) -> crate::Result<WebRtcEvent> {
//         match self.rtc.poll_output() {
//             Ok(output) => self.handle_output(output).await,
//             Err(error) => {
//                 tracing::debug!(
//                     target: LOG_TARGET,
//                     connection_id = ?self.connection_id,
//                     ?error,
//                     "`WebRtcConnection::poll_output()` failed",
//                 );
//                 return Err(Error::WebRtc(error));
//             }
//         }
//     }

//     /// Handle data received from peer.
//     pub(super) async fn on_input(&mut self, buffer: Vec<u8>) -> crate::Result<()> {
//         let message = Input::Receive(
//             Instant::now(),
//             Receive {
//                 source: self.peer_address,
//                 destination: self.local_address,
//                 contents: buffer.as_slice().try_into().map_err(|_| Error::InvalidData)?,
//             },
//         );

//         match self.rtc.accepts(&message) {
//             true => self.rtc.handle_input(message).map_err(|error| {
//                 tracing::debug!(target: LOG_TARGET, source = ?self.peer_address, ?error, "failed
// to handle data");                 Error::InputRejected
//             }),
//             false => return Err(Error::InputRejected),
//         }
//     }

//     async fn handle_output(&mut self, output: Output) -> crate::Result<WebRtcEvent> {
//         match output {
//             Output::Transmit(transmit) => {
//                 self.socket
//                     .send_to(&transmit.contents, transmit.destination)
//                     .await
//                     .expect("send to succeed");
//                 Ok(WebRtcEvent::Noop)
//             }
//             Output::Timeout(t) => Ok(WebRtcEvent::Timeout(t)),
//             Output::Event(e) => match e {
//                 Event::IceConnectionStateChange(v) => {
//                     if v == IceConnectionState::Disconnected {
//                         tracing::debug!(target: LOG_TARGET, "ice connection closed");
//                         return Err(Error::Disconnected);
//                     }
//                     Ok(WebRtcEvent::Noop)
//                 }
//                 Event::ChannelOpen(cid, name) => {
//                     // TODO: remove, report issue to smoldot
//                     tokio::time::sleep(std::time::Duration::from_millis(500)).await;
//                     self.on_channel_open(cid, name).map(|_| WebRtcEvent::Noop)
//                 }
//                 Event::ChannelData(data) => self.on_channel_data(data).await,
//                 Event::ChannelClose(channel_id) => {
//                     // TODO: notify the protocol
//                     tracing::debug!(target: LOG_TARGET, ?channel_id, "channel closed");
//                     Ok(WebRtcEvent::Noop)
//                 }
//                 Event::Connected => {
//                     match std::mem::replace(&mut self.state, State::Poisoned) {
//                         State::Closed => {
//                             let remote_fingerprint = self.remote_fingerprint();
//                             let local_fingerprint = self.local_fingerprint();

//                             let handshaker = NoiseContext::with_prologue(
//                                 &self.id_keypair,
//                                 noise_prologue_new(local_fingerprint, remote_fingerprint),
//                             );

//                             self.state = State::Opened { handshaker };
//                         }
//                         state => {
//                             tracing::debug!(
//                                 target: LOG_TARGET,
//                                 ?state,
//                                 "invalid state for connection"
//                             );
//                             return Err(Error::InvalidState);
//                         }
//                     }
//                     Ok(WebRtcEvent::Noop)
//                 }
//                 event => {
//                     tracing::warn!(target: LOG_TARGET, ?event, "unhandled event");
//                     Ok(WebRtcEvent::Noop)
//                 }
//             },
//         }
//     }

//     /// Get remote fingerprint to bytes.
//     fn remote_fingerprint(&mut self) -> Vec<u8> {
//         let fingerprint = self
//             .rtc
//             .direct_api()
//             .remote_dtls_fingerprint()
//             .clone()
//             .expect("fingerprint to exist");
//         Self::fingerprint_to_bytes(&fingerprint)
//     }

//     /// Get local fingerprint as bytes.
//     fn local_fingerprint(&mut self) -> Vec<u8> {
//         Self::fingerprint_to_bytes(&self.rtc.direct_api().local_dtls_fingerprint())
//     }

//     /// Convert `Fingerprint` to bytes.
//     fn fingerprint_to_bytes(fingerprint: &Fingerprint) -> Vec<u8> {
//         const MULTIHASH_SHA256_CODE: u64 = 0x12;
//         Multihash::wrap(MULTIHASH_SHA256_CODE, &fingerprint.bytes)
//             .expect("fingerprint's len to be 32 bytes")
//             .to_bytes()
//     }

//     fn on_noise_channel_open(&mut self) -> crate::Result<()> {
//         tracing::trace!(target: LOG_TARGET, "send initial noise handshake");

//         let State::Opened { mut handshaker } = std::mem::replace(&mut self.state,
// State::Poisoned)         else {
//             return Err(Error::InvalidState);
//         };

//         // create first noise handshake and send it to remote peer
//         let payload = WebRtcMessage::encode(handshaker.first_message(Role::Dialer));

//         self.rtc
//             .channel(self._noise_channel_id)
//             .ok_or(Error::ChannelDoesntExist)?
//             .write(true, payload.as_slice())
//             .map_err(|error| Error::WebRtc(error))?;

//         self.state = State::HandshakeSent { handshaker };
//         Ok(())
//     }

//     fn on_channel_open(&mut self, channel_id: ChannelId, name: String) -> crate::Result<()> {
//         tracing::debug!(target: LOG_TARGET, ?channel_id, channel_name = ?name, "channel opened");

//         if channel_id == self._noise_channel_id {
//             return self.on_noise_channel_open();
//         }

//         match self.pending_outbound.remove(&channel_id) {
//             None => {
//                 tracing::trace!(target: LOG_TARGET, ?channel_id, "remote opened a substream");
//             }
//             Some((protocol, fallback_names, substream_id, permit)) => {
//                 tracing::trace!(target: LOG_TARGET, ?channel_id, "dialer negotiate protocol");

//                 let (dialer_state, message) =
//                     DialerState::propose(protocol.clone(), fallback_names)?;
//                 let message = WebRtcMessage::encode(message);

//                 self.rtc
//                     .channel(channel_id)
//                     .ok_or(Error::ChannelDoesntExist)?
//                     .write(true, message.as_ref())
//                     .map_err(|error| Error::WebRtc(error))?;

//                 self.substreams.insert(
//                     channel_id,
//                     SubstreamState::Opening {
//                         protocol,
//                         fallback: None,
//                         substream_id,
//                         dialer_state,
//                         permit,
//                     },
//                 );
//             }
//         }

//         Ok(())
//     }

//     async fn on_noise_channel_data(&mut self, data: Vec<u8>) -> crate::Result<WebRtcEvent> {
//         tracing::trace!(target: LOG_TARGET, "handle noise handshake reply");

//         let State::HandshakeSent { mut handshaker } =
//             std::mem::replace(&mut self.state, State::Poisoned)
//         else {
//             return Err(Error::InvalidState);
//         };

//         let message = WebRtcMessage::decode(&data)?.payload.ok_or(Error::InvalidData)?;
//         let public_key = handshaker.get_remote_public_key(&message)?;
//         let remote_peer_id = PeerId::from_public_key(&public_key);

//         tracing::trace!(
//             target: LOG_TARGET,
//             ?remote_peer_id,
//             "remote reply parsed successfully"
//         );

//         // create second noise handshake message and send it to remote
//         let payload = WebRtcMessage::encode(handshaker.second_message());

//         let mut channel =
//             self.rtc.channel(self._noise_channel_id).ok_or(Error::ChannelDoesntExist)?;

//         channel.write(true, payload.as_slice()).map_err(|error| Error::WebRtc(error))?;

//         let remote_fingerprint = self
//             .rtc
//             .direct_api()
//             .remote_dtls_fingerprint()
//             .clone()
//             .expect("fingerprint to exist")
//             .bytes;

//         const MULTIHASH_SHA256_CODE: u64 = 0x12;
//         let certificate = Multihash::wrap(MULTIHASH_SHA256_CODE, &remote_fingerprint)
//             .expect("fingerprint's len to be 32 bytes");

//         let address = Multiaddr::empty()
//             .with(Protocol::from(self.peer_address.ip()))
//             .with(Protocol::Udp(self.peer_address.port()))
//             .with(Protocol::WebRTC)
//             .with(Protocol::Certhash(certificate))
//             .with(Protocol::P2p(PeerId::from(public_key).into()));

//         self.protocol_set
//             .report_connection_established(
//                 remote_peer_id,
//                 Endpoint::listener(address, self.connection_id),
//             )
//             .await?;

//         self.state = State::Open {
//             peer: remote_peer_id,
//         };

//         Ok(WebRtcEvent::Noop)
//     }

//     /// Report open substream to the protocol.
//     async fn report_open_substream(
//         &mut self,
//         channel_id: ChannelId,
//         protocol: ProtocolName,
//     ) -> crate::Result<WebRtcEvent> {
//         // let substream_id = self.substream_id.next();
//         // let (mut substream, tx) = self.backend.substream(channel_id);
//         // let substream: Box<dyn SubstreamT> = {
//         //     substream.apply_codec(self.protocol_set.protocol_codec(&protocol));
//         //     Box::new(substream)
//         // };
//         // let permit = self.protocol_set.try_get_permit().ok_or(Error::ConnectionClosed)?;

//         // self.substreams.insert(
//         //     channel_id,
//         //     SubstreamState::Open {
//         //         substream_id,
//         //         substream: SubstreamContext::new(channel_id, tx),
//         //         permit,
//         //     },
//         // );
//         // TODO: fix

//         if let State::Open { peer, .. } = &mut self.state {
//             // let _ = self
//             //     .protocol_set
//             //     .report_substream_open(*peer, protocol.clone(), Direction::Inbound, substream)
//             //     .await;
//             todo!();
//         }

//         Ok(WebRtcEvent::Noop)
//     }

//     /// Negotiate protocol for the channel
//     async fn listener_negotiate_protocol(&mut self, d: ChannelData) -> crate::Result<WebRtcEvent>
// {         tracing::trace!(target: LOG_TARGET, channel_id = ?d.id, "negotiate protocol for the
// channel");

//         let payload = WebRtcMessage::decode(&d.data)?.payload.ok_or(Error::InvalidData)?;

//         let (protocol, response) =
//             listener_negotiate(&mut self.protocol_set.protocols().iter(), payload.into())?;

//         let message = WebRtcMessage::encode(response.to_vec());

//         self.rtc
//             .channel(d.id)
//             .ok_or(Error::ChannelDoesntExist)?
//             .write(true, message.as_ref())
//             .map_err(|error| Error::WebRtc(error))?;

//         self.report_open_substream(d.id, protocol).await

//         // let substream_id = self.substream_id.next();
//         // let (mut substream, tx) = self.backend.substream(d.id);
//         // let substream: Box<dyn SubstreamT> = {
//         //     substream.apply_codec(self.protocol_set.protocol_codec(&protocol));
//         //     Box::new(substream)
//         // };
//         // let permit = self.protocol_set.try_get_permit().ok_or(Error::ConnectionClosed)?;

//         // self.substreams.insert(
//         //     d.id,
//         //     SubstreamState::Open {
//         //         substream_id,
//         //         substream: SubstreamContext::new(d.id, tx),
//         //         permit,
//         //     },
//         // );

//         // if let State::Open { peer, .. } = &mut self.state {
//         //     let _ = self
//         //         .protocol_set
//         //         .report_substream_open(*peer, protocol.clone(), Direction::Inbound, substream)
//         //         .await;
//         // }
//         // Ok(WebRtcEvent::Noop)
//     }

//     async fn on_channel_data(&mut self, d: ChannelData) -> crate::Result<WebRtcEvent> {
//         match &self.state {
//             State::HandshakeSent { .. } => self.on_noise_channel_data(d.data).await,
//             State::Open { .. } => {
//                 match self.substreams.get_mut(&d.id) {
//                     None => match self.listener_negotiate_protocol(d).await {
//                         Ok(_) => {
//                             tracing::debug!(target: LOG_TARGET, "protocol negotiated for the
// channel");

//                             Ok(WebRtcEvent::Noop)
//                         }
//                         Err(error) => {
//                             tracing::debug!(target: LOG_TARGET, ?error, "failed to negotiate
// protocol");

//                             // TODO: close channel
//                             Ok(WebRtcEvent::Noop)
//                         }
//                     },
//                     Some(SubstreamState::Poisoned) => return Err(Error::ConnectionClosed),
//                     Some(SubstreamState::Opening {
//                         ref mut dialer_state,
//                         ..
//                     }) => {
//                         tracing::info!(target: LOG_TARGET, "try to decode message");
//                         let message =
//                             WebRtcMessage::decode(&d.data)?.payload.ok_or(Error::InvalidData)?;
//                         tracing::info!(target: LOG_TARGET, "decoded successfully");

//                         match dialer_state.register_response(message) {
//                             Ok(HandshakeResult::NotReady) => {}
//                             Ok(HandshakeResult::Succeeded(protocol)) => {
//                                 tracing::warn!(target: LOG_TARGET, ?protocol, "protocol
// negotiated, inform protocol handler");

//                                 return self.report_open_substream(d.id, protocol).await;
//                             }
//                             Err(error) => {
//                                 tracing::error!(target: LOG_TARGET, ?error, "failed to negotiate
// protocol");                                 // TODO: close channel
//                             }
//                         }

//                         Ok(WebRtcEvent::Noop)
//                     }
//                     Some(SubstreamState::Open { substream, .. }) => {
//                         // TODO: might be empty message with flags
//                         // TODO: if decoding fails, close the substream
//                         let message =
//                             WebRtcMessage::decode(&d.data)?.payload.ok_or(Error::InvalidData)?;
//                         let _ = substream.tx.send(message).await;

//                         Ok(WebRtcEvent::Noop)
//                     }
//                 }
//             }
//             _ => Err(Error::InvalidState),
//         }
//     }

//     /// Open outbound substream.
//     fn open_substream(
//         &mut self,
//         protocol: ProtocolName,
//         fallback_names: Vec<ProtocolName>,
//         substream_id: SubstreamId,
//         permit: Permit,
//     ) {
//         let channel_id = self.rtc.direct_api().create_data_channel(ChannelConfig {
//             label: protocol.to_string(),
//             ordered: false,
//             reliability: Default::default(),
//             negotiated: None,
//             protocol: protocol.to_string(),
//         });

//         tracing::trace!(
//             target: LOG_TARGET,
//             ?channel_id,
//             ?substream_id,
//             ?protocol,
//             ?fallback_names,
//             "open data channel"
//         );

//         self.pending_outbound
//             .insert(channel_id, (protocol, fallback_names, substream_id, permit));
//     }

//     /// Run the event loop of a negotiated WebRTC connection.
//     pub(super) async fn run(mut self) -> crate::Result<()> {
//         loop {
//             if !self.rtc.is_alive() {
//                 tracing::debug!(
//                     target: LOG_TARGET,
//                     "`Rtc` is not alive, closing `WebRtcConnection`"
//                 );
//                 return Ok(());
//             }

//             let duration = match self.poll_output().await {
//                 Ok(WebRtcEvent::Timeout(timeout)) => {
//                     let timeout =
//                         std::cmp::min(timeout, Instant::now() + Duration::from_millis(100));
//                     (timeout - Instant::now()).max(Duration::from_millis(1))
//                 }
//                 Ok(WebRtcEvent::Noop) => continue,
//                 Err(error) => {
//                     tracing::debug!(
//                         target: LOG_TARGET,
//                         ?error,
//                         "error occurred, closing connection"
//                     );
//                     self.rtc.disconnect();
//                     return Ok(());
//                 }
//             };

//             tokio::select! {
//                 message = self.dgram_rx.recv() => match message {
//                     Some(message) => match self.on_input(message).await {
//                         Ok(_) | Err(Error::InputRejected) => {},
//                         Err(error) => {
//                             tracing::debug!(target: LOG_TARGET, ?error, "failed to handle
// input");                             return Err(error)
//                         }
//                     }
//                     None => {
//                         tracing::debug!(
//                             target: LOG_TARGET,
//                             source = ?self.peer_address,
//                             "transport shut down, shutting down connection",
//                         );
//                         return Ok(());
//                     }
//                 },
//                 event = self.backend.next_event() => {
//                     let (channel_id, message) = event.ok_or(Error::EssentialTaskClosed)?;

//                     match self.substreams.get_mut(&channel_id) {
//                         None => {
//                             tracing::debug!(target: LOG_TARGET, "protocol tried to send message
// over substream that doesn't exist");                         }
//                         Some(SubstreamState::Poisoned) => {},
//                         Some(SubstreamState::Opening { .. }) => {
//                             tracing::debug!(target: LOG_TARGET, "protocol tried to send message
// over substream that isn't open");                         }
//                         Some(SubstreamState::Open { .. }) => {
//                             tracing::trace!(target: LOG_TARGET, ?channel_id, ?message, "send
// message to remote peer");

//                             self.rtc
//                                 .channel(channel_id)
//                                 .ok_or(Error::ChannelDoesntExist)?
//                                 .write(true, message.as_ref())
//                                 .map_err(|error| Error::WebRtc(error))?;
//                         }
//                     }
//                 }
//                 event = self.protocol_set.next() => match event {
//                     Some(event) => match event {
//                         ProtocolCommand::OpenSubstream { protocol, fallback_names, substream_id,
// permit } => {                             self.open_substream(protocol, fallback_names,
// substream_id, permit);                         }
//                         ProtocolCommand::ForceClose => {
//                             tracing::debug!(target: LOG_TARGET, "force closing connection");
//                             return Ok(());
//                         }
//                     }
//                     None => {
//                         tracing::debug!(target: LOG_TARGET, "handle to protocol closed, closing
// connection");                         return Ok(());
//                     }
//                 },
//                 _ = tokio::time::sleep(duration) => {}
//             }

//             // drive time forward in the client
//             if let Err(error) = self.rtc.handle_input(Input::Timeout(Instant::now())) {
//                 tracing::debug!(
//                     target: LOG_TARGET,
//                     ?error,
//                     "failed to handle timeout for `Rtc`"
//                 );

//                 self.rtc.disconnect();
//                 return Err(Error::Disconnected);
//             }
//         }
//     }
// }
