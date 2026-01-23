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
    /// Uses a single allocation by pre-calculating the total size and encoding
    /// the varint length prefix and protobuf message directly into the output buffer.
    pub fn encode(payload: Vec<u8>, flag: Option<Flag>) -> Vec<u8> {
        let protobuf_payload = schema::webrtc::Message {
            message: (!payload.is_empty()).then_some(payload),
            flag: flag.map(|f| f as i32),
        };

        // Calculate sizes upfront for single allocation with exact capacity
        let protobuf_len = protobuf_payload.encoded_len();
        // Varint uses 7 bits per byte, so calculate exact length needed
        let varint_len = if protobuf_len == 0 {
            1
        } else {
            (usize::BITS - protobuf_len.leading_zeros()) as usize / 7 + 1
        };

        // Single allocation for the entire output with exact size
        let mut out_buf = Vec::with_capacity(varint_len + protobuf_len);

        // Encode varint length prefix directly
        let mut varint_buf = unsigned_varint::encode::usize_buffer();
        let varint_slice = unsigned_varint::encode::usize(protobuf_len, &mut varint_buf);
        out_buf.extend_from_slice(varint_slice);

        // Encode protobuf directly into output buffer
        protobuf_payload
            .encode(&mut out_buf)
            .expect("Vec<u8> to provide needed capacity");

        out_buf
    }

    /// Decode payload into [`WebRtcMessage`].
    ///
    /// Decodes the varint length prefix directly from the slice without allocations,
    /// then decodes the protobuf message from the remaining bytes.
    ///
    /// # Flag handling
    ///
    /// Unknown flag values (e.g., from a newer protocol version) are logged as warnings
    /// and treated as `None` for forward compatibility. This allows the message payload
    /// to still be processed even if the flag is not recognized.
    pub fn decode(payload: &[u8]) -> Result<Self, ParseError> {
        // Decode varint length prefix directly from slice (no allocation)
        // Returns (decoded_length, remaining_bytes_after_varint)
        let (len, remaining) =
            unsigned_varint::decode::usize(payload).map_err(|_| ParseError::InvalidData)?;

        // Get exactly `len` bytes of protobuf data (no allocation)
        let protobuf_data = remaining.get(..len).ok_or(ParseError::InvalidData)?;

        match schema::webrtc::Message::decode(protobuf_data) {
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
