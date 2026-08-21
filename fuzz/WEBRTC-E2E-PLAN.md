# Plan: single `webrtc-e2e` fuzzer — real str0m client dials litep2p over localhost

## Goal

Replace the split `webrtc-state` + `webrtc-datagram` harnesses with one fuzzer that runs a real str0m WebRTC **client** (`Rtc`, ICE-controlling) against a live litep2p WebRTC **listener** on `127.0.0.1:0`. The fuzzer has direct control of the packet stream — driving the client's legit traffic **and** injecting raw/corrupted/error datagrams — while litep2p's responses (STUN, DTLS, SCTP, Noise, multistream-select, ping PONG, substream data) come back over the same loopback socket and are validated by the fuzzer.

## Why this architecture

- `WebRtcTransport::dial()` is `NotSupported` (src/transport/webrtc/mod.rs:515), so the client must be a raw str0m `Rtc`. str0m is in-process by design: `poll_output()` → `DatagramSend`, `handle_input(Input::Receive)` ← datagrams, so a loopback UDP pair is trivial.
- The server already uses `direct_api()` + ICE-lite + `REMOTE_FINGERPRINT` (all-FF, verification off) in `make_rtc` (mod.rs:194-247); a controlling client completes ICE/DTLS/SCTP against it with no SDP code.
- This is the only shape that reaches litep2p's real handshake: `OpeningWebRtcConnection` → Noise → `WebRtcConnection::new` (connection.rs:294) with a real SCTP stream id, so the multistream-select `Accepted/Rejected/Pending` states become reachable for the first time (currently blocked — connection.rs:1526-1546).

## Phase 0 — De-risk spike (0.5 day)

Stand up in a throwaway `tests/` or scratch binary: str0m client `Rtc` (controlling role, fixture cert, fixed identity key) dialing a `Litep2p` node with WebRTC + ping. Confirm end-to-end: ICE → DTLS → SCTP → Noise → first channel → ping PONG, all over one `127.0.0.1` UDP pair. Nail the Noise prologue fingerprint matching (`noise_prologue`, opening.rs:54) — the client must use the server's *real* DTLS fingerprint, not the all-FF placeholder. This de-risks everything downstream.

## Phase 1 — Fuzz-feature plumbing in `src/transport/webrtc` (1–1.5 days)

Extend the existing `#[cfg(feature = "fuzz")]` surface (already used for `FuzzConnection`, `util`, `schema`, `multistream_select`):
- A `FuzzE2eConnection`/`FuzzWebRtcNode` scaffold that builds the server-side `OpeningWebRtcConnection` → `WebRtcConnection` transition plus the paired client `Rtc`, mirroring `make_rtc` + opening.rs with a fixed DTLS cert (reuse `webrtc-datagram`'s fixture) and a fixed identity keypair.
- A raw-datagram injection hook into the listener's socket path (send from the client socket to the bound port — real quinn-udp, no bypass).
- Expose draining `Litep2p::next_event()` so the oracle can track what litep2p saw.

## Phase 2 — Packet control (1 day)

Fuzzer input (bincode script, `allow_trailing_bytes`, shared `script.rs`/`gen_seeds.rs` like webrtc-state) maps to a bounded op stream (cap ops + bytes per iteration):

| Op | Effect |
|---|---|
| `DriveClient` | Poll client `Rtc`, transmit `poll_output()` datagrams (legit traffic) |
| `InjectRaw(datagram)` | Send fuzzed bytes straight at the listener (garbage STUN, truncated DTLS, non-minimal varint, oversized/empty frames, flood) |
| `CorruptNext {mode}` | Flip/truncate/duplicate/reorder the next client-emitted datagram before send (in-flight packet mutation) |
| `AdvanceTime` | Step virtual time so str0m/litep2p timeouts fire (paused clock, as in webrtc-state) |
| `CloseChannel` / `ResetStream` | Client-side half-close of a substream |

Paused tokio clock (`start_paused(true)`), one runtime via `OnceLock`, fresh logical connection per iteration on the long-lived node.

## Phase 3 — Oracles: validate litep2p's responses (0.5–1 day)

- **Protocol-level, byte-exact** (on unmutated packets): multistream-select response must equal `webrtc_listener_negotiate` output (pattern already proven at webrtc-state main.rs:647-690); ping payload → PONG within a bounded step budget.
- **State invariants** (reuse from webrtc-state): half-close `Protocol` checker (main.rs:244) and `assert_buffers_bounded` reassembly budget (main.rs:189) — now against *real* SCTP stream ids and real outbound writes.
- **Injected-error policy**: assert only no-panic + no unbounded buffer retention (rejected frames must drop their reassembly buffer — the wedged-buffer regression, main.rs:792).
- **Liveness**: `Litep2p::next_event()` returning `None` = harness failure, not a finding.

## Phase 4 — Corpus + seeds (0.5 day)

`gen_seeds` emits: full-handshake, negotiate, ping round-trip, and error-injection seeds. **Key win:** capture *real* STUN/DTLS/SCTP bytes from the client `Rtc` during a seed handshake and commit them — this solves webrtc-datagram's core problem (blind mutation at 2⁻³² for the STUN cookie, README:56-62) by seeding raw-injection with genuine in-flight packets.

## Phase 5 — Migration (0.5–1 day)

- Retire `webrtc-state` + `webrtc-datagram`; port their valuable tests/seeds into the new crate.
- **Keep `webrtc-codec`** — it's the only exact-repro, high-throughput target; bytes still flow through the same parsers in e2e, but exactness is worth keeping (15 min to retain).
- Update `fuzz/README.md` (harness table, "split three ways" narrative), `WEEKEND-RUNBOOK.md`, and CI notes.

## Tradeoffs / accepted limits

- Node persists across iterations → repro is "whole queue + op sequence", not a single file (same as webrtc-datagram; document it).
- Handshake uses CSPRNG (Noise `generate_keypair`, snow; DTLS) → non-byte-exact, but it's setup, not the fuzz target.
- Lower exec/s than `webrtc-codec` → that's why codec stays.
- quinn-udp GRO/GSO and `datagram_buffer_size` drop-on-full need a real flood corpus to hit (follow-up).

**Total: ~4.5–5 days**, Phase 0 and Phase 3 being the highest-value, highest-risk items.
