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
    error::ParseError,
    transport::webrtc::schema::{self, webrtc::message::Flag},
};

use prost::Message;

/// WebRTC message.
#[derive(Debug)]
pub struct WebRtcMessage {
    /// Payload.
    pub payload: Option<Vec<u8>>,

    /// Flag.
    pub flag: Option<Flag>,
}

impl WebRtcMessage {
    /// Encode WebRTC message with optional flag.
    ///
    /// Interop note: emits a raw protobuf `Message`, with no preceding unsigned-varint
    /// length prefix. SCTP preserves message boundaries on the data channel, so the
    /// libp2p webrtc-direct spec does not require one and go-libp2p/rust-libp2p do not
    /// emit it. This deviates from previous litep2p builds, so litep2p ↔ litep2p WebRTC
    /// no longer interoperates with peers running the prefix variant.
    pub fn encode(payload: Vec<u8>, flag: Option<Flag>) -> Vec<u8> {
        let protobuf_payload = schema::webrtc::Message {
            message: (!payload.is_empty()).then_some(payload),
            flag: flag.map(|f| f as i32),
        };

        let mut out_buf = Vec::with_capacity(protobuf_payload.encoded_len());
        protobuf_payload
            .encode(&mut out_buf)
            .expect("Vec<u8> to provide needed capacity");
        out_buf
    }

    /// Decode payload into [`WebRtcMessage`].
    ///
    /// Treats the entire input slice as a protobuf `Message` (no varint length prefix).
    ///
    /// # Flag handling
    ///
    /// Unknown flag values (e.g., from a newer protocol version) are logged as warnings
    /// and treated as `None` for forward compatibility. This allows the message payload
    /// to still be processed even if the flag is not recognized.
    pub fn decode(payload: &[u8]) -> Result<Self, ParseError> {
        match schema::webrtc::Message::decode(payload) {
            Ok(message) => {
                let flag = message.flag.and_then(|f| match Flag::try_from(f) {
                    Ok(flag) => Some(flag),
                    Err(_) => {
                        tracing::warn!(
                            target: "litep2p::webrtc",
                            ?f,
                            "received message with unknown flag value, ignoring flag"
                        );
                        None
                    }
                });
                Ok(Self {
                    payload: message.message,
                    flag,
                })
            }
            Err(_) => Err(ParseError::InvalidData),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_payload_no_flag() {
        let message = WebRtcMessage::encode("Hello, world!".as_bytes().to_vec(), None);
        let decoded = WebRtcMessage::decode(&message).unwrap();

        assert_eq!(decoded.payload, Some("Hello, world!".as_bytes().to_vec()));
        assert_eq!(decoded.flag, None);
    }

    #[test]
    fn with_payload_and_flag() {
        let message =
            WebRtcMessage::encode("Hello, world!".as_bytes().to_vec(), Some(Flag::StopSending));
        let decoded = WebRtcMessage::decode(&message).unwrap();

        assert_eq!(decoded.payload, Some("Hello, world!".as_bytes().to_vec()));
        assert_eq!(decoded.flag, Some(Flag::StopSending));
    }

    #[test]
    fn no_payload_with_flag() {
        let message = WebRtcMessage::encode(vec![], Some(Flag::ResetStream));
        let decoded = WebRtcMessage::decode(&message).unwrap();

        assert_eq!(decoded.payload, None);
        assert_eq!(decoded.flag, Some(Flag::ResetStream));
    }
}
