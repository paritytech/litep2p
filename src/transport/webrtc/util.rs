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

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::webrtc";

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
        // ilog2 gives the position of the highest set bit (0-indexed), divide by 7 for varint bytes
        let varint_len = if protobuf_len == 0 {
            1
        } else {
            (protobuf_len.ilog2() as usize / 7) + 1
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

    /// Decode a protobuf-encoded [`schema::webrtc::Message`] body with no varint length prefix.
    ///
    /// # Flag handling
    ///
    /// Unknown flag values (e.g., from a newer protocol version) are logged as warnings
    /// and treated as `None` for forward compatibility. This allows the message payload
    /// to still be processed even if the flag is not recognized.
    pub fn decode(protobuf_data: &[u8]) -> Result<Self, ParseError> {
        match schema::webrtc::Message::decode(protobuf_data) {
            Ok(message) => {
                let flag = message.flag.and_then(|f| match Flag::try_from(f) {
                    Ok(flag) => Some(flag),
                    Err(_) => {
                        tracing::warn!(
                            target: LOG_TARGET,
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

/// Try to extract one complete `varint length ++ body` frame from the front of `buffer`.
///
/// On success, returns `Some(body)` and removes the consumed bytes (varint + body)
/// from `buffer`. On failure, leaves `buffer` untouched and returns `None`.
///
/// `None` is returned when either:
/// - the varint length prefix is not fully buffered yet, or
/// - fewer than `len` body bytes follow the varint.
///
/// This is the building block used by the WebRTC inbound path to reassemble
/// pbio-delimited frames that go-libp2p splits across two SCTP messages
/// (varint in one message, body in the next).
pub fn extract_framed_message(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let (len, remaining) = unsigned_varint::decode::usize(buffer).ok()?;

    if remaining.len() < len {
        return None;
    }

    let varint_len = buffer.len() - remaining.len();
    let body = remaining[..len].to_vec();
    buffer.drain(..varint_len + len);
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip the unsigned-varint length prefix that [`WebRtcMessage::encode`] prepends,
    /// returning the bare protobuf body that [`WebRtcMessage::decode`] expects.
    fn protobuf_body(encoded: &[u8]) -> &[u8] {
        let (len, rest) = unsigned_varint::decode::usize(encoded).unwrap();
        &rest[..len]
    }

    #[test]
    fn with_payload_no_flag() {
        let message = WebRtcMessage::encode("Hello, world!".as_bytes().to_vec(), None);
        let decoded = WebRtcMessage::decode(protobuf_body(&message)).unwrap();

        assert_eq!(decoded.payload, Some("Hello, world!".as_bytes().to_vec()));
        assert_eq!(decoded.flag, None);
    }

    #[test]
    fn with_payload_and_flag() {
        let message =
            WebRtcMessage::encode("Hello, world!".as_bytes().to_vec(), Some(Flag::StopSending));
        let decoded = WebRtcMessage::decode(protobuf_body(&message)).unwrap();

        assert_eq!(decoded.payload, Some("Hello, world!".as_bytes().to_vec()));
        assert_eq!(decoded.flag, Some(Flag::StopSending));
    }

    #[test]
    fn no_payload_with_flag() {
        let message = WebRtcMessage::encode(vec![], Some(Flag::ResetStream));
        let decoded = WebRtcMessage::decode(protobuf_body(&message)).unwrap();

        assert_eq!(decoded.payload, None);
        assert_eq!(decoded.flag, Some(Flag::ResetStream));
    }

    #[test]
    fn extract_single_frame_one_chunk() {
        // The common case: a peer (e.g. smoldot) sends the whole `varint ++ body`
        // in a single SCTP message. Extraction should succeed and drain the buffer.
        let frame = WebRtcMessage::encode(b"hello".to_vec(), None);
        let mut buffer = frame.clone();

        let body = extract_framed_message(&mut buffer).expect("complete frame");
        assert_eq!(body, protobuf_body(&frame));
        assert!(buffer.is_empty(), "buffer fully drained");
    }

    #[test]
    fn extract_single_frame_empty_body() {
        // A zero-length body is a legal frame: `0x00` varint, no body bytes.
        let mut buffer = vec![0x00];

        let body = extract_framed_message(&mut buffer).expect("zero-length frame");
        assert!(body.is_empty());
        assert!(buffer.is_empty());
    }

    #[test]
    fn extract_frame_split_varint_then_body() {
        // go-libp2p's pbio writer issues two `Write` calls (varint, then body)
        // which surface as two SCTP messages.
        let frame = WebRtcMessage::encode(b"split-across-sctp".to_vec(), None);
        let (len, rest) = unsigned_varint::decode::usize(&frame).unwrap();
        let varint_bytes = &frame[..frame.len() - rest.len()];
        let body_bytes = &rest[..len];

        let mut buffer = Vec::new();

        // SCTP message #1: just the varint. No complete frame yet.
        buffer.extend_from_slice(varint_bytes);
        assert!(extract_framed_message(&mut buffer).is_none());
        assert_eq!(buffer, varint_bytes, "varint preserved for next attempt");

        // SCTP message #2: the body arrives. Extraction now succeeds.
        buffer.extend_from_slice(body_bytes);
        let body = extract_framed_message(&mut buffer).expect("frame now complete");
        assert_eq!(body, body_bytes);
        assert!(buffer.is_empty());
    }

    #[test]
    fn extract_frame_split_with_multi_byte_varint() {
        // Real noise frames are bigger than 128 bytes, so the varint itself is multi-byte.
        // Verify the split still works when the varint takes 2 bytes.
        let payload = vec![0xab; 300];
        let frame = WebRtcMessage::encode(payload, None);
        let (len, rest) = unsigned_varint::decode::usize(&frame).unwrap();
        let varint_bytes = &frame[..frame.len() - rest.len()];
        let body_bytes = &rest[..len];
        assert!(varint_bytes.len() >= 2, "expected multi-byte varint");

        let mut buffer = Vec::new();
        buffer.extend_from_slice(varint_bytes);
        assert!(extract_framed_message(&mut buffer).is_none());

        buffer.extend_from_slice(body_bytes);
        let body = extract_framed_message(&mut buffer).expect("complete frame");
        assert_eq!(body.len(), len);
        assert_eq!(body, body_bytes);
        assert!(buffer.is_empty());
    }

    #[test]
    fn extract_frame_with_partial_varint() {
        // Even more adversarial: the varint itself is split across SCTP messages.
        // First byte alone has the high bit set, so the varint isn't decodable yet.
        let payload = vec![0xcd; 300];
        let frame = WebRtcMessage::encode(payload, None);
        let (len, rest) = unsigned_varint::decode::usize(&frame).unwrap();
        let varint_bytes = &frame[..frame.len() - rest.len()];
        assert!(varint_bytes.len() >= 2);

        let mut buffer = Vec::new();

        // First byte of the varint only — undecodable.
        buffer.extend_from_slice(&varint_bytes[..1]);
        assert!(extract_framed_message(&mut buffer).is_none());

        // Remainder of varint arrives, body still missing.
        buffer.extend_from_slice(&varint_bytes[1..]);
        assert!(extract_framed_message(&mut buffer).is_none());

        // Body arrives — frame now complete.
        buffer.extend_from_slice(&rest[..len]);
        let body = extract_framed_message(&mut buffer).expect("complete frame");
        assert_eq!(body.len(), len);
        assert!(buffer.is_empty());
    }

    #[test]
    fn extract_from_empty_buffer() {
        let mut buffer: Vec<u8> = Vec::new();
        assert!(extract_framed_message(&mut buffer).is_none());
        assert!(buffer.is_empty());
    }

    #[test]
    fn extract_two_frames_concatenated() {
        // Inbound path drains in a loop: if two frames are coalesced into one SCTP
        // message, two consecutive extractions must each yield one body.
        let frame_a = WebRtcMessage::encode(b"first".to_vec(), None);
        let frame_b = WebRtcMessage::encode(b"second".to_vec(), None);
        let body_a = protobuf_body(&frame_a).to_vec();
        let body_b = protobuf_body(&frame_b).to_vec();

        let mut buffer = Vec::new();
        buffer.extend_from_slice(&frame_a);
        buffer.extend_from_slice(&frame_b);

        let extracted_a = extract_framed_message(&mut buffer).expect("first frame");
        assert_eq!(extracted_a, body_a);

        let extracted_b = extract_framed_message(&mut buffer).expect("second frame");
        assert_eq!(extracted_b, body_b);

        assert!(buffer.is_empty());
        assert!(extract_framed_message(&mut buffer).is_none());
    }

    #[test]
    fn extract_frame_then_partial_next_frame() {
        // One complete frame followed by the start of a second frame: the first
        // frame is returned and the partial bytes of frame #2 remain buffered.
        let frame_a = WebRtcMessage::encode(b"complete".to_vec(), None);
        let frame_b = WebRtcMessage::encode(b"incoming".to_vec(), None);
        let body_a = protobuf_body(&frame_a).to_vec();

        let mut buffer = Vec::new();
        buffer.extend_from_slice(&frame_a);
        buffer.extend_from_slice(&frame_b[..2]); // partial second frame

        let extracted = extract_framed_message(&mut buffer).expect("first frame");
        assert_eq!(extracted, body_a);
        assert_eq!(buffer, frame_b[..2], "partial second frame preserved");

        // Second extraction is a no-op until the rest of frame_b arrives.
        assert!(extract_framed_message(&mut buffer).is_none());
        buffer.extend_from_slice(&frame_b[2..]);

        let extracted_b = extract_framed_message(&mut buffer).expect("second frame complete");
        assert_eq!(extracted_b, protobuf_body(&frame_b));
        assert!(buffer.is_empty());
    }

    #[test]
    fn extract_body_arrives_byte_by_byte() {
        // Worst-case fragmentation: every body byte arrives in its own SCTP message.
        let payload: Vec<u8> = (0..50u8).collect();
        let frame = WebRtcMessage::encode(payload.clone(), None);
        let (len, rest) = unsigned_varint::decode::usize(&frame).unwrap();
        let varint_bytes = &frame[..frame.len() - rest.len()];
        let body_bytes = rest[..len].to_vec();

        let mut buffer = Vec::new();
        buffer.extend_from_slice(varint_bytes);
        assert!(extract_framed_message(&mut buffer).is_none());

        for (i, byte) in body_bytes.iter().enumerate() {
            buffer.push(*byte);
            if i + 1 < body_bytes.len() {
                assert!(
                    extract_framed_message(&mut buffer).is_none(),
                    "should still be waiting at byte {i}",
                );
            }
        }

        let extracted = extract_framed_message(&mut buffer).expect("complete frame");
        assert_eq!(extracted, body_bytes);
        assert!(buffer.is_empty());
    }

    #[test]
    fn extract_does_not_consume_on_failure() {
        // On `None`, the buffer must be left exactly as-is so the caller can
        // append more bytes and retry. Verify both for the partial-varint and
        // partial-body cases.
        let frame = WebRtcMessage::encode(vec![0u8; 200], None);
        let (len, rest) = unsigned_varint::decode::usize(&frame).unwrap();
        let varint_bytes = &frame[..frame.len() - rest.len()];

        // Partial varint.
        let mut buffer = varint_bytes[..1].to_vec();
        let snapshot = buffer.clone();
        assert!(extract_framed_message(&mut buffer).is_none());
        assert_eq!(buffer, snapshot);

        // Complete varint, partial body.
        let mut buffer = varint_bytes.to_vec();
        buffer.extend_from_slice(&rest[..len / 2]);
        let snapshot = buffer.clone();
        assert!(extract_framed_message(&mut buffer).is_none());
        assert_eq!(buffer, snapshot);
    }
}
