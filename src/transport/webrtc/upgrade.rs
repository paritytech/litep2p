// Copyright 2022 Parity Technologies (UK) Ltd.
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
    config::Role,
    crypto::ed25519::Keypair,
    peer_id::PeerId,
    transport::webrtc::{error::Error, fingerprint::Fingerprint, sdp, util::WebRtcMessage},
};

use futures::channel::oneshot::{self, Sender};
use futures::future::Either;
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data::data_channel::DataChannel;
use webrtc::data::data_channel::DataChannel as DetachedDataChannel;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::RTCDataChannel;
use webrtc::dtls_transport::dtls_role::DTLSRole;
use webrtc::ice::network_type::NetworkType;
use webrtc::ice::udp_mux::UDPMux;
use webrtc::ice::udp_network::UDPNetwork;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::RTCPeerConnection;

use std::{net::SocketAddr, sync::Arc, time::Duration};

// use crate::tokio::{error::Error, fingerprint::Fingerprint, sdp, substream::Substream, Connection};

use bytes::Bytes;

/// Represents the state of closing one half (either read or write) of the connection.
///
/// Gracefully closing the read or write requires sending the `STOP_SENDING` or `FIN` flag respectively
/// and flushing the underlying connection.
#[derive(Debug, Copy, Clone)]
pub(crate) enum Closing {
    Requested,
    MessageSent,
}

#[derive(Debug, Copy, Clone)]
pub(crate) enum State {
    Open,
    ReadClosed,
    WriteClosed,
    ClosingRead {
        /// Whether the write side of our channel was already closed.
        write_closed: bool,
        inner: Closing,
    },
    ClosingWrite {
        /// Whether the write side of our channel was already closed.
        read_closed: bool,
        inner: Closing,
    },
    BothClosed {
        reset: bool,
    },
}

pub struct Substream {
    // io: FramedDc,
    io: Arc<DataChannel>,
    state: State,
    read_buffer: Bytes,
    // Dropping this will close the oneshot and notify the receiver by emitting `Canceled`.
    // drop_notifier: Option<oneshot::Sender<GracefullyClosed>>,
}

impl Substream {
    /// Returns a new `Substream` and a listener, which will notify the receiver when/if the substream
    /// is dropped.
    pub(crate) fn new(data_channel: Arc<DataChannel>) -> Self {
        // let (sender, receiver) = oneshot::channel();

        let substream = Self {
            io: data_channel,
            state: State::Open,
            read_buffer: Bytes::default(),
            // drop_notifier: Some(sender),
        };
        // let listener = DropListener::new(framed_dc::new(data_channel), receiver);

        // (substream, listener)
        substream
    }

    /// Write data to the channel.
    pub async fn write<T: Into<Bytes>>(&mut self, data: T) {
        self.io.write(&data.into()).await.unwrap();
    }

    /// Read data from the channel.
    pub async fn read(&mut self, out_buf: &mut [u8]) -> usize {
        self.io.read(out_buf).await.unwrap()
    }
}

pub(crate) fn noise_prologue(
    client_fingerprint: Fingerprint,
    server_fingerprint: Fingerprint,
) -> Vec<u8> {
    let client = client_fingerprint.to_multihash().to_bytes();
    let server = server_fingerprint.to_multihash().to_bytes();
    const PREFIX: &[u8] = b"libp2p-webrtc-noise:";
    let mut out = Vec::with_capacity(PREFIX.len() + client.len() + server.len());
    out.extend_from_slice(PREFIX);
    out.extend_from_slice(&client);
    out.extend_from_slice(&server);
    out
}

/// Creates a new inbound WebRTC connection.
pub(crate) async fn inbound(
    addr: SocketAddr,
    config: RTCConfiguration,
    udp_mux: Arc<dyn UDPMux + Send + Sync>,
    server_fingerprint: Fingerprint,
    remote_ufrag: String,
    id_keys: Keypair,
) -> Result<(PeerId, ()), ()> {
    tracing::debug!("new inbound connection from {addr} (ufrag: {remote_ufrag})");

    let peer_connection = new_inbound_connection(addr, config, udp_mux, &remote_ufrag)
        .await
        .unwrap();

    let offer = sdp::offer(addr, &remote_ufrag);
    tracing::debug!("calculated SDP offer for inbound connection: {:?}", offer);
    peer_connection.set_remote_description(offer).await.unwrap();

    let answer = peer_connection.create_answer(None).await.unwrap();
    tracing::debug!("created SDP answer for inbound connection: {:?}", answer);
    peer_connection.set_local_description(answer).await.unwrap(); // This will start the gathering of ICE candidates.

    tracing::error!("create substream for noise handshake");

    let mut data_channel = create_substream_for_noise_handshake(&peer_connection)
        .await
        .unwrap();

    tracing::error!("get remote fingerprint");
    let client_fingerprint = get_remote_fingerprint(&peer_connection).await;
    tracing::error!("handshake noise");

    use crate::crypto::noise::NoiseContext;

    let prologue = noise_prologue(client_fingerprint, server_fingerprint);
    let mut noise = NoiseContext::with_prologue(&id_keys, prologue);

    let message = noise.first_message(Role::Dialer);
    let message = WebRtcMessage::encode(message, None);

    data_channel.write(message).await;

    let mut data = vec![0u8; 1024];
    let nread = data_channel.read(&mut data).await;

    let message = WebRtcMessage::decode(&data[..nread])
        .unwrap()
        .payload
        .unwrap();
    let public_key = noise.get_remote_public_key(&message).unwrap();
    let remote_peer_id = PeerId::from_public_key(&public_key);

    tracing::info!("remote peer id: {remote_peer_id}");

    let message = noise.second_message();
    let message = WebRtcMessage::encode(message, None);

    data_channel.write(message).await;

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // let peer_id = noise::inbound(
    //     id_keys,
    //     data_channel,
    //     client_fingerprint,
    //     server_fingerprint,
    // )
    // .await
    // .unwrap();

    // Ok((peer_id, Connection::new(peer_connection).await))
    todo!();
}

async fn new_inbound_connection(
    addr: SocketAddr,
    config: RTCConfiguration,
    udp_mux: Arc<dyn UDPMux + Send + Sync>,
    ufrag: &str,
) -> Result<RTCPeerConnection, Error> {
    let mut se = setting_engine(udp_mux, ufrag, addr);
    {
        se.set_lite(true);
        se.disable_certificate_fingerprint_verification(true);
        // Act as a DTLS server (one which waits for a connection).
        //
        // NOTE: removing this seems to break DTLS setup (both sides send `ClientHello` messages,
        // but none end up responding).
        se.set_answering_dtls_role(DTLSRole::Server)?;
    }

    let connection = APIBuilder::new()
        .with_setting_engine(se)
        .build()
        .new_peer_connection(config)
        .await
        .unwrap();

    Ok(connection)
}

/// Generates a random ufrag and adds a prefix according to the spec.
fn random_ufrag() -> String {
    format!(
        "libp2p+webrtc+v1/{}",
        thread_rng()
            .sample_iter(&Alphanumeric)
            .take(64)
            .map(char::from)
            .collect::<String>()
    )
}

fn setting_engine(
    udp_mux: Arc<dyn UDPMux + Send + Sync>,
    ufrag: &str,
    addr: SocketAddr,
) -> SettingEngine {
    let mut se = SettingEngine::default();

    // Set both ICE user and password to our fingerprint because that's what the client is
    // expecting..
    se.set_ice_credentials(ufrag.to_owned(), ufrag.to_owned());

    se.set_udp_network(UDPNetwork::Muxed(udp_mux.clone()));

    // Allow detaching data channels.
    se.detach_data_channels();

    // Set the desired network type.
    //
    // NOTE: if not set, a [`webrtc_ice::agent::Agent`] might pick a wrong local candidate
    // (e.g. IPv6 `[::1]` while dialing an IPv4 `10.11.12.13`).
    let network_type = match addr {
        SocketAddr::V4(_) => NetworkType::Udp4,
        SocketAddr::V6(_) => NetworkType::Udp6,
    };
    se.set_network_types(vec![network_type]);

    se
}

/// Returns the SHA-256 fingerprint of the remote.
async fn get_remote_fingerprint(conn: &RTCPeerConnection) -> Fingerprint {
    let cert_bytes = conn.sctp().transport().get_remote_certificate().await;

    Fingerprint::from_certificate(&cert_bytes)
}

async fn create_substream_for_noise_handshake(conn: &RTCPeerConnection) -> Result<Substream, ()> {
    let data_channel = conn
        .create_data_channel(
            "",
            Some(RTCDataChannelInit {
                negotiated: Some(0), // 0 is reserved for the Noise substream
                ..RTCDataChannelInit::default()
            }),
        )
        .await
        .unwrap();

    tracing::error!("data channel opened");

    let (tx, rx) = oneshot::channel::<Arc<DataChannel>>();

    // Wait until the data channel is opened and detach it.
    register_data_channel_open_handler(data_channel, tx).await;

    tracing::error!("register data channel to open handler");

    let channel = match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Err(error) => {
            tracing::error!("timeout");
            return Err(());
        }
        Ok(Ok(channel)) => channel,
        Ok(Err(error)) => {
            tracing::error!("failed to open data channel");
            return Err(());
        }
    };

    let substream = Substream::new(channel);

    Ok(substream)
}

async fn register_data_channel_open_handler(
    data_channel: Arc<RTCDataChannel>,
    data_channel_tx: Sender<Arc<DetachedDataChannel>>,
) {
    data_channel.on_open({
        let data_channel = data_channel.clone();
        Box::new(move || {
            tracing::debug!("Data channel {} open", data_channel.id());

            Box::pin(async move {
                let data_channel = data_channel.clone();
                let id = data_channel.id();
                match data_channel.detach().await {
                    Ok(detached) => {
                        if let Err(e) = data_channel_tx.send(detached.clone()) {
                            tracing::error!("Can't send data channel {}: {:?}", id, e);
                            if let Err(e) = detached.close().await {
                                tracing::error!("Failed to close data channel {}: {}", id, e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Can't detach data channel {}: {}", id, e);
                    }
                };
            })
        })
    });
}
