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

//! Structure-aware fuzzing of the WebRTC state machines.
//!
//! `fuzz/webrtc-codec` fuzzes stateless parsers; this harness fuzzes the parts that have
//! memory. Their bugs are *orderings*, not malformed bytes, so the fuzzer input is decoded
//! into a script of operations rather than fed in as a byte blob.
//!
//! Two layers share the corpus, matching how they nest in production:
//!
//! - [`Input::Substream`] drives `SubstreamHandle`, which implements the libp2p WebRTC
//!   half-close protocol — `FIN`/`FIN_ACK`/`STOP_SENDING`/`RESET_STREAM` across three
//!   shared atomic states, a bounded inbound channel, and a 10-second FIN_ACK timer. Every
//!   outbound message is checked against [`Protocol`], because "did not panic" is not an
//!   oracle for a state machine: `substream.rs` carries no `debug_assert!`, so a wrong flag
//!   or a lost FIN_ACK would otherwise pass silently.
//! - [`Input::Connection`] drives `WebRtcConnection`'s inbound path: per-channel frame
//!   reassembly and the channel state machine, across interleaved channels. The oracle is
//!   [`assert_buffers_bounded`].
//!
//! Determinism comes from `Substream::new()` needing no socket, keypair or `Rtc`, from
//! every substream operation being driven with `now_or_never()` so nothing blocks, and from
//! virtual time being paused so the FIN_ACK timeout is reachable without waiting.
//!
//! The connection layer is bounded by having no DTLS handshake, so writes cannot succeed
//! and negotiation never completes; see `FuzzConnection`'s documentation for exactly what
//! that does and does not reach.
//!
//! # Errors are not stop signals
//!
//! Both layers deliberately keep replaying after an operation fails, because that is what
//! production does. `run_event_loop`'s `Event::ChannelData` arm logs an `on_inbound_data`
//! error at debug level and continues; a harness that stops on the first error could never
//! observe what follows that error. Since the wedged-buffer fix, `on_inbound_data` treats a
//! permanent framing error as fatal for the channel: it drops the reassembly buffer and
//! closes the channel, so later data is discarded instead of accumulating.

// `script` is shared with `gen_seeds.rs`, which uses the parts the harness does not.
#[allow(dead_code)]
mod script;

use bincode::Options;
use futures::{FutureExt, StreamExt};
use litep2p::{
    transport::webrtc::{
        schema::webrtc::message::Flag,
        substream::{Message, Substream, SubstreamHandle},
        util::{WebRtcMessage, MAX_FRAME_SIZE},
        FuzzConnection,
    },
    types::protocol::ProtocolName,
};
use script::{
    bincode_options, ConnectionOp, ConnectionScript, Input, Op, Script, MAX_OPS, MAX_OP_BYTES,
};
use std::sync::OnceLock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Protocols the fuzzed connection advertises.
const SUPPORTED_PROTOCOLS: [&str; 2] = ["/ipfs/ping/1.0.0", "/fuzz/1.0.0"];

/// Channels a connection script may open.
///
/// High enough that the sum across channels is a quantity worth watching: a per-channel
/// regression shows up as megabytes of retained buffers rather than kilobytes. Bounded only
/// so one input cannot open unbounded str0m allocations.
const MAX_CHANNELS: usize = 256;

/// Slack over `MAX_FRAME_SIZE` allowed in a single reassembly buffer.
///
/// A buffer only retains bytes while a frame is mid-reassembly, and no frame may declare
/// more than `MAX_FRAME_SIZE`. The slack covers the three-byte varint header plus one
/// in-flight SCTP message.
const BUFFER_SLACK: usize = 512;

/// Aggregate reassembly budget for one connection.
///
/// This follows from the per-buffer bound times the channel cap, so today it cannot fail on
/// its own. It is asserted separately because nothing in litep2p enforces a global bound:
/// if the per-buffer cap is ever relaxed, this is the assertion that notices the total went
/// unbounded with it.
const MAX_BUFFERED_BYTES: usize = MAX_CHANNELS * (MAX_FRAME_SIZE + BUFFER_SLACK);

// Keep the script-level byte cap above the frame size, or an oversized frame becomes
// inexpressible and the rejection path goes unfuzzed.
const _: () = assert!(MAX_OP_BYTES > MAX_FRAME_SIZE);

/// One tokio runtime for the whole process.
///
/// Under AFL's persistent mode the closure below runs thousands of times per fork, so
/// building a runtime per input means an epoll instance and a timer driver per input. The
/// clock stays paused, so `Op::AdvanceTime` still costs no wall-clock time; virtual time
/// accumulates across iterations, which nothing here reads as an absolute.
static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap_or_else(|error| harness_failure(&format!("tokio runtime failed to build: {error}")))
    })
}

/// Stop with a message that cannot be mistaken for a finding.
///
/// A panic aborts and AFL files the current input as a crash. A plain non-zero exit is not recorded
/// as one, so a harness or environment failure (a failed socket bind, a runtime that will not
/// build) stays out of the crash directory instead of masquerading as a litep2p bug. Mirrors the
/// same helper in the `webrtc-datagram` harness.
fn harness_failure(what: &str) -> ! {
    eprintln!("webrtc-state: harness failure: {what}");
    eprintln!("this is a harness or environment problem, not a finding in litep2p");
    std::process::exit(70);
}

fn main() {
    ziggy::fuzz!(|data: &[u8]| {
        let Ok(input) = bincode_options().deserialize::<Input>(data) else {
            return;
        };

        match input {
            Input::Substream(script) => runtime().block_on(replay(script)),
            Input::Connection(script) => runtime().block_on(replay_connection(script)),
        }
    });
}

/// Replay a connection-level script against `WebRtcConnection`'s inbound path.
///
/// The payoff here is *interleaving*: reassembly buffers are per-channel, each capped at
/// `MAX_FRAME_SIZE`, with nothing capping their sum. Many channels each parked
/// mid-reassembly is the shape that question takes, and it needs no successful write —
/// which matters, because the scaffold has no DTLS handshake and so cannot write at all.
async fn replay_connection(script: ConnectionScript) {
    let protocols = SUPPORTED_PROTOCOLS.iter().map(|name| ProtocolName::from(*name)).collect();

    // A setup failure must be loud but must not look like a finding. Returning here would leave
    // every connection iteration a silent no-op while the exec counter climbed (a campaign that
    // looks healthy and tests nothing); panicking would file the input as a crash. Exit 70 instead,
    // so a bad environment stays out of the crash directory.
    let mut connection = match FuzzConnection::new(protocols).await {
        Ok(connection) => connection,
        Err(error) => harness_failure(&format!("connection scaffold failed to build: {error}")),
    };

    for op in script.ops.into_iter().take(MAX_OPS) {
        match op {
            ConnectionOp::OpenChannel => {
                if connection.channel_count() < MAX_CHANNELS {
                    let _ = connection.open_channel().await;
                }
            }
            ConnectionOp::OpenNegotiated => {
                if connection.channel_count() < MAX_CHANNELS {
                    let protocol = ProtocolName::from(SUPPORTED_PROTOCOLS[0]);
                    let _ = connection.open_negotiated_channel(protocol);
                }
            }
            ConnectionOp::Inbound { channel, mut data } => {
                data.truncate(MAX_OP_BYTES);

                // Ignored on purpose. See the module docs: production logs this and carries
                // on, so stopping here would hide the buffer growth that behaviour allows.
                let _ = connection.inbound(channel as usize, data).await;
            }
            ConnectionOp::CloseChannel { channel } => {
                let _ = connection.close_channel(channel as usize).await;
            }
            ConnectionOp::PollHandles => {
                connection.poll_handles();
            }
            ConnectionOp::ReadSubstream { channel, len } => {
                let _ = connection.read_substream(channel as usize, len as usize);
            }
        }

        assert_buffers_bounded(&connection);
    }
}

/// Check every reassembly-memory bound after an operation.
fn assert_buffers_bounded(connection: &FuzzConnection) {
    let channels = connection.channel_count();

    let largest = connection.max_buffered_bytes();
    assert!(
        largest <= MAX_FRAME_SIZE + BUFFER_SLACK,
        "a single reassembly buffer grew to {largest} bytes, past the \
         {MAX_FRAME_SIZE}-byte frame cap; a buffer only retains bytes while a frame is \
         mid-reassembly, so either an oversized frame was accepted or a rejected one was \
         left in place for the next append to grow",
    );

    // A buffer for a channel that was never opened can never be reclaimed, because
    // `on_channel_closed` only runs for channels litep2p knows about.
    let count = connection.buffer_count();
    assert!(
        count <= channels,
        "{count} reassembly buffers exist but only {channels} channels were opened; a \
         buffer was created for a channel with no state and nothing will reclaim it",
    );

    let total = connection.buffered_bytes();
    assert!(
        total <= MAX_BUFFERED_BYTES,
        "reassembly buffers hold {total} bytes across {count} channels, past the \
         {MAX_BUFFERED_BYTES}-byte aggregate budget",
    );
}

/// Map a fuzzer-chosen index onto a wire flag.
fn flag(index: u8) -> Flag {
    match index % 4 {
        0 => Flag::Fin,
        1 => Flag::StopSending,
        2 => Flag::ResetStream,
        3 => Flag::FinAck,
        _ => unreachable!(),
    }
}

/// Invariants the outbound message stream must satisfy.
///
/// Every rule here is read off `substream.rs`'s own state machine rather than from the
/// libp2p spec, so a violation is a litep2p bug and not a harness opinion. The relevant
/// facts, all in `poll_next` and `poll_half_close`:
///
/// - `FIN_ACK` is emitted only while `reader_state` is `Fin`, which only `on_message` sets,
///   and emitting it advances the state to `FinAck` with no path back.
/// - `FIN` is emitted only from `WriterState::Open` or `StopSending`, and emitting it
///   advances to `Fin`, from which the only exit is `FinAck`.
/// - both `RESET_STREAM` sites set `ChannelState::Reset` as they emit, and `Reset` makes
///   every later poll return `None`.
/// - once the writer reaches `Fin`, `poll_next` stops polling the outbound channel, so no
///   payload can follow a `FIN`.
/// - `STOP_SENDING` is a flag the peer sends to us; litep2p never emits it.
#[derive(Default)]
struct Protocol {
    /// A `FIN` was handed to the handle. Recorded even when `on_message` discards it, which
    /// only ever makes the `FIN_ACK` rule more permissive.
    peer_fin_delivered: bool,
    /// A `FIN` was emitted.
    fin_sent: bool,
    /// A `FIN_ACK` was emitted.
    fin_ack_sent: bool,
    /// A `RESET_STREAM` was emitted.
    reset_sent: bool,
}

impl Protocol {
    /// Record that a `FIN` is about to be delivered to the handle.
    fn peer_fin(&mut self) {
        self.peer_fin_delivered = true;
    }

    /// Check one outbound message against the rules above.
    fn observe(&mut self, message: &Message) {
        assert!(
            !self.reset_sent,
            "a message was emitted after RESET_STREAM; the reset moves the channel to a \
             terminal state, so the peer has already discarded this stream",
        );

        match message.flag {
            Some(Flag::FinAck) => {
                assert!(
                    self.peer_fin_delivered,
                    "FIN_ACK was emitted without the peer ever sending FIN",
                );
                assert!(
                    !self.fin_ack_sent,
                    "FIN_ACK was emitted twice; the reader state advances to FinAck on the \
                     first one and never returns to Fin",
                );
                self.fin_ack_sent = true;
            }
            Some(Flag::Fin) => {
                assert!(
                    !self.fin_sent,
                    "FIN was emitted twice; the writer state advances to Fin on the first \
                     one and can only move on to FinAck",
                );
                self.fin_sent = true;
            }
            Some(Flag::ResetStream) => self.reset_sent = true,
            Some(Flag::StopSending) => {
                panic!("STOP_SENDING was emitted, but it is a flag only the peer sends to us")
            }
            None => assert!(
                !self.fin_sent,
                "a payload was emitted after FIN closed the write half; once the writer \
                 reaches Fin, `poll_next` stops polling the outbound channel entirely",
            ),
        }
    }
}

async fn replay(script: Script) {
    let (substream, mut handle) = Substream::new();
    let mut substream = Some(substream);
    let mut protocol = Protocol::default();

    // Set once `on_message` errors. At that point `connection.rs` has torn the channel down
    // and no further peer data can arrive — but the protocol still owns its `Substream`, so
    // local reads and writes continue and must observe the failure.
    let mut peer_gone = false;

    for op in script.ops.into_iter().take(MAX_OPS) {
        match op {
            Op::Inbound {
                mut payload,
                flag: idx,
            } => {
                if peer_gone {
                    continue;
                }

                // The wire cannot deliver more than one frame's worth: `on_inbound_data`
                // rejects any frame declaring more than `MAX_FRAME_SIZE` before a payload
                // ever reaches the handle.
                payload.truncate(MAX_FRAME_SIZE);

                let flag = idx.map(flag);
                if flag == Some(Flag::Fin) {
                    protocol.peer_fin();
                }

                let message = WebRtcMessage {
                    payload: Some(payload),
                    flag,
                };

                if let Some(Err(_)) = handle.on_message(message).now_or_never() {
                    peer_gone = true;
                }
            }
            Op::Write { data } => {
                if let Some(substream) = substream.as_mut() {
                    // A failing write is ordinary, not a teardown: the peer sent
                    // STOP_SENDING, or the channel closed. Production keeps the connection
                    // and simply stops writing, so the script carries on. `now_or_never`
                    // may also leave a short write when the outbound channel fills, which
                    // is the same faithful model — `Substream` keeps its `PollSender`
                    // rather than the dropped future.
                    let _ = substream.write_all(&data).now_or_never();
                }
            }
            Op::Read { len } => {
                if let Some(substream) = substream.as_mut() {
                    let mut buffer = vec![0u8; len as usize];
                    let _ = substream.read(&mut buffer).now_or_never();
                }
            }
            Op::Shutdown =>
                if let Some(substream) = substream.as_mut() {
                    let _ = substream.shutdown().now_or_never();
                },
            Op::PollOutbound => {
                if let Some(Some(message)) = handle.next().now_or_never() {
                    protocol.observe(&message);
                }
            }
            Op::AdvanceTime => {
                // Comfortably past the 10s FIN_ACK timeout. The timeout constant is
                // private, so the harness overshoots rather than importing it.
                tokio::time::advance(std::time::Duration::from_secs(30)).await;
            }
            Op::DropSubstream => {
                substream = None;
            }
        }
    }

    // Drain whatever the handle still owes, then let both halves drop. `Drop for
    // SubstreamHandle` resets the stream unless both directions reached FIN_ACK, so
    // reaching this point with a live handle is itself part of the coverage.
    drain(&mut handle, &mut protocol);
    drop(substream);
    drain(&mut handle, &mut protocol);
}

/// Poll the handle until it stops yielding, bounded so a self-feeding state machine
/// cannot spin forever inside one iteration.
fn drain(handle: &mut SubstreamHandle, protocol: &mut Protocol) {
    for _ in 0..MAX_OPS {
        match handle.next().now_or_never().flatten() {
            Some(message) => protocol.observe(&message),
            None => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(future)
    }

    fn run(ops: Vec<Op>) {
        block_on(replay(Script { ops }));
    }

    /// The harness must survive any script without panicking on its own account. Sweep a
    /// deterministic spread of op sequences so a self-inflicted panic surfaces here rather
    /// than as a bogus finding on the first fuzzing run.
    #[test]
    fn scripts_replay_without_harness_panics() {
        let payloads = [vec![], vec![0u8; 1], vec![0xab; 4096]];

        for flag_idx in 0..5u8 {
            for payload in &payloads {
                run(vec![
                    Op::Inbound {
                        payload: payload.clone(),
                        flag: (flag_idx < 4).then_some(flag_idx),
                    },
                    Op::Read { len: 4096 },
                    Op::Write { data: payload.clone() },
                    Op::PollOutbound,
                    Op::Shutdown,
                    Op::AdvanceTime,
                    Op::PollOutbound,
                    Op::DropSubstream,
                    Op::PollOutbound,
                ]);
            }
        }
    }

    /// Every ordering of a small alphabet, so the protocol assertions are exercised far
    /// more widely than the committed seeds do. If any rule in [`Protocol`] is wrong about
    /// litep2p, this is where it shows up — as a harness bug, before fuzzing starts.
    #[test]
    fn protocol_rules_hold_across_short_scripts() {
        fn op(code: u8) -> Op {
            match code {
                0 => Op::Inbound {
                    payload: vec![],
                    flag: Some(script::FIN),
                },
                1 => Op::Inbound {
                    payload: vec![],
                    flag: Some(script::FIN_ACK),
                },
                2 => Op::Inbound {
                    payload: vec![],
                    flag: Some(script::STOP_SENDING),
                },
                3 => Op::Inbound {
                    payload: vec![],
                    flag: Some(script::RESET_STREAM),
                },
                4 => Op::Inbound {
                    payload: vec![1, 2, 3],
                    flag: None,
                },
                5 => Op::Write { data: vec![4, 5, 6] },
                6 => Op::Shutdown,
                7 => Op::PollOutbound,
                8 => Op::AdvanceTime,
                _ => Op::DropSubstream,
            }
        }

        const ALPHABET: u8 = 10;

        for a in 0..ALPHABET {
            for b in 0..ALPHABET {
                for c in 0..ALPHABET {
                    // A poll after every step, so emitted messages are actually observed
                    // rather than left queued where no assertion can see them.
                    run(vec![
                        op(a),
                        Op::PollOutbound,
                        op(b),
                        Op::PollOutbound,
                        op(c),
                        Op::PollOutbound,
                    ]);
                }
            }
        }
    }

    /// A `FIN_ACK` must follow a peer `FIN`, and the harness must actually see it. This
    /// pins the positive case, so the rule cannot be satisfied by never emitting anything.
    #[test]
    fn fin_ack_follows_peer_fin() {
        block_on(async {
            let (_substream, mut handle) = Substream::new();
            let mut protocol = Protocol::default();

            protocol.peer_fin();
            handle
                .on_message(WebRtcMessage {
                    payload: None,
                    flag: Some(Flag::Fin),
                })
                .now_or_never()
                .unwrap()
                .unwrap();

            let message = handle.next().now_or_never().flatten().expect("FIN_ACK is owed");
            protocol.observe(&message);

            assert_eq!(message.flag, Some(Flag::FinAck));
            assert!(protocol.fin_ack_sent, "the observer must have recorded the FIN_ACK");
        });
    }

    /// Seeds that trip a harness assertion because they reproduce an open litep2p defect.
    ///
    /// Keeping them in the corpus is the point: the fuzzer should start from a known-bad
    /// shape and mutate around it. Listing them here keeps `cargo test` honest — any other
    /// seed that starts failing is a regression, and when the defect is fixed this test
    /// fails with "no longer trips" and the entry should be deleted.
    ///
    /// Empty: the wedged-buffer defect is fixed, so `conn-wedged-buffer-growth` now replays
    /// cleanly and was removed from this list.
    const KNOWN_FAILING_SEEDS: &[&str] = &[];

    /// Every committed seed must decode with the harness's own bincode options and replay
    /// cleanly. A seed that fails to decode is worse than no seed: it looks like corpus
    /// coverage while every iteration returns immediately.
    #[test]
    fn committed_corpus_decodes_and_replays() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus");
        let entries = std::fs::read_dir(&dir).expect("corpus directory to exist");

        let mut count = 0;
        for entry in entries {
            let path = entry.expect("readable entry").path();
            let name = path.file_name().expect("named seed").to_string_lossy().to_string();
            let data = std::fs::read(&path).expect("readable seed");

            let input = bincode_options()
                .deserialize::<Input>(&data)
                .unwrap_or_else(|error| panic!("seed {name} does not decode: {error}"));

            let replay_seed = move || match input {
                Input::Substream(script) => block_on(replay(script)),
                Input::Connection(script) => block_on(replay_connection(script)),
            };

            if KNOWN_FAILING_SEEDS.contains(&name.as_str()) {
                // The assertion firing is the expected result, so silence the message.
                // `set_hook` is process-wide, so a concurrent test that panics inside this
                // window loses its message; it still fails, just less loudly.
                let hook = std::panic::take_hook();
                std::panic::set_hook(Box::new(|_| {}));
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(replay_seed));
                std::panic::set_hook(hook);

                assert!(
                    result.is_err(),
                    "seed {name} no longer trips an assertion; if litep2p was fixed, drop \
                     it from KNOWN_FAILING_SEEDS",
                );
            } else {
                replay_seed();
            }

            count += 1;
        }

        assert!(count >= 15, "expected the generated corpus, found {count} seeds");
    }

    /// Sweep connection scripts and require that no assertion fires at all.
    ///
    /// The connection oracle has three assertions and two of them are about counts rather than
    /// about a single call, which is exactly where a harness starts accusing litep2p of things
    /// it did not do. The wedged-buffer defect that used to make this sweep tolerate one
    /// failure is fixed, so the bar is now absolute: any panic here is a false positive and
    /// must surface in `cargo test` rather than as a crash on the first fuzzing run.
    #[test]
    fn connection_scripts_trip_no_assertions() {
        // A legal frame, an unfinishable one, and the three shapes that are rejected
        // permanently.
        let patterns: Vec<Vec<u8>> = vec![
            vec![],
            vec![0x00],
            WebRtcMessage::encode(b"hello".to_vec(), None),
            vec![0xac, 0x02, 0xaa, 0xbb],
            vec![0x80, 0x00],
            vec![0x81, 0x80, 0x01],
            vec![0x5a; 1024],
        ];

        const ALPHABET: u8 = 7;

        for pattern in &patterns {
            for a in 0..ALPHABET {
                for b in 0..ALPHABET {
                    for c in 0..ALPHABET {
                        let op = |code: u8| match code {
                            0 => ConnectionOp::OpenChannel,
                            1 => ConnectionOp::OpenNegotiated,
                            2 => ConnectionOp::Inbound {
                                channel: 0,
                                data: pattern.clone(),
                            },
                            3 => ConnectionOp::Inbound {
                                channel: 1,
                                data: pattern.clone(),
                            },
                            4 => ConnectionOp::CloseChannel { channel: 0 },
                            5 => ConnectionOp::PollHandles,
                            _ => ConnectionOp::ReadSubstream { channel: 0, len: 64 },
                        };

                        let script = ConnectionScript {
                            ops: vec![ConnectionOp::OpenChannel, op(a), op(b), op(c)],
                        };

                        block_on(replay_connection(script));
                    }
                }
            }
        }
    }

    /// The negotiation seed's frame body must be a protobuf `webrtc.Message` that the
    /// listener actually accepts.
    ///
    /// This reads the committed seed and pulls the real bytes out of it, because the failure
    /// it guards against is silent: framing the multistream payload by hand instead of
    /// through `WebRtcMessage::encode` makes `WebRtcMessage::decode` fail on the first field
    /// tag, so `on_inbound_opening_channel_data` bails before `webrtc_listener_negotiate` is
    /// ever called. `dispatch_framed_message` swallows that error, so the scaffold reports
    /// the same `Ok(())` either way and nothing else in this harness can tell the difference.
    #[test]
    fn negotiate_seed_body_reaches_negotiation() {
        use litep2p::{
            multistream_select::{webrtc_listener_negotiate, ListenerSelectResult},
            transport::webrtc::util::extract_framed_message,
        };

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("corpus")
            .join("conn-single-channel-negotiate");
        let data = std::fs::read(&path).expect("negotiation seed to exist");

        let Ok(Input::Connection(script)) = bincode_options().deserialize::<Input>(&data) else {
            panic!("the negotiation seed must decode as a connection script");
        };

        let frame = script
            .ops
            .iter()
            .find_map(|op| match op {
                ConnectionOp::Inbound { data, .. } => Some(data.clone()),
                _ => None,
            })
            .expect("the seed must feed a frame");

        let mut buffer = bytes::BytesMut::from(&frame[..]);
        let body = extract_framed_message(&mut buffer)
            .expect("the seed's frame must de-frame")
            .expect("the seed's frame must be complete");

        let payload = WebRtcMessage::decode(&body)
            .expect("the frame body must be a protobuf webrtc.Message")
            .payload
            .expect("the message must carry the multistream payload");

        let protocols =
            SUPPORTED_PROTOCOLS.iter().map(|name| ProtocolName::from(*name)).collect();
        let result = webrtc_listener_negotiate(protocols, payload.into(), false)
            .expect("the seed must negotiate");

        assert!(
            matches!(result, ListenerSelectResult::Accepted { .. }),
            "the negotiation seed must be accepted, got {result:?}",
        );
    }

    /// The `Open`-state seed must deliver its payload all the way to the local substream.
    ///
    /// `OpenNegotiated` exists to make `ChannelState::Open` reachable, and the only way to know
    /// it worked is to observe the payload arriving. If this fails, the seed is opening a
    /// channel that swallows its input and the whole `Open` path is nominal coverage only.
    #[test]
    fn open_substream_seed_delivers_payload() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("corpus")
            .join("conn-open-substream-traffic");
        let data = std::fs::read(&path).expect("open-substream seed to exist");

        let Ok(Input::Connection(script)) = bincode_options().deserialize::<Input>(&data) else {
            panic!("the open-substream seed must decode as a connection script");
        };

        // The first framed payload the seed feeds, taken from the seed itself rather than
        // rebuilt, so a stale seed cannot pass this.
        let frame = script
            .ops
            .iter()
            .find_map(|op| match op {
                ConnectionOp::Inbound { data, .. } => Some(data.clone()),
                _ => None,
            })
            .expect("the seed must feed a frame");

        block_on(async {
            let protocols =
                SUPPORTED_PROTOCOLS.iter().map(|name| ProtocolName::from(*name)).collect();
            let mut connection =
                FuzzConnection::new(protocols).await.expect("scaffold must build");

            let index = connection
                .open_negotiated_channel(ProtocolName::from(SUPPORTED_PROTOCOLS[0]))
                .expect("negotiated channel installs");

            connection.inbound(index, frame).await.expect("frame is handled");

            assert_eq!(
                connection.read_substream(index, 64).as_deref(),
                Some(&b"payload"[..]),
                "the seed's payload must reach the local end of the substream",
            );
        });
    }

    /// The connection scaffold must actually stand up and accept inbound bytes. If
    /// `FuzzConnection::new` fails, the whole connection layer is unfuzzed.
    #[test]
    fn connection_scaffold_accepts_inbound_data() {
        block_on(async {
            let protocols =
                SUPPORTED_PROTOCOLS.iter().map(|name| ProtocolName::from(*name)).collect();
            let mut connection = FuzzConnection::new(protocols)
                .await
                .expect("scaffold must build without a DTLS handshake");

            assert_eq!(connection.open_channel().await.expect("channel opens"), 0);
            assert_eq!(connection.open_channel().await.expect("channel opens"), 1);

            // A frame that will never complete must be buffered, not rejected.
            connection
                .inbound(0, vec![0xac, 0x02, 0xaa, 0xbb])
                .await
                .expect("incomplete frame is not an error");
            assert_eq!(
                connection.buffered_bytes(),
                4,
                "an incomplete frame must stay buffered awaiting more bytes",
            );

            // A second channel gets its own buffer — this is the per-channel accounting the
            // aggregate-memory question rests on.
            connection.inbound(1, vec![0xac, 0x02]).await.expect("buffered");
            assert_eq!(connection.buffer_count(), 2);
            assert_eq!(connection.buffered_bytes(), 6);

            // Closing must reclaim the buffer.
            connection.close_channel(0).await.expect("close");
            assert_eq!(
                connection.buffer_count(),
                1,
                "closing a channel must drop its reassembly buffer",
            );

            // Out-of-range indices are no-ops rather than panics.
            connection.inbound(200, vec![0xff]).await.expect("no-op");
            connection.close_channel(200).await.expect("no-op");
        });
    }

    /// A permanently rejected frame must drop the reassembly buffer and tear the channel
    /// down, so later bytes cannot accumulate on a buffer that will never parse.
    ///
    /// This is the reproducer for the wedged-buffer defect: previously the rejected frame
    /// was left in place and the channel stayed open, growing without bound. It was written
    /// as `#[should_panic]` while the defect existed and is inverted now that litep2p drops
    /// the buffer on a permanent framing error.
    #[test]
    fn permanent_parse_error_drops_buffer() {
        block_on(async {
            let protocols =
                SUPPORTED_PROTOCOLS.iter().map(|name| ProtocolName::from(*name)).collect();
            let mut connection =
                FuzzConnection::new(protocols).await.expect("scaffold must build");
            connection.open_channel().await.expect("channel opens");

            // Non-minimal varint: decodes to zero but is not the shortest encoding, so
            // `extract_framed_message` rejects it permanently without consuming it.
            assert!(
                connection.inbound(0, vec![0x80, 0x00]).await.is_err(),
                "permanent framing error must be reported",
            );

            assert_eq!(
                connection.buffer_count(),
                0,
                "the reassembly buffer must be dropped on a permanent framing error",
            );

            // Every further append must not regrow a buffer: the channel is closing and
            // its data is discarded. A `Closing` channel may transiently hold an empty
            // entry while its data is drained, but it must never retain bytes.
            for _ in 0..8 {
                let _ = connection.inbound(0, vec![0u8; MAX_OP_BYTES]).await;
                assert_buffers_bounded(&connection);
                assert_eq!(connection.buffered_bytes(), 0);
            }
        });
    }

    /// A clean write, read and half-close sequence must flow end to end, confirming the
    /// `now_or_never` driving actually makes progress instead of silently no-opping every
    /// operation (which would leave the harness reporting full coverage of nothing).
    #[test]
    fn happy_path_makes_progress() {
        block_on(async {
            let (mut substream, mut handle) = Substream::new();

            substream.write_all(&[1, 2, 3, 4]).now_or_never().unwrap().unwrap();
            let outbound = handle.next().now_or_never().flatten();
            assert_eq!(
                outbound.map(|message| message.payload),
                Some(vec![1, 2, 3, 4]),
                "a write must surface on the handle within a single poll",
            );

            handle
                .on_message(WebRtcMessage {
                    payload: Some(vec![9, 9]),
                    flag: None,
                })
                .now_or_never()
                .unwrap()
                .unwrap();

            let mut buffer = [0u8; 2];
            let read = substream.read(&mut buffer).now_or_never().unwrap().unwrap();
            assert_eq!((read, buffer), (2, [9, 9]), "inbound payload must be readable");
        });
    }

    /// The inbound channel is capped at 256 messages and an over-full channel is treated
    /// as flooding rather than as a slow reader. Confirm the harness can actually reach
    /// that reset path — if it cannot, the flood-handling code is untested by this fuzzer.
    ///
    /// The reset only fires on the message that finds the channel *already* full, so
    /// `MAX_OPS` has to exceed the 256-message capacity. This asserts the observable
    /// consequence — a RESET_STREAM on the outbound side — rather than just that nothing
    /// panicked, which would pass even with the cap set too low.
    #[test]
    fn inbound_flood_reaches_reset() {
        block_on(async {
            let (_substream, mut handle) = Substream::new();

            // Never read, so the inbound channel fills and stays full.
            for _ in 0..MAX_OPS {
                if handle
                    .on_message(WebRtcMessage {
                        payload: Some(vec![0u8; 16]),
                        flag: None,
                    })
                    .now_or_never()
                    .is_none()
                {
                    break;
                }
            }

            let reset = (0..MAX_OPS).any(|_| {
                matches!(
                    handle.next().now_or_never().flatten(),
                    Some(message) if message.flag == Some(Flag::ResetStream)
                )
            });

            assert!(
                reset,
                "flooding {MAX_OPS} messages past the 256-message inbound capacity must \
                 produce RESET_STREAM; if not, MAX_OPS is below the threshold and the \
                 flood path is unreachable from this harness",
            );
        });
    }
}
