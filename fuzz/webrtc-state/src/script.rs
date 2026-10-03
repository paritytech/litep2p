// Copyright 2026 litep2p developers
//
// Licensed under the same terms as the rest of this repository; see `src/main.rs`.

//! The fuzzed operation script, shared by the harness and its seed generator.

use serde::{Deserialize, Serialize};

/// Upper bound on decoded input size.
///
/// Without a limit the fuzzer can ask the decoder for a multi-gigabyte `Vec` and the
/// resulting OOM is reported as a litep2p finding. The limit keeps crashes attributable to
/// the code under test.
pub const MAX_DECODED_BYTES: u64 = 1 << 20;

/// Upper bound on operations per script, so one input cannot monopolise the fuzzer.
///
/// Must stay comfortably above the substream's 256-message inbound capacity: the flood
/// path only triggers on the message that finds the channel already full, so a cap of 256
/// would fill the channel exactly and leave the reset unreachable.
pub const MAX_OPS: usize = 512;

/// Upper bound on the bytes a single operation may carry.
///
/// A megabyte of `0x00` is a million zero-length frames, each costing a hashmap round trip,
/// a protobuf decode and a tracing field evaluation. That is a fuzzer timeout reported as a
/// hang, not a litep2p defect. The cap is a little over `MAX_FRAME_SIZE` so an oversized
/// frame is still expressible.
pub const MAX_OP_BYTES: usize = (16 * 1024) + 512;

/// The bincode configuration shared by the harness and the seed generator.
///
/// Varint lengths keep more of the fuzzer's bytes carrying meaning instead of padding
/// length prefixes.
///
/// `allow_trailing_bytes` matters more than it looks. With bincode's default
/// reject-trailing behaviour, every mutation that inserts or deletes a byte shifts how much
/// the decode consumes and the remainder is then rejected as garbage, so the fuzzer can only
/// flip payload bytes in place and never explores *orderings* — which is the entire reason
/// this harness takes a structured script instead of a byte blob.
///
/// Both binaries call this, so the encoder and the decoder cannot drift apart. If they ever
/// did, every committed seed would silently stop decoding.
pub fn bincode_options() -> impl bincode::Options {
    use bincode::Options as _;

    bincode::options().with_limit(MAX_DECODED_BYTES).allow_trailing_bytes()
}

/// A single step in the fuzzed script.
#[derive(Debug, Serialize, Deserialize)]
pub enum Op {
    /// Deliver a message from the remote peer to the handle.
    ///
    /// `flag` is an index rather than a `Flag`, because `WebRtcMessage::flag` is already
    /// a validated enum — unknown wire integers are coerced to `None` further upstream in
    /// `WebRtcMessage::decode`, which `fuzz/webrtc-codec` covers.
    Inbound { payload: Vec<u8>, flag: Option<u8> },

    /// Write to the substream, which the handle should observe as outbound messages.
    Write { data: Vec<u8> },

    /// Read from the substream.
    Read { len: u16 },

    /// Close the write half, which emits `FIN` and arms the FIN_ACK timer.
    Shutdown,

    /// Drain one outbound message from the handle.
    PollOutbound,

    /// Jump virtual time past the FIN_ACK timeout so the forced-reset path is reachable.
    AdvanceTime,

    /// Drop the substream while keeping the handle.
    ///
    /// This closes the outbound sender, so the next poll of the handle sees the write side
    /// finish and runs `poll_half_close`. It says nothing about `Drop for SubstreamHandle`,
    /// which only runs when the handle itself goes out of scope at the end of a script.
    DropSubstream,
}

/// Flag indices, named so seeds read as protocol sequences rather than magic numbers.
/// Must match the mapping in `main.rs`.
pub const FIN: u8 = 0;
pub const STOP_SENDING: u8 = 1;
pub const RESET_STREAM: u8 = 2;
pub const FIN_ACK: u8 = 3;

#[derive(Debug, Serialize, Deserialize)]
pub struct Script {
    pub ops: Vec<Op>,
}

/// A step in a connection-level script.
///
/// Channels are addressed by creation index rather than by `ChannelId`, because str0m's
/// `ChannelId` is opaque and only the connection knows the real ones. Out-of-range indices
/// are no-ops, which keeps mutated inputs useful instead of aborting the script.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConnectionOp {
    /// Open a new inbound data channel.
    OpenChannel,

    /// Feed raw bytes to a channel, as an SCTP message would arrive.
    Inbound { channel: u8, data: Vec<u8> },

    /// Close a channel, which should drop its reassembly buffer.
    CloseChannel { channel: u8 },

    /// Install a channel already in `ChannelState::Open`, bypassing multistream-select.
    ///
    /// Without this the connection layer never reaches the `Open` state, because the
    /// multistream-select response write always fails in the scaffold and every negotiation
    /// outcome closes its channel. This is what makes `on_open_channel_data`,
    /// `SubstreamHandle::on_message` and the handle set reachable through the connection.
    ///
    /// New variants go at the end of this enum on purpose: bincode encodes the variant index,
    /// so appending keeps every committed seed decoding.
    OpenNegotiated,

    /// Poll the substream handle set once, driving its round-robin.
    PollHandles,

    /// Read from the local end of a substream, so inbound delivery is observable.
    ReadSubstream { channel: u8, len: u16 },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ConnectionScript {
    pub ops: Vec<ConnectionOp>,
}

/// Top-level fuzzer input.
///
/// Both layers share one corpus so a single ziggy target covers them. They are separate
/// variants rather than separate binaries because the substream layer is reached *through*
/// the connection layer in production, and keeping them adjacent makes that relationship
/// obvious in the corpus.
#[derive(Debug, Serialize, Deserialize)]
pub enum Input {
    /// Drive `SubstreamHandle` directly.
    Substream(Script),

    /// Drive `WebRtcConnection`'s inbound path.
    Connection(ConnectionScript),
}
