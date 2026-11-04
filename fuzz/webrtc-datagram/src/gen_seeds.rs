// Copyright 2026 litep2p developers
//
// Licensed under the same terms as the rest of this repository; see `src/main.rs`.

//! One-shot generator for the committed `webrtc-datagram` seed corpus.
//!
//! Run with `cargo run --bin gen_seeds -- corpus` to (re)generate the seeds under `corpus/`.
//!
//! # Input format
//!
//! The harness reads the first byte as a target selector (even = the pre-auth noise channel, odd =
//! a post-auth substream) and splits the rest into channel-write chunks on length-byte boundaries
//! (see `channel_chunks` in `main.rs`). Each chunk is one SCTP message.
//!
//! - **Pre-auth** seeds are `unsigned-varint length ++ body` frames read by the server's
//!   `extract_framed_message` / `WebRtcMessage::decode` on the noise channel.
//! - **Post-auth** seeds are multistream-select lines wrapped in a `WebRtcMessage` (the substream
//!   wire format), which the server feeds to `webrtc_listener_negotiate` on a real channel.

use std::{fs, path::PathBuf};

use litep2p::transport::webrtc::util::WebRtcMessage;

/// Encode `n` as an unsigned-varint (LEB128), matching `unsigned_varint::encode`.
fn uvarint(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if n == 0 {
            break;
        }
    }
    out
}

/// A single `varint(len) ++ body` msgio frame.
fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = uvarint(body.len() as u64);
    out.extend_from_slice(body);
    out
}

/// Encode chunks into the harness input format: `(len_byte ++ chunk)*`, each chunk <= 255 bytes.
fn chunks(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        assert!(part.len() <= u8::MAX as usize, "chunk too large for a 1-byte length prefix");
        out.push(part.len() as u8);
        out.extend_from_slice(part);
    }
    out
}

/// Prepend the target-selector byte to a chunk stream.
fn with_mode(mode: u8, mut body: Vec<u8>) -> Vec<u8> {
    let mut out = vec![mode];
    out.append(&mut body);
    out
}

fn main() {
    let dir: PathBuf = std::env::args().nth(1).unwrap_or_else(|| "corpus".to_string()).into();
    fs::create_dir_all(&dir).expect("corpus directory to be creatable");

    // Pre-auth (mode 0): a minimal valid `webrtc.Message { message: [0xAA, 0xBB] }` frame plus the
    // framing edge cases, all read by `on_noise_channel_data`.
    let valid = frame(&[0x12u8, 0x02, 0xAA, 0xBB]);

    // Post-auth (mode 1): multistream-select lines wrapped in a `WebRtcMessage`, the substream wire
    // format the server's negotiation path expects.
    let ms_header = WebRtcMessage::encode(frame(b"/multistream/1.0.0\n"), None);
    let ms_ping = WebRtcMessage::encode(frame(b"/ipfs/ping/1.0.0\n"), None);

    let seeds: Vec<(&str, Vec<u8>)> = vec![
        // --- pre-auth noise-channel framing ---
        ("preauth-frame-message", with_mode(0, chunks(&[&valid]))),
        ("preauth-frame-split", with_mode(0, chunks(&[&valid[..1], &valid[1..]]))),
        ("preauth-frame-nonminimal", with_mode(0, chunks(&[&[0x80, 0x00]]))),
        ("preauth-frame-oversized", with_mode(0, chunks(&[&uvarint(1 << 20)]))),
        // --- post-auth substream negotiation ---
        ("substream-multistream-header", with_mode(1, chunks(&[&ms_header]))),
        ("substream-negotiate-ping", with_mode(1, chunks(&[&ms_header, &ms_ping]))),
    ];

    let mut written = 0;
    for (name, bytes) in &seeds {
        fs::write(dir.join(name), bytes).expect("seed to be writable");
        written += 1;
    }

    eprintln!("wrote {written} end-to-end seeds to {}", dir.display());
}
