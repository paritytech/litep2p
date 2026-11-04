// Copyright 2026 litep2p developers
//
// Licensed under the same terms as the rest of this repository; see `src/main.rs`.

//! Seed corpus generator for the `webrtc-state` harness.
//!
//! Run with `cargo run --bin gen_seeds -- corpus/` to (re)populate the corpus.
//!
//! Seeds matter more here than for any other harness in this tree. The input is a
//! bincode-encoded `Script`, and a fuzzer starting from nothing will essentially never
//! synthesise one that decodes — so without a corpus the harness silently does nothing on
//! every iteration while still reporting execs. Each seed below is a protocol sequence
//! worth mutating away from, drawn from the state transitions `substream.rs` implements.

// `script` is shared with `main.rs`, which uses the parts the generator does not.
#[allow(dead_code)]
mod script;

use litep2p::transport::webrtc::{
    schema::webrtc::message::Flag,
    util::{WebRtcMessage, MAX_FRAME_SIZE},
};
use script::{
    bincode_options, ConnectionOp, ConnectionScript, Input, Op, Script, FIN, FIN_ACK,
    MAX_OP_BYTES, RESET_STREAM, STOP_SENDING,
};
use std::{fs, path::PathBuf};

/// Frame bytes that will never complete: a varint declaring 300 bytes, with two supplied.
///
/// A channel fed this parks mid-reassembly and holds its buffer indefinitely, which is the
/// state the aggregate-memory question is about.
const INCOMPLETE_FRAME: &[u8] = &[0xac, 0x02, 0xaa, 0xbb];

/// A varint declaring `MAX_FRAME_SIZE + 1` (16385) bytes.
///
/// `extract_framed_message` rejects this the moment it decodes, and — this is the part that
/// matters — returns the error *without consuming it*, so the bytes stay at the head of the
/// reassembly buffer forever.
const OVERSIZED_FRAME: &[u8] = &[0x81, 0x80, 0x01];

fn inbound(payload: &[u8], flag: Option<u8>) -> Op {
    Op::Inbound {
        payload: payload.to_vec(),
        flag,
    }
}

fn write(data: &[u8]) -> Op {
    Op::Write {
        data: data.to_vec(),
    }
}

/// Connection-level seeds: channel interleaving and reassembly.
fn connection_seeds() -> Vec<(&'static str, ConnectionScript)> {
    // A complete multistream-select proposal, framed the way the wire carries it.
    //
    // This must go through `WebRtcMessage::encode`, not a hand-written length byte. The
    // frame *body* is a protobuf `webrtc.Message` whose `message` field holds the
    // multistream bytes; handing the raw multistream bytes over as the body makes
    // `WebRtcMessage::decode` fail on the first field tag, and negotiation is never entered
    // at all.
    let proposal = WebRtcMessage::encode(
        [
            b"\x13/multistream/1.0.0\n".to_vec(),
            b"\x11/ipfs/ping/1.0.0\n".to_vec(),
        ]
        .concat(),
        None,
    );

    vec![
        (
            // One channel, one complete negotiation attempt.
            "conn-single-channel-negotiate",
            ConnectionScript {
                ops: vec![
                    ConnectionOp::OpenChannel,
                    ConnectionOp::Inbound {
                        channel: 0,
                        data: proposal.clone(),
                    },
                ],
            },
        ),
        (
            // The aggregate-memory shape: many channels, each parked mid-reassembly.
            // Individually each buffer is well under MAX_FRAME_SIZE; the sum is what
            // nothing in litep2p caps.
            "conn-many-channels-incomplete-frames",
            ConnectionScript {
                ops: (0..16)
                    .flat_map(|channel| {
                        vec![
                            ConnectionOp::OpenChannel,
                            ConnectionOp::Inbound {
                                channel,
                                data: INCOMPLETE_FRAME.to_vec(),
                            },
                        ]
                    })
                    .collect(),
            },
        ),
        (
            // Close must drop the reassembly buffer; if it does not, this leaks per cycle.
            "conn-open-close-cycle",
            ConnectionScript {
                ops: (0..8)
                    .flat_map(|channel| {
                        vec![
                            ConnectionOp::OpenChannel,
                            ConnectionOp::Inbound {
                                channel,
                                data: INCOMPLETE_FRAME.to_vec(),
                            },
                            ConnectionOp::CloseChannel { channel },
                        ]
                    })
                    .collect(),
            },
        ),
        (
            // A frame dribbled in one byte at a time across two interleaved channels.
            "conn-interleaved-fragments",
            ConnectionScript {
                ops: {
                    let mut ops = vec![ConnectionOp::OpenChannel, ConnectionOp::OpenChannel];
                    for byte in proposal.iter() {
                        ops.push(ConnectionOp::Inbound {
                            channel: 0,
                            data: vec![*byte],
                        });
                        ops.push(ConnectionOp::Inbound {
                            channel: 1,
                            data: vec![*byte],
                        });
                    }
                    ops
                },
            },
        ),
        (
            // Data for channels that were never opened, and closes for the same.
            "conn-unknown-channels",
            ConnectionScript {
                ops: vec![
                    ConnectionOp::Inbound {
                        channel: 200,
                        data: proposal.clone(),
                    },
                    ConnectionOp::CloseChannel { channel: 200 },
                    ConnectionOp::OpenChannel,
                    ConnectionOp::CloseChannel { channel: 0 },
                    ConnectionOp::Inbound {
                        channel: 0,
                        data: proposal,
                    },
                ],
            },
        ),
        (
            // An oversized frame declaration must be rejected, not buffered toward.
            "conn-oversized-frame",
            ConnectionScript {
                ops: vec![
                    ConnectionOp::OpenChannel,
                    ConnectionOp::Inbound {
                        channel: 0,
                        data: OVERSIZED_FRAME.to_vec(),
                    },
                ],
            },
        ),
        (
            // The `Open` state, which nothing else in this harness can reach: a negotiated
            // channel, a framed payload that must arrive at the local end, a poll of the
            // handle set, then a FIN that makes the handle emit FIN_ACK.
            //
            // The FIN_ACK cannot be written out in this scaffold, so forwarding it fails and
            // the channel closes. That failure path is itself worth seeding.
            "conn-open-substream-traffic",
            ConnectionScript {
                ops: vec![
                    ConnectionOp::OpenNegotiated,
                    ConnectionOp::Inbound {
                        channel: 0,
                        data: WebRtcMessage::encode(b"payload".to_vec(), None),
                    },
                    ConnectionOp::ReadSubstream { channel: 0, len: 64 },
                    ConnectionOp::PollHandles,
                    ConnectionOp::Inbound {
                        channel: 0,
                        data: WebRtcMessage::encode(Vec::new(), Some(Flag::Fin)),
                    },
                    ConnectionOp::PollHandles,
                    ConnectionOp::ReadSubstream { channel: 0, len: 64 },
                ],
            },
        ),
        (
            // Several open channels polled together, then one closed mid-rotation. This is the
            // `SubstreamHandleSet` round-robin: the persistent `index`, the `pending` skip and
            // the `swap_remove` that reorders the map underneath it.
            "conn-open-handle-set-rotation",
            ConnectionScript {
                ops: {
                    let mut ops = vec![ConnectionOp::OpenNegotiated; 4];

                    for channel in 0..4u8 {
                        ops.push(ConnectionOp::Inbound {
                            channel,
                            data: WebRtcMessage::encode(b"rotate".to_vec(), None),
                        });
                    }
                    ops.extend(std::iter::repeat_n(ConnectionOp::PollHandles, 4));
                    ops.push(ConnectionOp::CloseChannel { channel: 1 });
                    ops.extend(std::iter::repeat_n(ConnectionOp::PollHandles, 4));

                    ops
                },
            },
        ),
        (
            // Data delivered after the channel is closed. `on_inbound_data` must drop it
            // without creating a reassembly buffer that nothing would ever reclaim.
            "conn-data-after-close",
            ConnectionScript {
                ops: vec![
                    ConnectionOp::OpenChannel,
                    ConnectionOp::CloseChannel { channel: 0 },
                    ConnectionOp::Inbound {
                        channel: 0,
                        data: INCOMPLETE_FRAME.to_vec(),
                    },
                    ConnectionOp::Inbound {
                        channel: 0,
                        data: WebRtcMessage::encode(b"after-close".to_vec(), None),
                    },
                ],
            },
        ),
        (
            // The same rejection, followed by more data on the same channel — which is what
            // a peer that keeps writing looks like.
            //
            // The rejected varint is never consumed and the channel is never closed, so
            // every byte here is appended to a buffer that can no longer be parsed. One
            // follow-up op is enough: 3 bytes of rejected varint plus MAX_OP_BYTES already
            // carries the buffer past MAX_FRAME_SIZE + BUFFER_SLACK.
            //
            // This seed reproduces an open defect, so the harness's corpus test expects it
            // to trip the buffer assertion. See the review notes on `on_inbound_data`.
            "conn-wedged-buffer-growth",
            ConnectionScript {
                ops: vec![
                    ConnectionOp::OpenChannel,
                    ConnectionOp::Inbound {
                        channel: 0,
                        data: OVERSIZED_FRAME.to_vec(),
                    },
                    ConnectionOp::Inbound {
                        channel: 0,
                        data: vec![0x5a; MAX_OP_BYTES],
                    },
                ],
            },
        ),
    ]
}

fn seeds() -> Vec<(&'static str, Script)> {
    vec![
        (
            // The nominal close: write, half-close, peer acknowledges, peer closes too.
            "full-fin-handshake",
            Script {
                ops: vec![
                    write(b"request"),
                    Op::PollOutbound,
                    Op::Shutdown,
                    Op::PollOutbound,
                    inbound(b"", Some(FIN_ACK)),
                    inbound(b"response", Some(FIN)),
                    Op::Read { len: 4096 },
                    Op::PollOutbound,
                ],
            },
        ),
        (
            // Both sides close at once — the ordering that classically breaks half-close
            // implementations.
            "simultaneous-close",
            Script {
                ops: vec![
                    Op::Shutdown,
                    inbound(b"", Some(FIN)),
                    Op::PollOutbound,
                    inbound(b"", Some(FIN_ACK)),
                    Op::PollOutbound,
                ],
            },
        ),
        (
            // FIN sent, acknowledgement never arrives: the 10s timer must force a reset.
            "fin-ack-timeout",
            Script {
                ops: vec![
                    Op::Shutdown,
                    Op::PollOutbound,
                    Op::AdvanceTime,
                    Op::PollOutbound,
                    Op::PollOutbound,
                ],
            },
        ),
        (
            // FIN_ACK with no FIN sent is a protocol violation and must reset. The ops after
            // it still run: the peer is gone, but the local protocol keeps its `Substream`
            // and has to observe the failure.
            "unexpected-fin-ack",
            Script {
                ops: vec![
                    inbound(b"", Some(FIN_ACK)),
                    Op::PollOutbound,
                    Op::Read { len: 16 },
                    write(b"after-reset"),
                    Op::PollOutbound,
                ],
            },
        ),
        (
            // Duplicate FIN must be absorbed, not double-processed. A second FIN_ACK going
            // out would be the observable failure.
            "duplicate-fin",
            Script {
                ops: vec![
                    inbound(b"", Some(FIN)),
                    inbound(b"", Some(FIN)),
                    Op::PollOutbound,
                    Op::PollOutbound,
                ],
            },
        ),
        (
            // Payload after FIN is a spec violation the reader must discard.
            "payload-after-fin",
            Script {
                ops: vec![
                    inbound(b"first", Some(FIN)),
                    inbound(b"after-fin", None),
                    Op::Read { len: 4096 },
                    Op::PollOutbound,
                ],
            },
        ),
        (
            // STOP_SENDING must halt the writer without tearing the read half down, then
            // the half-close still has to complete: the write fails, and the *following*
            // ops are what carry `StopSending` through `poll_half_close` into FIN.
            "stop-sending-then-write",
            Script {
                ops: vec![
                    write(b"before"),
                    Op::PollOutbound,
                    inbound(b"", Some(STOP_SENDING)),
                    write(b"after"),
                    Op::PollOutbound,
                    Op::Shutdown,
                    Op::PollOutbound,
                ],
            },
        ),
        (
            // A peer reset discards everything, including already-buffered inbound data.
            "peer-reset-mid-stream",
            Script {
                ops: vec![
                    inbound(b"buffered", None),
                    inbound(b"", Some(RESET_STREAM)),
                    inbound(b"after-reset", None),
                    Op::Read { len: 4096 },
                    Op::PollOutbound,
                    write(b"after-reset"),
                    Op::PollOutbound,
                ],
            },
        ),
        (
            // Inbound capacity is 256 messages; overflow is treated as flooding and reset
            // rather than as a slow reader.
            "inbound-flood",
            Script {
                ops: (0..300).map(|_| inbound(b"flood", None)).collect(),
            },
        ),
        (
            // Writes larger than MAX_FRAME_SIZE must be chunked across several messages.
            // One byte over the frame size is the whole point: it yields a full chunk plus a
            // one-byte tail, which is the boundary `poll_next` gets wrong if it gets it wrong
            // at all. A larger write only adds identical full chunks, and a fat seed slows
            // every mutation AFL derives from it.
            "large-write-chunking",
            Script {
                ops: vec![
                    write(&vec![0xab; MAX_FRAME_SIZE + 1]),
                    Op::PollOutbound,
                    Op::PollOutbound,
                    Op::PollOutbound,
                    Op::PollOutbound,
                ],
            },
        ),
        (
            // Dropping the substream with the handle still live closes the outbound sender,
            // which drives `poll_half_close`.
            "drop-substream-mid-flight",
            Script {
                ops: vec![
                    write(b"pending"),
                    inbound(b"pending", None),
                    Op::DropSubstream,
                    Op::PollOutbound,
                    Op::PollOutbound,
                ],
            },
        ),
        (
            // Interleaved traffic in both directions, no close at all.
            "bidirectional-traffic",
            Script {
                ops: vec![
                    write(b"a"),
                    inbound(b"b", None),
                    Op::PollOutbound,
                    Op::Read { len: 1 },
                    write(b"c"),
                    inbound(b"d", None),
                    Op::PollOutbound,
                    Op::Read { len: 1 },
                ],
            },
        ),
    ]
}

fn main() {
    use bincode::Options;

    let dir: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "corpus".to_string())
        .into();
    fs::create_dir_all(&dir).expect("corpus directory to be creatable");

    let inputs = seeds()
        .into_iter()
        .map(|(name, script)| (name, Input::Substream(script)))
        .chain(
            connection_seeds()
                .into_iter()
                .map(|(name, script)| (name, Input::Connection(script))),
        );

    let mut written = 0;
    for (name, input) in inputs {
        // `bincode_options` is shared with `main.rs`, so the encoder and decoder cannot
        // disagree about the format.
        let bytes = bincode_options().serialize(&input).expect("input to serialise");

        // Round-trip check: a seed that does not decode is worse than no seed at all,
        // because it looks like corpus coverage while contributing nothing.
        bincode_options()
            .deserialize::<Input>(&bytes)
            .expect("seed must decode with the harness's own options");

        fs::write(dir.join(name), &bytes).expect("seed to be writable");
        written += 1;
    }

    eprintln!("wrote {written} seeds to {}", dir.display());
}
