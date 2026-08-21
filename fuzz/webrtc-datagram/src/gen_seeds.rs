//! One-shot generator for the committed `webrtc-datagram` seed corpus.
//!
//! Run with `cargo run --bin gen_seeds -- corpus` to (re)generate the seeds under `corpus/`.
//!
//! # What the seeds are
//!
//! The harness completes a real WebRTC handshake at runtime, then writes the fuzz input to the
//! server's Noise channel. A seed file is split into chunks on length-byte boundaries (see
//! `channel_chunks` in `main.rs`); the server concatenates the chunks and reads them as a sequence
//! of `unsigned-varint length ++ body` frames in `extract_framed_message`, then prost-decodes each
//! body as a `webrtc.Message`. These seeds hand the fuzzer valid framings and the known permanent
//! error cases to start mutation from, since the crypto that guards this path is performed by the
//! harness, not the fuzzer.

use std::{fs, path::PathBuf};

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

/// A single `varint(len) ++ body` frame, the unit `extract_framed_message` reads.
fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = uvarint(body.len() as u64);
    out.extend_from_slice(body);
    out
}

/// Encode chunks into the harness input format: `(len_byte ++ chunk)*`, each chunk <= 255 bytes.
/// Splitting a frame across chunks is how a seed exercises cross-SCTP-message reassembly.
fn chunks(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        assert!(part.len() <= u8::MAX as usize, "chunk too large for a 1-byte length prefix");
        out.push(part.len() as u8);
        out.extend_from_slice(part);
    }
    out
}

fn main() {
    let dir: PathBuf = std::env::args().nth(1).unwrap_or_else(|| "corpus".to_string()).into();
    fs::create_dir_all(&dir).expect("corpus directory to be creatable");

    // A minimal valid `webrtc.Message { message: [0xAA, 0xBB] }`: field 2 (bytes), length 2. This
    // decodes to `{ payload: Some(_), flag: None }`, the shape `on_noise_channel_data` accepts, so
    // the frame reaches `get_remote_peer_id`.
    let message = [0x12u8, 0x02, 0xAA, 0xBB];
    let valid = frame(&message);

    let seeds: Vec<(&str, Vec<u8>)> = vec![
        // One complete frame in one chunk: framing + decode + get_remote_peer_id.
        ("frame-message", chunks(&[&valid])),
        // The varint and body in separate chunks: cross-message reassembly, the Ok(None) path.
        ("frame-split", chunks(&[&valid[..1], &valid[1..]])),
        // Empty-body frame: decodes to an empty message.
        ("frame-empty", chunks(&[&frame(&[])])),
        // Non-minimal varint zero (0x80 0x00): a permanent framing error that closes the channel.
        ("frame-nonminimal", chunks(&[&[0x80, 0x00]])),
        // A varint declaring a body far larger than MAX_FRAME_SIZE: rejected before the body.
        ("frame-oversized", chunks(&[&uvarint(1 << 20)])),
    ];

    let mut written = 0;
    for (name, bytes) in &seeds {
        fs::write(dir.join(name), bytes).expect("seed to be writable");
        written += 1;
    }

    eprintln!("wrote {written} end-to-end seeds to {}", dir.display());
}
