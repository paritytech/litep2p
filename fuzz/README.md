# litep2p fuzzing harnesses

Five [`ziggy`](https://github.com/srlabs/ziggy) harnesses. Each is a standalone crate with
its own `Cargo.lock` — the root `Cargo.toml` has no `[workspace]` table, so none of them is
a workspace member and `cargo test` at the repo root does not build them.

| Harness | Layer | Determinism | Notes |
|---|---|---|---|
| `simple` | Application protocols over TCP | Low | Raw bytes into kad / bitswap / request-response / notification (SRLabs, PR #367) |
| `structure-aware` | Application protocol commands over TCP | Low | bincode-decoded command enums replayed via `fuzz_send_message` (SRLabs, PR #365) |
| `webrtc-codec` | WebRTC wire parsers | **Exact** | Pure `fn(&[u8])`: frame reassembly, protobuf, multistream-select, Noise identity payload |
| `webrtc-state` | WebRTC state machines | High | Structure-aware scripts against `SubstreamHandle` and `WebRtcConnection` |
| `webrtc-datagram` | Live WebRTC listener | Low | Fuzzed UDP at a bound socket; mostly reaches `str0m` |

## Setup

```sh
cargo install cargo-ziggy cargo-afl honggfuzz
```

The WebRTC harnesses build `str0m` with **vendored OpenSSL**, so expect a slow first build
and native C in-process (relevant if you want ASan).

## Running

```sh
cd fuzz/webrtc-codec
cargo ziggy fuzz --input corpus     # seeds are committed; see "Corpora" below
```

Every harness also has ordinary `cargo test` self-checks. **Run them before fuzzing.** A
harness whose own assertions are subtly wrong reports itself as a finding on the first run,
and one whose seeds do not decode reports execs while doing nothing:

```sh
cargo test
```

## Which harness to reach for

litep2p's WebRTC transport is **listen-only** — `WebRtcTransport::dial()` returns
`Error::NotSupported`. There is no litep2p WebRTC client, so the "two instances, one dials
the other" pattern that `simple` and `structure-aware` use does not transfer. And `str0m`
owns all of ICE/STUN/DTLS/SCTP (litep2p uses `direct_api()`, so there is no SDP code at
all), which means litep2p's own parsers sit *behind* DTLS.

That is why the WebRTC coverage is split three ways instead of being one end-to-end fuzzer:

- **`webrtc-codec` is where the value is.** Pure synchronous parsers, no sockets or crypto,
  so crashes reproduce exactly from a corpus entry and throughput is high. Sub-targets are
  multiplexed on the first input byte.
- **`webrtc-state` covers what has memory.** The half-close protocol
  (`FIN`/`FIN_ACK`/`STOP_SENDING`/`RESET_STREAM`) and per-channel frame reassembly. Bugs
  here are orderings, so the input is a bincode script, not a byte blob.
- **`webrtc-datagram` is the honest long shot.** It exercises the pre-handshake path, but
  most parsing it reaches is `str0m`'s and DTLS gates everything else, so coverage-per-exec
  is poor. Its real value is resource exhaustion — the transport holds an `Rtc` per remote
  address pair — so run it under a memory ceiling:

  ```sh
  ( ulimit -v 2000000 && cargo ziggy fuzz )
  ```

  Crashes in `DatagramRecv::try_from` or `StunMessage::parse` belong upstream in `str0m`.

## Corpora

`webrtc-codec` and `webrtc-state` ship committed corpora under `corpus/`, regenerated with:

```sh
cargo run --bin gen_seeds -- corpus
```

Seeds are not optional for `webrtc-state`: its input is a bincode-encoded `Input`, and a
fuzzer starting from nothing will essentially never synthesise one that decodes. Without a
corpus it reports execs while every iteration returns immediately.

For a much better corpus than synthetic seeds, capture real frames. The `webrtc-interop` CI
job (`.github/workflows/ci.yml`) drives a go-libp2p perf client against a litep2p WebRTC
server via [`litep2p-perf`](https://github.com/lexnv/litep2p-perf); the decrypted
channel-data frames from a local run make ideal input for `webrtc-codec` and `webrtc-state`.

`webrtc-datagram` commits a DTLS certificate fixture (`src/fixture.rs`) instead of a corpus.
It is committed rather than generated at startup so the certhash — and therefore the
advertised multiaddr and peer identity — is stable across runs, matching how the interop CI
job pins the server with `--node-key secret`. Without it, crash reproducers would not
replay. Regenerate with:

```sh
cargo run --bin gen_fixture > src/fixture.rs
```

## The `fuzz` feature

The interesting WebRTC targets live in private modules, so the `fuzz` cargo feature widens
their visibility. It already existed for the SRLabs harnesses (`fuzz_send_message` on the
protocol handles); the WebRTC work extends the same mechanism:

| Item | Exposed under `fuzz` |
|---|---|
| `transport::webrtc::util` | `extract_framed_message`, `WebRtcMessage`, `MAX_FRAME_SIZE` |
| `transport::webrtc::substream` | `Substream::new`, `SubstreamHandle::on_message` |
| `transport::webrtc::schema` | generated `webrtc::Message` / `Flag` |
| `transport::webrtc::FuzzConnection` | scaffolding for `WebRtcConnection` |
| `multistream_select` | `webrtc_listener_negotiate`, `WebRtcDialerState` |
| `crypto::noise` | `NoiseContext::fuzz_parse_handshake_payload` |

Two things to know:

- **`fuzz` is not purely additive.** It removes the `ProtocolName::Static(&'static str)`
  variant, so harness code must construct names via `From<&'static str>`.
- **CI compile-tests this combination already.** `cargo check --all-features` enables `fuzz`
  and `webrtc` together, so the visibility plumbing cannot silently rot even though no CI
  job runs the fuzzers.

## Known limits

- **No CI runs any of these.** Matching the pre-existing state; only Dependabot touches
  `fuzz/` today. The `--all-features` check above is the sole guard against API drift.
- **`webrtc-state`'s connection layer cannot complete a negotiation.** `FuzzConnection`
  builds a `WebRtcConnection` without a DTLS handshake, so `Rtc::channel()` finds no open
  SCTP stream and every `write()` fails with `ChannelDoesntExist`. Reassembly, channel-state
  transitions and close-time cleanup are reachable; the `Open`-state substream path is not,
  and is covered by fuzzing `SubstreamHandle` directly instead. Lifting this means pairing
  two `Rtc` instances through a real DTLS/SCTP handshake per iteration — costly and
  non-deterministic, but it would unlock the full connection state machine.
- **`webrtc-state` drops post-close inbound data.** `dispatch_framed_message` carries a
  `debug_assert!(false)` on its "channel doesn't exist" branch, i.e. litep2p treats
  `Event::ChannelData` after `Event::ChannelClose` for the same channel as impossible. The
  scaffold honours that assumption; otherwise the assertion fires on nearly every script and
  buries everything else. Whether `str0m` can actually reorder those events is unverified —
  if it can, remove the guard in `FuzzConnection::inbound` and the assertion is the finding.
- **`webrtc-datagram` keeps state across iterations.** The node is built once, since
  standing up a transport per iteration would dominate runtime and churn through ports.
