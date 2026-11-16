// Copyright 2026 litep2p developers
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

//! Pure-parser fuzzing for litep2p's WebRTC transport.
//!
//! Every sub-target here is a synchronous function over untrusted bytes: no sockets, no
//! tokio runtime, no key generation, and no randomness. That makes crashes exactly
//! reproducible from a corpus entry, which is the whole point — the layers above
//! (`fuzz/webrtc-state`, `fuzz/webrtc-datagram`) trade that away for reach.
//!
//! Sub-targets are multiplexed on the first input byte, following the idiom in
//! `fuzz/simple/src/main.rs`. All of them are microsecond-scale, so they compete for
//! fuzzer energy on fair terms; the slower, stateful harnesses live in their own crates
//! precisely so they cannot starve these.
//!
//! Each sub-target returns an [`Outcome`] describing what it managed to do. The fuzz entry
//! point discards it; the corpus test asserts on it. Without that, a seed that bounces off
//! the outermost length check is indistinguishable from one that walks the whole parser,
//! which is how a committed corpus quietly rots into decoration.

use bytes::{Bytes, BytesMut};
use litep2p::{
    crypto::noise::NoiseContext,
    error::NegotiationError,
    multistream_select::{
        webrtc_listener_negotiate, HandshakeResult, ListenerSelectResult, WebRtcDialerState,
    },
    transport::webrtc::{
        schema::webrtc::message::Flag,
        util::{extract_framed_message, WebRtcMessage, MAX_FRAME_SIZE},
    },
    types::protocol::ProtocolName,
};

/// Number of sub-targets multiplexed on `data[0]`.
const NUM_TARGETS: u8 = 6;

/// Protocol names offered by the simulated listener in the negotiation sub-targets.
///
/// Deliberately mixes a realistic Substrate-style name, a short one, and a name that is
/// a prefix of another so the `ls`/fallback matching logic has something to get wrong.
const SUPPORTED_PROTOCOLS: [&str; 3] = ["/ipfs/ping/1.0.0", "/a", "/ipfs/ping/1.0.0-beta"];

/// What a sub-target actually managed to do with one input.
///
/// The fuzz entry point only ever discards this, so several fields are read exclusively by
/// the corpus test. That is the intent, not an oversight.
#[derive(Debug, Default)]
#[cfg_attr(not(test), allow(dead_code))]
struct Outcome {
    /// Frames `extract_framed_message` returned.
    frames: usize,
    /// A permanent framing error was reported, and checked to be genuinely permanent.
    framing_rejected: bool,
    /// A protobuf body decoded into a `WebRtcMessage`.
    decoded: bool,
    /// The listener accepted a protocol.
    negotiated: bool,
    /// The listener echoed the header and asked for the protocol.
    negotiation_pending: bool,
    /// The dialer completed its handshake.
    dialer_succeeded: bool,
    /// A noise identity key parsed, so the signature check was reached.
    noise_key_parsed: bool,
}

impl Outcome {
    /// Fold another outcome in, for accumulating across a corpus.
    #[cfg(test)]
    fn merge(&mut self, other: &Outcome) {
        self.frames += other.frames;
        self.framing_rejected |= other.framing_rejected;
        self.decoded |= other.decoded;
        self.negotiated |= other.negotiated;
        self.negotiation_pending |= other.negotiation_pending;
        self.dialer_succeeded |= other.dialer_succeeded;
        self.noise_key_parsed |= other.noise_key_parsed;
    }
}

fn main() {
    ziggy::fuzz!(|data: &[u8]| {
        let _ = run(data);
    });
}

/// Dispatch one input to its sub-target.
///
/// Shared by the fuzz entry point and the tests, so a test can never exercise a dispatch
/// path that differs from the real one.
fn run(data: &[u8]) -> Outcome {
    let Some((selector, data)) = data.split_first() else {
        return Outcome::default();
    };

    match selector % NUM_TARGETS {
        0 => fuzz_frame_reassembly(data),
        1 => fuzz_message_decode(data),
        2 => fuzz_encode_roundtrip(data),
        3 => fuzz_listener_negotiate(data),
        4 => fuzz_dialer_register_response(data),
        5 => fuzz_noise_handshake_payload(data),
        _ => unreachable!(),
    }
}

/// Split `data` into chunks, each prefixed by a single length byte.
///
/// This hands the fuzzer control over *where* the SCTP message boundaries fall, which is
/// the variable that matters for reassembly bugs. A trailing partial chunk is yielded as
/// whatever bytes remain.
fn chunks(mut data: &[u8]) -> Vec<&[u8]> {
    let mut chunks = Vec::new();

    while let Some((len, rest)) = data.split_first() {
        let len = std::cmp::min(*len as usize, rest.len());
        let (chunk, rest) = rest.split_at(len);
        chunks.push(chunk);
        data = rest;
    }

    chunks
}

/// Map a fuzzer-chosen selector onto an optional wire flag.
fn flag_for(selector: u8) -> Option<Flag> {
    match selector % 5 {
        0 => None,
        1 => Some(Flag::Fin),
        2 => Some(Flag::StopSending),
        3 => Some(Flag::ResetStream),
        4 => Some(Flag::FinAck),
        _ => unreachable!(),
    }
}

/// Derive a payload length that can put the *encoded* frame exactly on `MAX_FRAME_SIZE`.
///
/// Small values map straight through, which exercises the one- and two-byte varint paths.
/// Everything from 128 up lands in a window around the cap, because that boundary is the one
/// `extract_framed_message` turns on and a step size that skips it leaves the harness unable
/// to tell `>` from `>=`. The protobuf overhead is between three and five bytes depending on
/// the flag, so a window of a few dozen bytes below the cap covers every flag choice.
fn payload_len_for(selector: u8) -> usize {
    if selector < 128 {
        selector as usize
    } else {
        MAX_FRAME_SIZE - 16 + (selector - 128) as usize
    }
}

/// Sub-target 0: `extract_framed_message` driven as a *sequence* of chunk arrivals.
///
/// This is the highest-value target in this harness. `extract_framed_message` is only
/// pure in isolation — its real contract is stateful, because `connection.rs::on_inbound_data`
/// owns a long-lived `BytesMut`, appends each SCTP message to it, and re-calls in a loop.
/// (`opening.rs::on_noise_channel_data` shares the buffer but calls once per message rather
/// than draining.) Fuzzing one call against one buffer would miss the entire bug class.
///
/// The oracle is an independent model of the byte stream plus a cursor for how much of it the
/// parser claims to have consumed. That is what makes this more than a panic hunt:
///
/// - `Ok(Some)` must return exactly the bytes that followed the length prefix, and consume
///   exactly the prefix plus the body. A mis-slice that happens to preserve the body length
///   is invisible to length-only assertions.
/// - `Ok(None)` must leave the buffer untouched, because the caller's response is "append
///   more bytes and retry" — so a frame misclassified as incomplete-but-recoverable means the
///   buffer grows forever waiting on bytes that can never help.
/// - `Err` must be reserved for the three conditions no extra bytes can fix. Classifying a
///   merely-incomplete or perfectly legal frame as permanent tears down a live connection on
///   well-formed traffic, which is the mirror image of the wedged-buffer bug and just as bad.
fn fuzz_frame_reassembly(data: &[u8]) -> Outcome {
    let mut outcome = Outcome::default();
    let mut buffer = BytesMut::new();
    let mut stream: Vec<u8> = Vec::new();
    let mut consumed = 0usize;

    for chunk in chunks(data) {
        buffer.extend_from_slice(chunk);
        stream.extend_from_slice(chunk);

        // Drain in a loop, exactly as `connection.rs::on_inbound_data` does: several
        // frames can be coalesced into one SCTP message.
        loop {
            let pending = &stream[consumed..];
            assert_eq!(
                buffer.len(),
                pending.len(),
                "harness bookkeeping desynchronised from the buffer",
            );

            match extract_framed_message(&mut buffer) {
                Ok(Some(body)) => {
                    let (declared, tail) = unsigned_varint::decode::usize(pending)
                        .expect("a frame was extracted, so its length prefix must decode");
                    let varint_len = pending.len() - tail.len();

                    assert!(
                        declared <= MAX_FRAME_SIZE,
                        "extracted a frame declaring {declared} bytes, past MAX_FRAME_SIZE",
                    );
                    assert_eq!(
                        body.len(),
                        declared,
                        "returned body is {} bytes but the prefix declared {declared}",
                        body.len(),
                    );
                    assert_eq!(
                        &body[..],
                        &pending[varint_len..varint_len + declared],
                        "returned body is not the bytes that followed the length prefix",
                    );

                    consumed += varint_len + declared;
                    assert_eq!(
                        buffer.len(),
                        stream.len() - consumed,
                        "extraction should have consumed {} bytes (prefix {varint_len} plus \
                         body {declared})",
                        varint_len + declared,
                    );

                    outcome.frames += 1;
                }
                Ok(None) => {
                    assert_eq!(
                        &buffer[..],
                        pending,
                        "Ok(None) must not consume: callers append and retry, so consuming \
                         here silently drops bytes",
                    );
                    break;
                }
                Err(error) => {
                    match unsigned_varint::decode::usize(pending) {
                        Ok((declared, _)) => assert!(
                            declared > MAX_FRAME_SIZE,
                            "rejected a frame declaring {declared} bytes with {error:?}, but \
                             {declared} is within MAX_FRAME_SIZE, so the frame was either \
                             complete or still arriving",
                        ),
                        Err(unsigned_varint::decode::Error::Insufficient) => panic!(
                            "rejected an incomplete length prefix with {error:?}; more bytes \
                             may still complete it, so this must be Ok(None)",
                        ),
                        // Overflow and NotMinimal can never be fixed by more bytes.
                        Err(_) => {}
                    }

                    assert_eq!(
                        &buffer[..],
                        pending,
                        "a rejected frame must leave the buffer untouched",
                    );

                    outcome.framing_rejected = true;
                    return outcome;
                }
            }
        }

        // Growth bound: the only reason the buffer retains bytes is a frame still being
        // reassembled, and a frame is capped at `MAX_FRAME_SIZE`. The slack covers the
        // varint header plus one in-flight chunk.
        assert!(
            buffer.len() <= MAX_FRAME_SIZE + 512,
            "buffer grew to {} bytes after appending {}; oversized or short frames should \
             have been rejected",
            buffer.len(),
            stream.len(),
        );
    }

    outcome
}

/// Sub-target 1: bare protobuf decode of a `webrtc.Message` body.
fn fuzz_message_decode(data: &[u8]) -> Outcome {
    Outcome {
        // Unknown `flag` integers are logged and coerced to `None` for forward
        // compatibility, so any i32 is a legal input here rather than an error.
        decoded: WebRtcMessage::decode(data).is_ok(),
        ..Outcome::default()
    }
}

/// Sub-target 2: `encode` → `extract_framed_message` → `decode` round-trip.
///
/// The direction is deliberate. Asserting `encode(decode(arbitrary_bytes)) == arbitrary_bytes`
/// would be unsound: `decode` coerces unknown flags to `None`, prost drops unknown fields, and
/// `encode` folds an empty payload to `None`. Starting from a self-encoded frame keeps every
/// equality below true by construction, so a failure is a real disagreement between the two
/// halves of the codec.
///
/// The sharpest check is that the length prefix `encode` computes agrees with the body it
/// actually writes, since that arithmetic is hand-rolled as `ilog2(len) / 7 + 1`.
fn fuzz_encode_roundtrip(data: &[u8]) -> Outcome {
    let Some((flag_selector, data)) = data.split_first() else {
        return Outcome::default();
    };

    let flag = flag_for(*flag_selector);
    let payload_len = data.first().copied().map_or(0, payload_len_for);
    let payload: Vec<u8> = data.iter().copied().cycle().take(payload_len).collect();

    let encoded = WebRtcMessage::encode(payload.clone(), flag);

    // The prefix must describe the body exactly: no slack, no truncation.
    let (declared_len, body) = unsigned_varint::decode::usize(&encoded)
        .expect("encode must emit a decodable varint length prefix");
    assert_eq!(
        declared_len,
        body.len(),
        "declared body length {declared_len} disagrees with the {} bytes emitted",
        body.len(),
    );

    // Only frames within the cap survive de-framing; beyond it, rejection is correct. Both
    // sides of this branch are reachable, and so is the boundary itself.
    if declared_len > MAX_FRAME_SIZE {
        let mut buffer = BytesMut::from(&encoded[..]);
        assert!(
            extract_framed_message(&mut buffer).is_err(),
            "a frame of {declared_len} bytes exceeds MAX_FRAME_SIZE and must be rejected",
        );

        return Outcome {
            framing_rejected: true,
            ..Outcome::default()
        };
    }

    let mut buffer = BytesMut::from(&encoded[..]);
    let extracted = extract_framed_message(&mut buffer)
        .expect("self-encoded frame must de-frame")
        .expect("self-encoded frame is complete");
    assert!(buffer.is_empty(), "de-framing must drain a single-frame buffer");

    let decoded = WebRtcMessage::decode(&extracted).expect("self-encoded frame must decode");
    assert_eq!(decoded.flag, flag, "flag must survive the round-trip");

    // `encode` maps an empty payload to `None` rather than `Some(vec![])`.
    let expected = (!payload.is_empty()).then_some(payload);
    assert_eq!(decoded.payload, expected, "payload must survive the round-trip");

    Outcome {
        frames: 1,
        decoded: true,
        ..Outcome::default()
    }
}

/// Sub-target 3: listener-side multistream-select negotiation.
///
/// Reaches the private `decode_multistream_message` and `protocol::decode`, including the
/// `ls` response loop and its `MAX_PROTOCOLS` cap. Both `header_received` states matter:
/// the accept/reject matrix is keyed on it, so fixing it would leave half the states
/// unreachable.
///
/// The invariant worth asserting is protocol confusion. Accepting a name that was never
/// offered would hand a peer a substream on a protocol this node does not speak, and with an
/// empty offer list `Accepted` must be impossible at all — both of which the containment
/// check below covers.
fn fuzz_listener_negotiate(data: &[u8]) -> Outcome {
    let Some((flags, data)) = data.split_first() else {
        return Outcome::default();
    };

    let header_received = flags & 1 == 1;

    // Vary how many protocols are on offer: an empty set must reject cleanly rather than
    // index into nothing.
    let protocol_count = (flags >> 1) as usize % (SUPPORTED_PROTOCOLS.len() + 1);
    let supported = SUPPORTED_PROTOCOLS[..protocol_count]
        .iter()
        .map(|name| ProtocolName::from(*name))
        .collect::<Vec<_>>();

    let mut outcome = Outcome::default();

    match webrtc_listener_negotiate(
        supported.clone(),
        Bytes::copy_from_slice(data),
        header_received,
    ) {
        Ok(ListenerSelectResult::Accepted { protocol, .. }) => {
            assert!(
                supported.contains(&protocol),
                "accepted {protocol}, which is not one of the {protocol_count} offered \
                 protocols {supported:?}",
            );
            outcome.negotiated = true;
        }
        Ok(ListenerSelectResult::PendingProtocol { .. }) => outcome.negotiation_pending = true,
        Ok(ListenerSelectResult::Rejected { .. }) | Err(_) => {}
    }

    outcome
}

/// Sub-target 4: dialer-side response parsing.
///
/// `register_response` walks a run of length-prefixed messages within one payload using
/// `advance`/`split_to`, and `propose_next_fallback` mutates the state machine mid-stream.
/// Feeding several responses into one state keeps the fuzzer in the multi-round paths
/// instead of restarting from `WaitingResponse` every time.
///
/// The dialer half of protocol confusion is a listener that echoes a protocol the dialer
/// never proposed. `propose` starts on the main protocol and each accepted fallback steps one
/// place down `SUPPORTED_PROTOCOLS` in order, so the current proposal is always known and the
/// success value can be checked against it.
fn fuzz_dialer_register_response(data: &[u8]) -> Outcome {
    let Some((count, data)) = data.split_first() else {
        return Outcome::default();
    };

    let fallbacks = SUPPORTED_PROTOCOLS[1..]
        .iter()
        .map(|name| ProtocolName::from(*name))
        .collect::<Vec<_>>();

    let Ok((mut state, _proposal)) =
        WebRtcDialerState::propose(ProtocolName::from(SUPPORTED_PROTOCOLS[0]), fallbacks)
    else {
        return Outcome::default();
    };

    let mut proposed = 0usize;
    let mut outcome = Outcome::default();

    for chunk in chunks(data).into_iter().take(*count as usize % 8 + 1) {
        match state.register_response(chunk.to_vec()) {
            // A rejection sends the dialer looking for the next fallback, which rewrites
            // `self.protocol` while leaving the handshake state alone: the header has
            // already been exchanged, so only the protocol needs re-proposing.
            Ok(HandshakeResult::Rejected) => {
                if matches!(state.propose_next_fallback(), Ok(None) | Err(_)) {
                    return outcome;
                }
                proposed += 1;
            }
            Ok(HandshakeResult::Succeeded(protocol)) => {
                assert_eq!(
                    &*protocol,
                    SUPPORTED_PROTOCOLS[proposed],
                    "handshake succeeded on a protocol the dialer never proposed",
                );
                outcome.dialer_succeeded = true;
                return outcome;
            }
            Ok(HandshakeResult::NotReady) => {}
            Err(_) => return outcome,
        }
    }

    outcome
}

/// Sub-target 5: the WebRTC Noise identity payload, with the crypto removed.
///
/// `get_remote_peer_id` only reaches this parser once `snow` has decrypted the second
/// handshake message, which fuzzer-random bytes essentially never manage — so pointing a
/// fuzzer at the full function burns every cycle on the two-byte length prefix. This
/// drives the tail directly: protobuf decode, `RemotePublicKey::from_protobuf_encoding`,
/// `PeerId::from_public_key_protobuf`, and the ed25519 signature check over
/// `"noise-libp2p-static-key:" ++ dh_remote_pubkey`.
///
/// `Ok` is unreachable by construction, since it needs a signature no fuzzer will forge, so
/// there is no success path to assert on. `BadSignature` is the useful marker: it means the
/// identity key parsed and the run got as far as verification, which is the depth the corpus
/// has to keep reaching.
fn fuzz_noise_handshake_payload(data: &[u8]) -> Outcome {
    let Some((split, data)) = data.split_first() else {
        return Outcome::default();
    };

    // The remote DH public key is a 32-byte X25519 key in practice, but it is
    // attacker-influenced, so let the fuzzer choose the length as well.
    let split = std::cmp::min(*split as usize, data.len());
    let (dh_remote_pubkey, payload) = data.split_at(split);

    match NoiseContext::fuzz_parse_handshake_payload(payload, dh_remote_pubkey) {
        Ok(_) | Err(NegotiationError::BadSignature) => Outcome {
            noise_key_parsed: true,
            ..Outcome::default()
        },
        Err(_) => Outcome::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift64* so the sweep below is reproducible without a dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| (self.next() >> 24) as u8).collect()
        }
    }

    /// The harness must not panic on arbitrary input — only on a genuine invariant
    /// violation in litep2p. A new harness that asserts something subtly untrue reports
    /// its own bug as a finding on the first run, so sweep every sub-target broadly
    /// before trusting any crash it produces.
    #[test]
    fn sub_targets_survive_random_input() {
        let mut rng = Rng(0x5eed_0d1a_bcde_f001);

        for target in 0..NUM_TARGETS {
            for len in 0..192usize {
                let mut input = vec![target];
                input.extend_from_slice(&rng.bytes(len));
                let _ = run(&input);
            }
        }
    }

    /// Well-formed frames must flow through the reassembly sub-target at every fragmentation
    /// boundary, and must actually be *extracted*. Asserting the frame count is what stops
    /// this passing vacuously: without it, a frame taking the `Err` path would return
    /// silently and the test would still be green.
    #[test]
    fn reassembly_accepts_valid_frames_at_every_split() {
        for payload_len in [0usize, 1, 5, 127, 128, 200] {
            let frame = WebRtcMessage::encode(vec![0xa5; payload_len], Some(Flag::Fin));

            for split in 0..=frame.len() {
                let (head, tail) = frame.split_at(split);

                // Chunk lengths are single bytes, so only exercise splits that fit.
                if head.len() > u8::MAX as usize || tail.len() > u8::MAX as usize {
                    continue;
                }

                let mut input = vec![head.len() as u8];
                input.extend_from_slice(head);
                input.push(tail.len() as u8);
                input.extend_from_slice(tail);

                let outcome = fuzz_frame_reassembly(&input);
                assert_eq!(
                    outcome.frames, 1,
                    "a well-formed {payload_len}-byte frame split at {split} must extract \
                     exactly once",
                );
                assert!(
                    !outcome.framing_rejected,
                    "a well-formed frame split at {split} must not be rejected",
                );
            }
        }
    }

    /// The adversarial inputs already covered by `util.rs`'s own tests belong in the seed
    /// corpus, not as fresh assertions. Confirm each reaches the sub-target and is
    /// classified the way the parser's contract says it should be.
    #[test]
    fn known_adversarial_seeds_are_classified() {
        let oversized = MAX_FRAME_SIZE + 1;
        let mut varint_buf = unsigned_varint::encode::usize_buffer();
        let oversized_varint = unsigned_varint::encode::usize(oversized, &mut varint_buf).to_vec();

        let seeds: Vec<(Vec<u8>, bool)> = vec![
            (vec![0x80; 11], true),      // overlong varint: accumulates past usize
            (vec![0x80, 0x00], true),    // non-minimal encoding of zero
            (oversized_varint, true),    // declares a body past MAX_FRAME_SIZE
            (vec![0x00], false),         // legal zero-length frame
        ];

        for (seed, permanent) in seeds {
            let mut input = vec![seed.len() as u8];
            input.extend_from_slice(&seed);

            let outcome = fuzz_frame_reassembly(&input);
            assert_eq!(
                outcome.framing_rejected, permanent,
                "{seed:02x?} should {} be a permanent rejection",
                if permanent { "" } else { "not" },
            );

            let _ = fuzz_message_decode(&seed);
        }
    }

    /// The `MAX_FRAME_SIZE` boundary must be reachable from a fuzzer input, not merely
    /// approached. A payload step size that skips it leaves the round-trip sub-target unable
    /// to tell `>` from `>=` in `extract_framed_message`'s size check, which is exactly the
    /// kind of off-by-one this harness exists to catch.
    #[test]
    fn roundtrip_reaches_the_frame_size_boundary() {
        for flag_selector in 0..5u8 {
            let mut declared = std::collections::BTreeSet::new();

            for len_byte in 128..=255u8 {
                let input = [flag_selector, len_byte, 0xa5];
                let _ = fuzz_encode_roundtrip(&input);

                // Recompute what that input encodes to, so the assertion is about the
                // declared frame length rather than about the payload length.
                let payload: Vec<u8> = input[1..]
                    .iter()
                    .copied()
                    .cycle()
                    .take(payload_len_for(len_byte))
                    .collect();
                let encoded = WebRtcMessage::encode(payload, flag_for(flag_selector));
                let (len, _) = unsigned_varint::decode::usize(&encoded).expect("valid prefix");
                declared.insert(len);
            }

            for expected in [MAX_FRAME_SIZE - 1, MAX_FRAME_SIZE, MAX_FRAME_SIZE + 1] {
                assert!(
                    declared.contains(&expected),
                    "flag selector {flag_selector} cannot produce a frame declaring \
                     {expected} bytes; the boundary is unreachable",
                );
            }
        }
    }

    /// The round-trip sub-target is the one carrying real equality assertions, so verify
    /// it agrees with itself across every flag and a spread of payload sizes, including the
    /// whole window around the frame cap.
    #[test]
    fn encode_roundtrip_holds_for_all_flags() {
        for flag_selector in 0..5u8 {
            for len_byte in [0u8, 1, 8, 64, 127].iter().copied().chain(128..=255u8) {
                let _ = fuzz_encode_roundtrip(&[flag_selector, len_byte, 0xde, 0xad, 0xbe, 0xef]);
            }
        }
    }

    /// Every committed seed must replay through the real dispatch, and the corpus as a whole
    /// must reach every state worth seeding.
    ///
    /// This reads the seed *files* rather than rebuilding inputs by hand. A test that
    /// constructs its own bytes cannot notice that a committed seed stopped reaching past the
    /// outermost guard, which is precisely the failure it is supposed to catch.
    #[test]
    fn committed_corpus_reaches_meaningful_states() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus");
        let entries = std::fs::read_dir(&dir).expect("corpus directory to exist");

        let mut total = Outcome::default();
        let mut count = 0;

        for entry in entries {
            let path = entry.expect("readable entry").path();
            let data = std::fs::read(&path).expect("readable seed");
            total.merge(&run(&data));
            count += 1;
        }

        assert!(count >= 20, "expected the generated corpus, found {count} seeds");
        assert!(total.frames > 0, "no seed extracted a single frame");
        assert!(
            total.framing_rejected,
            "no seed reached a permanent framing rejection, so the reject path is unseeded",
        );
        assert!(total.decoded, "no seed decoded a protobuf body");
        assert!(
            total.negotiated,
            "no seed reached an accepted negotiation; check that the negotiate seeds offer a \
             non-empty protocol list, since the count lives in the high bits of the flags byte",
        );
        assert!(
            total.negotiation_pending,
            "no seed reached the header-echo path, which is half the accept/reject matrix",
        );
        assert!(total.dialer_succeeded, "no seed completed a dialer handshake");
        assert!(
            total.noise_key_parsed,
            "no seed's identity key parsed, so the signature check and the peer-id derivation \
             behind it are both unreached",
        );
    }
}
