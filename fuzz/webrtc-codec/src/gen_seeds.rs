// Copyright 2026 litep2p developers
//
// Licensed under the same terms as the rest of this repository; see `src/main.rs`.

//! Seed corpus generator for the `webrtc-codec` harness.
//!
//! Run with `cargo run --bin gen_seeds -- corpus/` to (re)populate the corpus. Seeds are
//! committed so a fuzzing run starts from inputs that already reach past the first length
//! check, rather than spending its early budget rediscovering the frame format.
//!
//! Every seed is `[sub-target selector] ++ payload`, matching the dispatch in `main.rs`.

use litep2p::transport::webrtc::{
    schema::webrtc::message::Flag,
    util::{WebRtcMessage, MAX_FRAME_SIZE},
};
use std::{fs, path::PathBuf};

/// Number of protocols the listener offers in the negotiation seeds.
///
/// Matches `SUPPORTED_PROTOCOLS.len()` in `main.rs`; the modulo there maps this to the full
/// set.
const ALL_PROTOCOLS: u8 = 3;

/// A valid Ed25519 public key, from RFC 8032 test vector 1.
///
/// Any real curve point works. What does not work is an arbitrary 32-byte string, because
/// `ed25519::PublicKey::try_from_bytes` decompresses the point and rejects anything that is
/// not on the curve. No private key is needed: the signature can never verify anyway, and
/// reaching the verification step is the point.
const ED25519_PUBLIC_KEY: [u8; 32] = [
    0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07, 0x3a,
    0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07, 0x51, 0x1a,
];

/// Build the flags byte for the listener-negotiation sub-target.
fn negotiate_flags(header_received: bool, protocol_count: u8) -> u8 {
    (protocol_count << 1) | u8::from(header_received)
}

/// Wrap a frame as a single length-prefixed chunk for the reassembly sub-target.
fn one_chunk(frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for piece in frame.chunks(u8::MAX as usize) {
        out.push(piece.len() as u8);
        out.extend_from_slice(piece);
    }
    out
}

/// Split a frame into two chunks at `at`, so the reassembly path is seeded already
/// fragmented — the split-varint and partial-body states are where the bugs live.
fn two_chunks(frame: &[u8], at: usize) -> Vec<u8> {
    let (head, tail) = frame.split_at(at.min(frame.len()));
    let mut out = Vec::new();
    for piece in [head, tail] {
        for sub in piece.chunks(u8::MAX as usize) {
            out.push(sub.len() as u8);
            out.extend_from_slice(sub);
        }
    }
    out
}

fn seeds() -> Vec<(String, Vec<u8>)> {
    let mut seeds: Vec<(String, Vec<u8>)> = Vec::new();

    let hello = WebRtcMessage::encode(b"hello".to_vec(), None);
    let with_flag = WebRtcMessage::encode(b"hello".to_vec(), Some(Flag::Fin));
    let empty_with_flag = WebRtcMessage::encode(vec![], Some(Flag::ResetStream));
    let large = WebRtcMessage::encode(vec![0xab; 300], None);

    // Sub-target 0: frame reassembly.
    seeds.push(("reassembly-single".into(), one_chunk(&hello)));
    seeds.push(("reassembly-flag".into(), one_chunk(&with_flag)));
    seeds.push(("reassembly-empty-body".into(), one_chunk(&[0x00])));
    seeds.push((
        "reassembly-two-frames".into(),
        one_chunk(&[hello.clone(), with_flag.clone()].concat()),
    ));
    // Multi-byte varint split from its body: go-libp2p's pbio writer really does this.
    seeds.push(("reassembly-split-varint".into(), two_chunks(&large, 1)));
    seeds.push(("reassembly-split-body".into(), two_chunks(&large, 2)));
    seeds.push(("reassembly-overlong-varint".into(), one_chunk(&[0x80; 11])));
    seeds.push(("reassembly-non-minimal-varint".into(), one_chunk(&[0x80, 0x00])));

    let mut varint_buf = unsigned_varint::encode::usize_buffer();
    let oversized = unsigned_varint::encode::usize(MAX_FRAME_SIZE + 1, &mut varint_buf).to_vec();
    seeds.push(("reassembly-oversized".into(), one_chunk(&oversized)));

    // Sub-target 1: bare protobuf decode. Bodies only, prefix stripped.
    for (name, frame) in [
        ("decode-payload", &hello),
        ("decode-flag", &with_flag),
        ("decode-empty-payload", &empty_with_flag),
    ] {
        let (len, body) = unsigned_varint::decode::usize(frame).expect("valid frame");
        seeds.push((name.into(), body[..len].to_vec()));
    }
    // An unknown flag integer must be tolerated, not rejected: field 1 varint = 99.
    seeds.push(("decode-unknown-flag".into(), vec![0x08, 0x63]));

    // Sub-target 2: round-trip. `[flag selector, length byte, ...payload bytes]`.
    for flag_selector in 0..5u8 {
        seeds.push((
            format!("roundtrip-flag-{flag_selector}"),
            vec![flag_selector, 0x04, 0xde, 0xad, 0xbe, 0xef],
        ));
    }
    // A length byte of 64 means 64 * 256 = 16 KiB, straddling MAX_FRAME_SIZE.
    seeds.push(("roundtrip-at-max-frame".into(), vec![0, 64, 0xa5]));

    // Sub-target 3/4: multistream-select. `[flags]` / `[count]` then the wire payload.
    let ms_header = b"\x13/multistream/1.0.0\n".to_vec();
    let ms_protocol = b"\x11/ipfs/ping/1.0.0\n".to_vec();
    let ms_na = b"\x03na\n".to_vec();

    // `main.rs` reads bit 0 of this byte as `header_received` and the remaining bits as
    // `(flags >> 1) % (SUPPORTED_PROTOCOLS.len() + 1)`, the number of protocols the listener
    // offers. A flags byte of 0 or 1 therefore offers *nothing*, which makes `Accepted`
    // unreachable and leaves the corpus able to produce only reject and pending. Every seed
    // that is meant to negotiate has to offer the full set.
    let offer_all = negotiate_flags(false, ALL_PROTOCOLS);
    let offer_all_header_done = negotiate_flags(true, ALL_PROTOCOLS);

    seeds.push((
        "negotiate-header-and-protocol".into(),
        [vec![offer_all], ms_header.clone(), ms_protocol.clone()].concat(),
    ));
    seeds.push((
        "negotiate-protocol-only".into(),
        [vec![offer_all_header_done], ms_protocol.clone()].concat(),
    ));
    seeds.push((
        "negotiate-header-only".into(),
        [vec![offer_all], ms_header.clone()].concat(),
    ));
    // `ls\n` decodes to `Message::ListProtocols`, which is not a valid first message for the
    // listener, so this seeds the catch-all rejection rather than the `MAX_PROTOCOLS` loop.
    seeds.push((
        "negotiate-ls-rejected".into(),
        [vec![offer_all], ms_header.clone(), b"\x03ls\n".to_vec()].concat(),
    ));
    // Offering nothing must reject cleanly rather than index into an empty slice. Kept
    // deliberately, and named so it is not mistaken for a stale seed.
    seeds.push((
        "negotiate-no-local-protocols".into(),
        [vec![negotiate_flags(false, 0)], ms_header.clone(), ms_protocol.clone()].concat(),
    ));

    seeds.push((
        "dialer-accepts".into(),
        [
            vec![1u8, (ms_header.len() + ms_protocol.len()) as u8],
            ms_header.clone(),
            ms_protocol,
        ]
        .concat(),
    ));
    seeds.push((
        "dialer-rejects".into(),
        [vec![1u8, (ms_header.len() + ms_na.len()) as u8], ms_header, ms_na].concat(),
    ));

    // Sub-target 5: noise identity payload. `[dh key length] ++ dh key ++ payload`.
    //
    // The identity key has to be a real curve point. `VerifyingKey::from_bytes`
    // decompresses and validates it, so a made-up 32-byte string fails in
    // `RemotePublicKey::from_protobuf_encoding` and the run never reaches
    // `PeerId::from_public_key_protobuf` or the signature check at all.
    //
    // `keys_proto::PublicKey { Type: Ed25519 = 1, Data: <key> }` is field 1 as a varint and
    // field 2 as bytes, so `08 01 12 20 ..`. `NoiseHandshakePayload` carries that as
    // `identity_key` in field 1 and the signature as `identity_sig` in field 2.
    let identity_key = [vec![0x08, 0x01, 0x12, 0x20], ED25519_PUBLIC_KEY.to_vec()].concat();
    let payload = [
        vec![0x0a, identity_key.len() as u8],
        identity_key,
        vec![0x12, 64],
        vec![0x5a; 64],
    ]
    .concat();
    seeds.push((
        "noise-payload".into(),
        [vec![32u8], vec![0x11; 32], payload].concat(),
    ));
    seeds.push(("noise-empty".into(), vec![0u8]));

    seeds
}

fn main() {
    let dir: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "corpus".to_string())
        .into();
    fs::create_dir_all(&dir).expect("corpus directory to be creatable");

    // Sub-target selectors, keyed by the seed-name prefix used above.
    let selector_for = |name: &str| -> u8 {
        match name.split('-').next().expect("non-empty name") {
            "reassembly" => 0,
            "decode" => 1,
            "roundtrip" => 2,
            "negotiate" => 3,
            "dialer" => 4,
            "noise" => 5,
            other => panic!("seed name {other} does not map to a sub-target"),
        }
    };

    let mut written = 0;
    for (name, payload) in seeds() {
        let mut bytes = vec![selector_for(&name)];
        bytes.extend_from_slice(&payload);
        fs::write(dir.join(&name), &bytes).expect("seed to be writable");
        written += 1;
    }

    eprintln!("wrote {written} seeds to {}", dir.display());
}
