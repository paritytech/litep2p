# Kademlia scaling benchmark

Tooling to characterize how libp2p-Kademlia (litep2p implementation) discovery latency,
completion rate and per-node cost change as the network grows — specifically to compare a
**small capability-scoped DHT** (hundreds–low thousands of nodes) against a **single
ecosystem-wide DHT** (10k–50k nodes).

Components:

| file | purpose |
|---|---|
| `examples/kad_bench.rs` | the benchmark binary: spawns N litep2p nodes in one process over loopback TCP, runs one benchmark, emits one JSON result row |
| `scripts/kad-bench-sweep.sh` | runs the (benchmark, N, params, seed) grid, resumable, optional tc-netem RTT injection |
| `scripts/kad-bench-analyze.py` | aggregates rows across seeds (mean ± 95% CI), prints scaling + small-vs-ecosystem tables, renders plots |

## Quick start

```sh
ulimit -n 1048576

# one run
cargo run --release --example kad_bench -- --bench b --nodes 1000 --seed 1 --out results.jsonl

# the full grid (>= 3 seeds per combo)
BENCHES="a b c" NODES="200 500 1000 2000 5000" SEEDS="1 2 3" ./scripts/kad-bench-sweep.sh

# aggregate + plots
python3 scripts/kad-bench-analyze.py kad-bench-sweep-*/results.jsonl --plots plots/
```

## Network model

* N nodes (`--nodes`), each a full litep2p instance with the Kademlia protocol, listening
  on `127.0.0.1:0`.
* Seed connectivity: every node learns `--bootstrap-links` (default 20) random peers, then
  runs a Kademlia bootstrap (`FIND_NODE(self)`), staggered at `--bootstrap-rate` per second.
* A `--warmup` window (default 60 s) lets routing tables settle; nothing measured during
  warm-up counts.
* Every node runs a background `FIND_NODE(random peer)` every `--background-interval`
  seconds (default 5) for the entire run.
* Optional churn: `--churn-pct-per-min P` restarts P% of the nodes per minute. A churned
  node drops its connections, stays down `--churn-downtime` seconds (default 30), rejoins
  with the same identity on a new port, re-seeds 20 random links and re-announces its
  content (providers / records).

## Benchmarks

**A — routing convergence (`--bench a`).** Each node keeps `--concurrent-lookups`
(default 8) `FIND_NODE` queries in flight, walking a per-node random permutation of all N
peer IDs. A unit is one of the N nodes; it is *resolved* when any node's lookup targeted at
it returns it among the k closest. This answers "is every node findable, and how fast".

**B — provider discovery (`--bench b`), the core test.** `--keys` (default 16) keys model
parachains; each gets `--providers-per-key` (default 3) provider nodes — a tiny provider
set in a large keyspace. Providers announce with `START_PROVIDING` and litep2p republishes
every `--republish-interval` seconds (default 5). After a `--converge` window (default
120 s), every node repeatedly runs `GET_PROVIDERS` for `--sample-keys` keys it does not
provide, retrying every `--retry-interval` seconds. A unit is a (node, key) pair; it is
resolved only when the **full** provider set is returned. This is the "can parachain A find
the 2–3 nodes of parachain B" question.

**C — record storage (`--bench c`).** Same shape as B but with Kademlia records: every node
`PUT_VALUE`s one record (key = own peer ID ++ `"/test"`, deterministic `--payload-bytes`
payload, default 64; also test 1024) with `--quorum one|majority|all` (majority =
⌈(k+1)/2⌉), re-putting every `--republish-interval` seconds. Readers `GET_VALUE` with the
same quorum; a unit resolves when a quorum-satisfying result with the correct payload is
returned. Compares the cost of storing values vs announcing presence; quorum is swept
because it strongly affects both latency and success rate.

## Discovery-over-time query (`--checkpoints`)

To ask "running the 8 parallel `FIND_NODE` lookups per node, how many nodes have we
discovered after 2 min and after 5 min", run bench A with checkpoints:

```sh
cargo run --release --example kad_bench -- \
    --bench a --nodes 1000 --concurrent-lookups 8 \
    --measure-timeout 300 --checkpoints 120,300 --seed 1 --out results.jsonl
# or across sizes/seeds:
BENCHES=a NODES="500 1000 2000" SEEDS="1 2 3" CHECKPOINTS="120,300" ./scripts/kad-bench-sweep.sh
```

At each checkpoint (seconds into the measurement window) every node is asked for an
immediate snapshot of its **routing-table size** — the count of distinct peers it has
discovered (peers with at least one known address). The result row records, per checkpoint,
the per-node distribution (mean/p50/p95/max), so the headline "on average how many nodes
discovered" is `discovery.checkpoints[i].avg_routing_table_size.mean`; the analyzer averages
this across seeds as mean ± 95% CI. Each checkpoint also records the network-wide
resolved-unit count/% at that instant. The measurement phase is held open until the last
checkpoint even if 100% of units resolve earlier, and `--measure-timeout` must be ≥ the last
checkpoint. The standard time-to-{50,80,90,95,99,100}% metrics are captured in the same run.

## Metrics (per run, one JSON row)

* **time-to-threshold** for {50, 80, 90, 95, 99, 100}% of units, `null` (DNF) if not
  reached within `--measure-timeout` (default 300 s = 5 min), plus **max % reached**.
* **discovery checkpoints** (when `--checkpoints` is set): per-node routing-table size
  (avg = nodes discovered per node) and network-wide resolved % at each checkpoint time.
* unit resolve time distribution (B/C: from measurement start incl. retries; A: latency of
  the resolving lookup) and attempts per unit.
* per-lookup latency distributions (median/p95/p99/max) and outcome counts
  (resolved / partial / not-found / failed / timeout) → unresolved-query rate.
* **hops** per successful lookup (median/p95). Counted as request/response exchanges
  performed by the iterative query — i.e. *peers successfully queried*; with parallelism α
  the effective path depth is roughly this value / α.
* **messages** sent + received per node (totals and steady-state msgs/s during the
  measurement window) and bytes in/out per node (transport level, includes noise/yamux
  overhead).
* per-node routing-table size, stored-record count/bytes and provider count over time
  (`series`, sampled every `--stats-interval` s) and at the end (`node_state_final`).
* B/C: per-key resolution success rate; C: put success rate, get quorum-failure rate
  (`failed` outcomes), value-mismatch count, stored bytes per node.
* environment: cores, RLIMIT_NOFILE, max RSS, active netem qdisc.

## Tunables (all logged into the result row)

`--nodes`, `--k` (replication factor / "the 20 closest"), `--alpha` (lookup parallelism),
`--concurrent-lookups` ("the 8 in parallel"), `--query-timeout`, `--latency-ms` +
`--jitter-ms`, `--churn-pct-per-min`, `--ttl`, `--republish-interval`, `--transport`,
`--seed`, and the B/C workload knobs (`--keys`, `--providers-per-key`, `--sample-keys`,
`--payload-bytes`, `--quorum`).

Run every combination with ≥ 3 seeds; the analyzer reports mean ± 95% CI (Student-t).

## Methodology notes & limitations

* **CPU saturation.** All N nodes share one host, so valid *latency* measurements live on
  the flat part of the latency-vs-load curve. With the default `--concurrent-lookups 8`, a
  16-core machine saturates somewhere around N≈500; latencies then measure host scheduling,
  not the protocol, and log-log extrapolations from saturated points are meaningless.
  Watch `env.load_avg_1m` in the result row (should stay near or below `cores`), and lower
  the offered load (`--concurrent-lookups 1-2`, larger `--retry-interval`,
  `--background-interval`) or use a bigger machine for large N. Completion-rate and
  message/storage-count metrics remain meaningful under moderate saturation; latency and
  time-to-threshold do not.
* **Transport.** Only `--transport sockets` (TCP over loopback) exists; litep2p has no
  in-process/memory transport. Loopback sockets bound the practical in-process network
  size via file descriptors (~20 FDs/node ⇒ `ulimit -n 1048576`) and CPU. 200–5000 nodes
  is the validated range; 10k+ on a single host requires a large machine and ideally
  splitting across processes/hosts. The analyzer extrapolates ecosystem-scale (10k–50k)
  numbers from the measured range via log-log fit and marks them as such.
* **Latency injection.** `--latency-ms/--jitter-ms` are *declared* values recorded in the
  row. Actual RTT injection is applied machine-wide by the sweep script with
  `tc qdisc replace dev lo root netem delay <latency>ms <jitter>ms` (requires root; RTT ≈
  2×latency). Per-connection heterogeneous latency is not supported. The binary warns when
  a non-zero latency is declared but no netem qdisc is active.
* **alpha (`--alpha`)** is exposed through a litep2p patch
  (`KademliaConfigBuilder::with_parallelism_factor`); k-bucket *capacity* remains fixed at
  20 in litep2p — `--k` sweeps the replication factor / closest-set size, not bucket size.
* The hop/message/state metrics come from a benchmark-oriented observability patch
  (`KademliaHandle::metrics()`); counters are maintained inside the Kademlia event loop and
  add no network behavior.
* Record-update propagation (re-put a key, time until all readers see the new value) is
  not yet implemented; the machinery in bench C (deterministic payloads + verified reads)
  is designed so it can be added as a follow-up phase.
* In bench A the "resolvable" definition is per targeted lookup (a unit resolves when a
  lookup *for that node* finds it), which makes 100% reachable and well-defined; nodes
  walk shuffled permutations so coverage of all N targets is fast and uniform.

## Sizing guidance

Rough per-run wall time: bootstrap (N / `--bootstrap-rate`) + warm-up (60 s) +
[publish + converge (≈130 s) for B/C] + measurement (up to 300 s) + drain (≤15 s).
The default full grid (3 benchmarks × 5 sizes × 3 seeds, plus the C quorum × payload
sub-grid) is ~100 runs ≈ 8–14 h; trim with the env knobs of the sweep script.
