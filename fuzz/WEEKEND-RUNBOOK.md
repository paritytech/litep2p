# WebRTC fuzzing runbook (Ubuntu, 16-core EPYC, 48h unattended)

Runs the WebRTC ziggy harnesses (`webrtc-codec`, `webrtc-state`, `webrtc-datagram`) under AFL++ +
honggfuzz for ~2 days, supervised so it restarts through any failure and survives reboot, with
crashes and logs on disk and reproducible from a pinned commit.

Target commit: the tip of branch `gab_webrtc_fuzzin` with the fuzz harnesses committed. Commit and
push the harness work first (the `webrtc-datagram` corpus and `client.rs` may still be untracked);
`3bf53d20` predates all of it. The exact SHA is recorded into provenance in step 5.
Paths below assume user `fuzz`, checkout `~/litep2p`, output volume `/data`. Adjust if yours differ.

## How it's wired (two ziggy facts that drive the design)

- **Ziggy has no headless mode.** It always redraws an ANSI dashboard on stdout. Ignore it; the
  durable, readable artifacts are `logs/` and `crashes/` under the output dir (step 7).
- **Ziggy does not restart a dead fuzzer.** If any AFL++/honggfuzz child exits, ziggy exits too, so
  supervision has to be external. A systemd unit with `Restart=always` + `StartLimitIntervalSec=0`
  handles it (step 6). On restart AFL++ resumes from the persisted corpus (`AFL_AUTORESUME=1`), so no
  progress is lost.

## What to expect per target

| Target | Crash reproducible? | Notes |
|---|---|---|
| `webrtc-codec` | **Exactly**, from one input file. | Pure parser, no IO/clock/RNG. This is the target that matters; its oracles make a crash meaningful. |
| `webrtc-state` | **Usually not from one file.** | Shares a process-global paused clock (virtual time accumulates across iterations) and calls `PeerId::random()`. The placeholder UDP socket is now bound once per process, not per input, and a bind failure exits cleanly instead of being filed as a crash. Keep the whole crash dir + op sequence. |
| `webrtc-datagram` | **No** (real handshake, CSPRNG). | End-to-end: a str0m client completes the handshake incl. Noise auth as responder, then fuzzes the noise-channel framing (even selector) or the post-auth substream negotiation on a real channel (odd selector). One handshake per input, so low exec/s. The only target reaching the authenticated layer. Worth a couple of cores, not more. |

## 1. Provision

16-core EPYC dedicated (real cores, stable clocks), ≥32 GB RAM, and **≥100 GB free disk**. Each of
the three targets builds litep2p and vendored OpenSSL several times over (an AFL++, a honggfuzz, and
a coverage build), so the build trees alone reach tens of GB, on top of a growing corpus and
always-on logs. Put `/data` on the largest volume. Build and fuzz as a **non-root sudo user**
(`fuzz`); AFL++ prefers non-root.

## 2. Packages (root)

```bash
apt-get update && apt-get install -y \
  build-essential git curl pkg-config perl \
  clang llvm llvm-dev lld \
  binutils-dev libunwind-dev libblocksruntime-dev liblzma-dev \
  protobuf-compiler tmux
```

`protobuf-compiler` = `protoc`, required by litep2p's `build.rs`. `perl` + `build-essential` build
str0m's vendored OpenSSL. `binutils-dev` (libbfd), `libunwind-dev`, `libblocksruntime-dev`,
`liblzma-dev` are honggfuzz's build deps. `clang`/`llvm`/`llvm-dev`/`lld` build AFL++ and its LLVM
instrumentation (Ubuntu's default LLVM is fine; AFL++ supports LLVM 14 to 21).

## 3. System config (root; reboot-persistent)

AFL++ aborts if `core_pattern` pipes crashes to a handler, which Ubuntu does via Apport. Set a plain
pattern and stop Apport re-piping it at boot:

```bash
echo 'kernel.core_pattern=core' > /etc/sysctl.d/99-afl.conf && sysctl --system
echo 'enabled=0' > /etc/default/apport
systemctl disable --now apport.service 2>/dev/null || true
```

Pin the CPU governor to `performance` across reboots (no linux-tools dependency):

```bash
cat >/etc/systemd/system/cpu-performance.service <<'EOF'
[Unit]
Description=Set CPU governor to performance
[Service]
Type=oneshot
ExecStart=/bin/sh -c 'echo performance | tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor'
[Install]
WantedBy=multi-user.target
EOF
systemctl enable --now cpu-performance.service
```

The fuzz unit also exports `AFL_SKIP_CPUFREQ=1`, so AFL starts even if cpufreq is unavailable.

## 4. Toolchain + fuzzers (user `fuzz`)

```bash
# rustup (Ubuntu's apt rust can be stale; need >=1.85 for edition 2024)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && . "$HOME/.cargo/env"
rustup default stable
cargo install --locked cargo-afl      # builds AFL++ from source (needs clang/llvm)
cargo install --locked honggfuzz      # uses libblocksruntime-dev/binutils-dev/libunwind-dev from step 2
cargo install --locked ziggy          # cargo-ziggy CLI; must be >=1.7 (matches the crates' lock)
cargo install --locked casr           # for `cargo ziggy triage` crash dedup
```

## 5. Clone, record provenance, seed, build (user `fuzz`, once)

```bash
git clone https://github.com/paritytech/litep2p.git ~/litep2p
cd ~/litep2p && git checkout gab_webrtc_fuzzin   # push this branch to origin first if it isn't there

# provenance: pin everything a repro depends on
git rev-parse HEAD | tee ~/PROVENANCE            # the branch tip with the harnesses committed
{ rustc -V; cargo afl --version; ziggy --version; } >> ~/PROVENANCE

# all three targets ship committed corpora under fuzz/<target>/corpus (datagram's are mode-prefixed e2e seeds)

# build all three. --release activates the debug-assertions + overflow-checks profile in each crate.
# First build is slow: vendored OpenSSL + two instrumented binaries (AFL++ and honggfuzz) per target.
for t in webrtc-codec webrtc-state webrtc-datagram; do
  ( cd ~/litep2p/fuzz/$t && cargo ziggy build --release )
done
```

Do **not** `cargo update`. Each fuzz crate ships its own `Cargo.lock`; keep it for reproducible deps.

Preflight before committing the weekend (catches a broken harness before it wastes 48h):

```bash
# 1. self-checks. webrtc-datagram's tests run a real client<->listener handshake, Noise auth, and
#    a post-auth substream, so green confirms the end-to-end path builds and works.
for t in webrtc-codec webrtc-state webrtc-datagram; do ( cd ~/litep2p/fuzz/$t && cargo test ); done

# 2. determinism of the stateless targets
( cd ~/litep2p/fuzz/webrtc-codec && cargo ziggy stability webrtc-codec -n 10 )   # expect ~100%
( cd ~/litep2p/fuzz/webrtc-state && cargo ziggy stability webrtc-state -n 10 )   # <100% expected
```

Then **smoke each target under the real fuzzer for ~5 minutes**. This is the only preflight that
exercises the live AFL++ fork server, and it is mandatory for `webrtc-datagram`:

```bash
cd ~/litep2p/fuzz/webrtc-datagram
timeout 300 cargo ziggy fuzz webrtc-datagram --release --no-honggfuzz -j 2 -o /tmp/smoke -i corpus || true
cargo afl whatsup -s /tmp/smoke/webrtc-datagram/afl     # the corpus/paths count must be GROWING
```

Confirm the **corpus (paths) count climbs**, not just execs. `webrtc-datagram` stands up its litep2p
server on a background thread built before the fuzz loop; if the fork server places that thread in a
different process than the instrumented target, every input silently skips at the handshake and
paths stay flat while execs tick up, which looks healthy and tests nothing. Flat paths on
datagram means stop and fix the harness (initialize the server inside the fuzz closure so it shares
the instrumented process) before committing the weekend. Smoke `webrtc-codec` and `webrtc-state` the
same way (without `--no-honggfuzz`); there paths should climb quickly.

## 6. Supervisor (systemd template, auto-restart, reboot-safe)

`/etc/systemd/system/ziggy@.service`, where the instance name `%i` is the target (= crate dir):

```ini
[Unit]
Description=ziggy webrtc fuzz (%i)

[Service]
User=fuzz
WorkingDirectory=/home/fuzz/litep2p/fuzz/%i
EnvironmentFile=/etc/ziggy/%i.env
Environment=AFL_SKIP_CPUFREQ=1
Environment=RUST_BACKTRACE=full
ExecStart=/home/fuzz/.cargo/bin/cargo ziggy fuzz %i --release -j ${JOBS} $EXTRA -o /data/fuzz-output -i ${SEEDS}
Restart=always
RestartSec=10
StartLimitIntervalSec=0
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
```

Per-target env files (15 of 16 cores; ziggy splits each `-j` roughly ⅔ AFL++ / ⅓ honggfuzz). `EXTRA`
carries per-target flags: `webrtc-datagram` runs **AFL++-only** (`--no-honggfuzz`), because its
threaded, socket-driven harness is the least tested under honggfuzz's persistent loop; codec and
state keep both engines. An empty `EXTRA` expands to no argument (it is unbraced in `ExecStart`).

```bash
sudo mkdir -p /etc/ziggy
printf 'JOBS=8\nEXTRA=\nSEEDS=/home/fuzz/litep2p/fuzz/webrtc-codec/corpus\n'                   | sudo tee /etc/ziggy/webrtc-codec.env
printf 'JOBS=5\nEXTRA=\nSEEDS=/home/fuzz/litep2p/fuzz/webrtc-state/corpus\n'                   | sudo tee /etc/ziggy/webrtc-state.env
printf 'JOBS=2\nEXTRA=--no-honggfuzz\nSEEDS=/home/fuzz/litep2p/fuzz/webrtc-datagram/corpus\n' | sudo tee /etc/ziggy/webrtc-datagram.env
```

Start all three:

```bash
sudo install -d -o fuzz -g fuzz /data/fuzz-output
sudo systemctl daemon-reload
sudo systemctl enable --now ziggy@webrtc-codec ziggy@webrtc-state ziggy@webrtc-datagram
```

`Restart=always` + `StartLimitIntervalSec=0` cover process crash, OOM kill, and reboot (systemd never
stops retrying). The sysctl drop-in and `cpu-performance.service` re-apply at boot; `-i ${SEEDS}`
re-imports seeds on every start.

## 7. Logs, monitoring, readable errors

Everything is under `/data/fuzz-output/<target>/`:

- `logs/{afl.log,afl_1.log,honggfuzz.log}` are the durable per-fuzzer logs (not the ANSI dashboard).
- `crashes/<timestamp>/<input>` holds one raw reproducer per file; `timeouts/<timestamp>/` holds hangs.
- Clean cross-instance status:
  `cargo afl whatsup -s /data/fuzz-output/<target>/afl` reports execs/s, paths, unique crashes, and
  **stability %**. Machine-readable per instance: `/data/fuzz-output/<target>/afl/*/fuzzer_stats`.
- Symbolized panic + backtrace for a crash (the unit sets `RUST_BACKTRACE=full`):
  ```bash
  cd ~/litep2p/fuzz/<target>
  cargo ziggy run -i /data/fuzz-output/<target>/crashes/<ts>/<file>
  ```
- Dedup crashes into unique stacks:
  `cd ~/litep2p/fuzz/<target> && cargo ziggy triage <target>`.

Optional hourly digest. A timer appends a one-line summary to `/data/fuzz-output/status.log`:

```bash
cat >/etc/systemd/system/ziggy-status.service <<'EOF'
[Unit]
Description=ziggy status digest
[Service]
Type=oneshot
User=fuzz
ExecStart=/bin/sh -c 'for t in webrtc-codec webrtc-state webrtc-datagram; do \
  printf "%s %s crashes=%s\n" "$(date -Is)" "$t" \
  "$(find /data/fuzz-output/$t/crashes -type f 2>/dev/null | wc -l)"; \
  cargo afl whatsup -s /data/fuzz-output/$t/afl 2>/dev/null | grep -E "cycles|corpus|exec|cov"; \
done >> /data/fuzz-output/status.log'
EOF
cat >/etc/systemd/system/ziggy-status.timer <<'EOF'
[Unit]
Description=ziggy status digest hourly
[Timer]
OnCalendar=hourly
[Install]
WantedBy=timers.target
EOF
systemctl enable --now ziggy-status.timer
```

## 8. Reproducibility checklist

- **Provenance** pinned in `~/PROVENANCE`: the recorded litep2p SHA (the branch tip), `rustc`,
  `cargo-afl`, `honggfuzz`, `ziggy` versions. Committed `Cargo.lock` per crate and committed seed
  corpora complete the pin.
- **Panic-is-a-crash check.** Feed a deliberately panicking input, confirm a file lands under
  `crashes/`, and that `cargo ziggy run -i` prints the backtrace. Do this before you walk away.
- **Replay by target.** `webrtc-codec` reproduces exactly from one file. `webrtc-state` often won't,
  so keep the whole crash dir and the op sequence (the timing and global-state caveat above).
  A `webrtc-datagram` crash in the framed noise-channel handling (even selector) or the post-auth
  substream negotiation (odd selector) is a real litep2p finding; a crash inside the handshake
  itself, before the channel opens, is str0m or environment, not litep2p.

## Verification (run before leaving it)

1. `cargo ziggy build --release` succeeded for all three targets.
2. `cargo test` green in `webrtc-codec`, `webrtc-state`, and `webrtc-datagram` (the last runs a real
   client-to-listener handshake spike).
3. `systemctl status ziggy@webrtc-codec` is `active (running)`; `afl-whatsup` shows execs and corpus
   rising for codec and state. `webrtc-datagram` advances much more slowly (one handshake per input),
   but its corpus/paths count must still climb. The preflight smoke (step 5) is what proves that,
   and flat paths there mean the harness is testing nothing.
4. Kill an AFL++ PID by hand; systemd restarts it within 10s and AFL++ resumes (corpus count does
   **not** reset to the seed count).
5. After a `reboot`, all three services come back `active`; `cat /proc/sys/kernel/core_pattern`
   prints `core`; the governor is `performance`.
6. The panic-input test from step 8 yields a crash file and a readable backtrace.
