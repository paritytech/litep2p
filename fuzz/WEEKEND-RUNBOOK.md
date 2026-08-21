# WebRTC fuzzing runbook (Ubuntu, 16-core EPYC, 48h unattended)

Runs the WebRTC ziggy harnesses (`webrtc-codec`, `webrtc-state`, `webrtc-datagram`) under AFL++ +
honggfuzz for ~2 days, supervised so it restarts through any failure and survives reboot, with
crashes and logs on disk and reproducible from a pinned commit.

Target commit: `paritytech/litep2p @ 3bf53d20` (branch `gab_webrtc_fuzzin`).
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
| `webrtc-state` | **Usually not from one file.** | Shares a process-global paused clock (virtual time accumulates across iterations), calls `PeerId::random()`, and binds a real UDP socket per iteration. Keep the whole crash dir + op sequence. A crash whose message is a bind `.expect()` is environment noise, not a finding. |
| `webrtc-datagram` | **No** (state persists across iterations). | Now ships a STUN seed corpus, so mutation starts past the demux and reaches `make_rtc` and opening-connection setup. It still can't complete DTLS (the server's per-run crypto desyncs any handshake), so depth past connection setup is limited and most of it is str0m. Worth a couple of cores, not more. |

## 1. Provision

16-core EPYC dedicated (real cores, stable clocks), ≥32 GB RAM, and **≥60 GB free disk**.
`fuzz/target` is already ~11 GB, and ziggy adds `target/afl` + `target/honggfuzz` per crate plus a
growing corpus and always-on logs. Put `/data` on the largest volume. Build and fuzz as a
**non-root sudo user** (`fuzz`); AFL++ prefers non-root.

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
git rev-parse HEAD | tee ~/PROVENANCE            # expect 3bf53d20...
{ rustc -V; cargo afl --version; ziggy --version; } >> ~/PROVENANCE

# all three targets ship committed corpora under fuzz/<target>/corpus (datagram's are STUN requests)

# build all three. --release activates the debug-assertions + overflow-checks profile in each crate.
# First build is slow: vendored OpenSSL + two instrumented binaries (AFL++ and honggfuzz) per target.
for t in webrtc-codec webrtc-state webrtc-datagram; do
  ( cd ~/litep2p/fuzz/$t && cargo ziggy build --release )
done
```

Do **not** `cargo update`. Each fuzz crate ships its own `Cargo.lock`; keep it for reproducible deps.

Preflight before committing the weekend (both catch a broken harness before it wastes 48h):

```bash
( cd ~/litep2p/fuzz/webrtc-codec && cargo test )   # corpus-decode + reaches-states self-checks
( cd ~/litep2p/fuzz/webrtc-state && cargo test )
( cd ~/litep2p/fuzz/webrtc-codec && cargo ziggy stability webrtc-codec -n 10 )   # expect ~100%
( cd ~/litep2p/fuzz/webrtc-state && cargo ziggy stability webrtc-state -n 10 )   # <100% expected
```

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
ExecStart=/home/fuzz/.cargo/bin/cargo ziggy fuzz %i --release -j ${JOBS} -o /data/fuzz-output -i ${SEEDS}
Restart=always
RestartSec=10
StartLimitIntervalSec=0
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
```

Per-target env files (15 of 16 cores; ziggy splits each `-j` roughly ⅔ AFL++ / ⅓ honggfuzz):

```bash
sudo mkdir -p /etc/ziggy
printf 'JOBS=8\nSEEDS=/home/fuzz/litep2p/fuzz/webrtc-codec/corpus\n'    | sudo tee /etc/ziggy/webrtc-codec.env
printf 'JOBS=5\nSEEDS=/home/fuzz/litep2p/fuzz/webrtc-state/corpus\n'    | sudo tee /etc/ziggy/webrtc-state.env
printf 'JOBS=2\nSEEDS=/home/fuzz/litep2p/fuzz/webrtc-datagram/corpus\n' | sudo tee /etc/ziggy/webrtc-datagram.env
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

- **Provenance** pinned in `~/PROVENANCE`: litep2p SHA `3bf53d20...`, `rustc`, `cargo-afl`, `honggfuzz`,
  `ziggy` versions. Committed `Cargo.lock` per crate and committed seed corpora complete the pin.
- **Panic-is-a-crash check.** Feed a deliberately panicking input, confirm a file lands under
  `crashes/`, and that `cargo ziggy run -i` prints the backtrace. Do this before you walk away.
- **Replay by target.** `webrtc-codec` reproduces exactly from one file. `webrtc-state` often won't,
  so keep the whole crash dir and the op sequence (the timing and global-state caveat above).
  `webrtc-datagram` crashes are str0m or environment noise, not litep2p findings.

## Verification (run before leaving it)

1. `cargo ziggy build --release` succeeded for all three targets.
2. `cargo test` green in `webrtc-codec` and `webrtc-state`.
3. `systemctl status ziggy@webrtc-codec` is `active (running)`; `afl-whatsup` shows execs and corpus
   rising for codec and state (datagram stays flat, which is expected).
4. Kill an AFL++ PID by hand; systemd restarts it within 10s and AFL++ resumes (corpus count does
   **not** reset to the seed count).
5. After a `reboot`, all three services come back `active`; `cat /proc/sys/kernel/core_pattern`
   prints `core`; the governor is `performance`.
6. The panic-input test from step 8 yields a crash file and a readable backtrace.
