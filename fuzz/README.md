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
| `webrtc-datagram` | End-to-end over a real, authenticated connection | Low | A str0m client completes the handshake incl. Noise auth, then fuzzes the noise-channel framing and the post-auth substream layer. See below |

## Setup

```sh
cargo install cargo-ziggy cargo-afl honggfuzz
```

The WebRTC harnesses build `str0m` with **vendored OpenSSL**, so expect a slow first build
and native C in-process (relevant if you want ASan).

## Running

```sh
cd fuzz/webrtc-codec
cargo ziggy fuzz webrtc-codec -i corpus   # seeds are committed; see "Corpora" below
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
- **`webrtc-datagram` fuzzes the transport end to end over a real, authenticated connection.** A
  real str0m client (`src/client.rs`, ICE-controlling, DTLS-active) completes ICE/DTLS/SCTP against
  the listener over loopback. The first input byte selects the target. An even byte writes the
  chunks to the noise channel, driving the server's `on_noise_channel_data` framing and decode. An
  odd byte first runs the libp2p Noise handshake as the *responder*, so the server authenticates the
  client, then opens a real substream and feeds the chunks into multistream-select
  (`webrtc_listener_negotiate`) on a channel with a real SCTP stream id. That post-auth path is the
  one no other in-process harness reaches, since litep2p is listen-only and never runs the client
  half of the handshake. One handshake per input makes it slow, and the CSPRNG handshake makes it
  not byte-exact reproducible; treat it as an integration complement to the two stateless targets.

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

`webrtc-datagram` ships a seed corpus under `corpus/` and a committed DTLS certificate fixture
(`src/fixture.rs`). Every seed begins with the target-selector byte. The `preauth-*` seeds are
noise-channel `unsigned-varint length ++ body` frames (a valid frame, a split frame, and the
permanent framing errors); the `substream-*` seeds are multistream-select lines wrapped in a
`WebRtcMessage`, for the post-auth substream path. The fixture is committed rather than generated at
startup so the server's certhash and peer identity stay stable across runs. Regenerate with:

```sh
cargo run --bin gen_seeds -- corpus            # end-to-end seed corpus
cargo run --bin gen_fixture > src/fixture.rs   # DTLS certificate fixture
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
- **`webrtc-state`'s connection layer cannot complete a negotiation.** `FuzzConnection` builds
  a `WebRtcConnection` without a DTLS handshake, and its channels are created with
  `negotiated: None`, so str0m assigns no SCTP stream id, `Rtc::channel()` returns `None` and
  every `write()` fails with `ChannelDoesntExist`. The consequence is sharper than it sounds:
  `on_inbound_opening_channel_data` writes the multistream-select response *before* branching
  on the outcome, so `Accepted`, `Rejected` and `PendingProtocol` all fail there and close the
  channel. A channel from `open_channel()` is a one-frame channel, and the back-and-forth
  negotiation states stay unreachable through `FuzzConnection`. Reaching them for real is exactly
  what `webrtc-datagram` now does end to end, via an authenticated str0m client (see below).

  The `Open` state is reachable, through `open_negotiated_channel()`, which installs a
  substream and its handle directly and skips the write that cannot succeed. Inbound frames
  then travel the real path — reassembly, `on_open_channel_data`,
  `SubstreamHandle::on_message` — and `poll_handles()` drives the `SubstreamHandleSet`
  round-robin. `read_substream()` makes the delivery observable, which is what keeps that claim
  honest. Lifting the rest means pairing two `Rtc` instances through a real DTLS/SCTP handshake
  per iteration: costly and non-deterministic, but it would unlock the full state machine.
- **`webrtc-datagram` is slow and not byte-exact reproducible.** It performs one real ICE/DTLS/SCTP
  handshake per input, plus a Noise handshake on the post-auth path, so throughput is a fraction of
  the stateless targets, and the CSPRNG handshake means a crash replays as "the input against a
  fresh handshake", not deterministically from the file alone. The listener is built once per
  process and each input uses a fresh connection, so per-input state does not bleed; abandoned
  server-side connections are reclaimed on their ICE/DTLS timeout.
