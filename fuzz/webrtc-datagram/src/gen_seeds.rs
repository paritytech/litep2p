//! One-shot generator for the committed `webrtc-datagram` seed corpus.
//!
//! Run with `cargo run --bin gen_seeds -- corpus` to (re)generate the seeds under `corpus/`.
//!
//! # Why these seeds exist
//!
//! Without a seed, blind mutation never produces a datagram that clears the transport's demux:
//! `on_socket_input` only builds an `Rtc` for a packet that passes `is_stun_packet`, then
//! `StunMessage::parse`, then `split_username` (see `src/main.rs` module docs). The magic cookie
//! alone is 2⁻³² to hit by chance. These seeds start the fuzzer from a valid STUN binding request,
//! so mutation explores from *past* the gate: `make_rtc`, `rtc.handle_input`, and the
//! `OpeningWebRtcConnection` bookkeeping all run. litep2p does not validate the ufrag
//! (`mod.rs:404-442`), so an arbitrary `ufrag:pass` username is enough to create a connection.
//!
//! The requests are built with str0m's own serializer, so whatever `StunMessage::parse` checks
//! (length framing, the mandatory MESSAGE-INTEGRITY and PRIORITY attributes for a binding request,
//! the FINGERPRINT CRC) is satisfied by construction. Each seed is parsed back before it is written,
//! so a broken seed fails generation rather than shipping.
//!
//! # Seed wire format
//!
//! The harness reads the first byte as the sender selector and then splits the remainder into
//! datagrams, each prefixed by a single length byte (`main.rs::datagrams`). So a seed is
//! `selector ++ (len ++ datagram)*`, and every datagram must be at most 255 bytes.

use std::{fs, path::PathBuf};

use str0m::ice::{StunMessage, TransId};

/// Build a STUN binding request that clears the harness gate, and prove it does before returning.
fn binding_request(username: &str, use_candidate: bool) -> Vec<u8> {
    // controlling=true, tie-breaker and priority are arbitrary; the password is never verified at
    // the gate (litep2p calls `parse` + `split_username`, not `verify`), so a dummy HMAC is fine.
    let message = StunMessage::binding_request(username, TransId::new(), true, 0, 100, use_candidate);

    let mut buffer = [0u8; 512];
    let len = message
        .to_bytes(Some(b"fuzz-password"), &mut buffer, |_key, _payloads| [0u8; 20])
        .expect("STUN binding request to serialize");
    let bytes = buffer[..len].to_vec();

    let parsed = StunMessage::parse(&bytes).expect("generated STUN must parse");
    assert!(parsed.split_username().is_some(), "generated STUN must carry a `ufrag:pass` username");
    assert!(bytes.len() <= u8::MAX as usize, "datagram must fit the 1-byte length prefix");
    bytes
}

/// Frame datagrams into the harness input format: `selector ++ (len ++ datagram)*`.
fn frame(selector: u8, datagrams: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![selector];
    for datagram in datagrams {
        assert!(datagram.len() <= u8::MAX as usize, "datagram too large for a 1-byte prefix");
        out.push(datagram.len() as u8);
        out.extend_from_slice(datagram);
    }
    out
}

fn main() {
    let dir: PathBuf = std::env::args().nth(1).unwrap_or_else(|| "corpus".to_string()).into();
    fs::create_dir_all(&dir).expect("corpus directory to be creatable");

    let plain = binding_request("fuzz-remote:fuzz-local", false);
    let use_candidate = binding_request("fuzz-remote:fuzz-local", true);

    let seeds: Vec<(&str, Vec<u8>)> = vec![
        // One request from one source: reaches `make_rtc` and inserts one `OpeningWebRtcConnection`.
        ("stun-binding", frame(0, &[&plain])),
        // The USE-CANDIDATE variant, a distinct attribute layout for the parser to chew on.
        ("stun-binding-usecandidate", frame(0, &[&use_candidate])),
        // Two requests: the two datagrams land on different sender sockets, so the fuzzer starts
        // from a state with two live opening connections in the table.
        ("stun-binding-pair", frame(0, &[&plain, &use_candidate])),
    ];

    let mut written = 0;
    for (name, bytes) in &seeds {
        fs::write(dir.join(name), bytes).expect("seed to be writable");
        written += 1;
    }

    eprintln!("wrote {written} STUN seeds to {}", dir.display());
}
