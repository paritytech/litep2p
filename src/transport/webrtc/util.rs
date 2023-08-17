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
    error::Error,
    transport::webrtc::{fingerprint::Fingerprint, schema},
};

use bytes::BytesMut;
use futures::{
    channel::{
        mpsc,
        oneshot::{self, Sender},
    },
    lock::Mutex as FutMutex,
    stream::FuturesUnordered,
    StreamExt,
    {future::BoxFuture, ready},
};
use prost::Message;
use tokio_util::codec::{Decoder, Encoder};
use webrtc::{
    data::data_channel::DataChannel as DetachedDataChannel,
    data_channel::RTCDataChannel,
    ice::udp_mux::UDPMux,
    peer_connection::{configuration::RTCConfiguration, RTCPeerConnection},
};

use std::sync::Arc;

/// Logging target for the file.
const LOG_TARGET: &str = "webrtc::util";

/// WebRTC mesage.
#[derive(Debug)]
pub struct WebRtcMessage {
    /// Payload.
    pub payload: Option<Vec<u8>>,

    // Flags.
    pub flags: Option<i32>,
}

impl WebRtcMessage {
    /// Encode `payload` and `flag` into a [`WebRtcMessage`].
    pub fn encode(payload: Vec<u8>, flag: Option<i32>) -> BytesMut {
        let protobuf_payload = schema::webrtc::Message {
            message: (!payload.is_empty()).then_some(payload),
            flag,
        };
        let mut payload = BytesMut::with_capacity(protobuf_payload.encoded_len());
        protobuf_payload
            .encode(&mut payload)
            .expect("Vec<u8> to provide needed capacity");

        payload
    }

    /// Decode payload into [`WebRtcMessage`].
    pub fn decode(payload: &[u8]) -> crate::Result<Self> {
        match schema::webrtc::Message::decode(payload) {
            Ok(message) => Ok(Self {
                payload: message.message,
                flags: message.flag,
            }),
            Err(_) => Err(Error::InvalidData),
        }
    }
}

/// Register handler for incoming substreams.
// TODO: this can possibly be refactored
pub async fn register_incoming_data_channels_handler(
    connection: &RTCPeerConnection,
    tx: Arc<FutMutex<mpsc::Sender<Arc<DetachedDataChannel>>>>,
) {
    connection.on_data_channel(Box::new(move |data_channel: Arc<RTCDataChannel>| {
        tracing::debug!(target: LOG_TARGET, id = ?data_channel.id(), "incoming data channel");

        let tx = tx.clone();

        Box::pin(async move {
            data_channel.on_open({
                let data_channel = data_channel.clone();
                Box::new(move || {
                    tracing::debug!(
                        target: LOG_TARGET,
                        id = ?data_channel.id(),
                        "data channel open",
                    );

                    Box::pin(async move {
                        let data_channel = data_channel.clone();
                        let id = data_channel.id();
                        match data_channel.detach().await {
                            Ok(detached) => {
                                let mut tx = tx.lock().await;
                                if let Err(error) = tx.try_send(detached.clone()) {
                                    tracing::error!(
                                        target: LOG_TARGET,
                                        ?id,
                                        ?error,
                                        "failed to send data channel",
                                    );

                                    // We're not accepting data channels fast enough =>
                                    // close this channel.
                                    //
                                    // Ideally we'd refuse to accept a data channel
                                    // during the negotiation process, but it's not
                                    // possible with the current API.
                                    if let Err(error) = detached.close().await {
                                        tracing::error!(
                                            target: LOG_TARGET,
                                            ?id,
                                            ?error,
                                            "failed to close data channel",
                                        );
                                    }
                                }
                            }
                            Err(error) => {
                                tracing::error!(
                                    target: LOG_TARGET,
                                    ?id,
                                    ?error,
                                    "cannot detach data channel",
                                );
                            }
                        };
                    })
                })
            });
        })
    }));
}

/// Register handler for channel open event.
pub async fn register_data_channel_open_handler(
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

/// Returns the SHA-256 fingerprint of the remote.
pub async fn get_remote_fingerprint(conn: &RTCPeerConnection) -> Fingerprint {
    let cert_bytes = conn.sctp().transport().get_remote_certificate().await;

    Fingerprint::from_certificate(&cert_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_payload_no_flags() {
        let message = WebRtcMessage::encode("Hello, world!".as_bytes().to_vec(), None);
        let decoded = WebRtcMessage::decode(&message).unwrap();

        assert_eq!(decoded.payload, Some("Hello, world!".as_bytes().to_vec()));
        assert_eq!(decoded.flags, None);
    }

    #[test]
    fn with_payload_and_flags() {
        let message = WebRtcMessage::encode("Hello, world!".as_bytes().to_vec(), Some(1i32));
        let decoded = WebRtcMessage::decode(&message).unwrap();

        assert_eq!(decoded.payload, Some("Hello, world!".as_bytes().to_vec()));
        assert_eq!(decoded.flags, Some(1i32));
    }

    #[test]
    fn no_payload_with_flags() {
        let message = WebRtcMessage::encode(vec![], Some(2i32));
        let decoded = WebRtcMessage::decode(&message).unwrap();

        assert_eq!(decoded.payload, None);
        assert_eq!(decoded.flags, Some(2i32));
    }
}
