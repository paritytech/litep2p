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

//! Kademlia scaling macro-benchmark: spawns N in-process litep2p nodes over loopback TCP
//! and measures how discovery latency, completion rate and per-node cost change with N.
//!
//! ```sh
//! ulimit -n 1048576
//! cargo run --release --example kad_bench -- --bench a --nodes 1000 --seed 1 --out results.jsonl
//! ```
//!
//! Network model: every node is seeded with `--bootstrap-links` random peers, bootstraps with
//! `FIND_NODE(self)`, then runs a background `FIND_NODE(random)` every `--background-interval`
//! seconds for the rest of the run. A `--warmup` window lets routing tables settle; nothing
//! observed during warm-up is part of the measurement.
//!
//! Benchmarks:
//!  * `--bench a` — routing convergence. Each node keeps `--concurrent-lookups` `FIND_NODE`
//!    queries in flight, walking a per-node random permutation of all peer IDs. A unit
//!    (= one of the N nodes) is "resolved" when any node's lookup for it returns it.
//!  * `--bench b` — provider discovery. `--keys` keys are each assigned
//!    `--providers-per-key` provider nodes which announce via `START_PROVIDING` and
//!    republish every `--republish-interval` seconds. After `--converge` seconds every node
//!    repeatedly runs `GET_PROVIDERS` for `--sample-keys` keys it does not provide until the
//!    FULL provider set is returned. A unit is one (node, key) pair.
//!  * `--bench c` — record storage. Every node `PUT_VALUE`s one record
//!    (key = peer_id ++ "/test", `--payload-bytes` deterministic payload, `--quorum`),
//!    re-putting every `--republish-interval` seconds. After `--converge` seconds every node
//!    repeatedly runs `GET_VALUE` (same quorum) for records of `--sample-keys` random other
//!    nodes until a quorum-satisfying, payload-correct value is returned. A unit is one
//!    (node, key) pair.
//!
//! For each threshold {50,80,90,95,99,100}% the time from measurement start until that
//! fraction of units resolved is reported (DNF/null if not reached within
//! `--measure-timeout`, default 5 minutes). One structured JSON row with all parameters and
//! metrics is appended to `--out`.
//!
//! Transport: only `--transport sockets` (TCP over 127.0.0.1) is supported; litep2p has no
//! in-process transport. `--latency-ms`/`--jitter-ms` are *declared* values recorded in the
//! result row — actual RTT injection is applied externally with tc-netem on `lo` (see
//! `scripts/kad-bench-sweep.sh`); the benchmark warns if a non-zero latency is declared but
//! no netem qdisc is present.

use futures::StreamExt;
use litep2p::{
    config::ConfigBuilder,
    crypto::ed25519::Keypair,
    protocol::libp2p::kademlia::{
        ConfigBuilder as KademliaConfigBuilder, KademliaEvent, KademliaHandle, QueryId, Quorum,
        Record, RecordKey,
    },
    transport::tcp::config::Config as TcpConfig,
    types::multiaddr::Multiaddr,
    BandwidthSink, Litep2p, PeerId,
};
use rand::{rngs::StdRng, seq::SliceRandom, Rng, SeedableRng};
use serde_json::json;
use tokio::sync::{mpsc, watch};

use std::{
    collections::{HashMap, HashSet},
    io::Write as _,
    num::NonZeroUsize,
    sync::{Arc, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Discovery thresholds (percent of units resolved) for which time-to-threshold is reported.
const THRESHOLDS: [f64; 6] = [50.0, 80.0, 90.0, 95.0, 99.0, 100.0];

/// Buckets for per-lookup hop histograms; hop counts are clamped to the last bucket.
const HOP_BUCKETS: usize = 64;

/// Reservoir size used for percentile estimation of high-volume metrics.
const RESERVOIR_CAP: usize = 200_000;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bench {
    A,
    B,
    C,
}

impl Bench {
    fn name(self) -> &'static str {
        match self {
            Bench::A => "a",
            Bench::B => "b",
            Bench::C => "c",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuorumArg {
    One,
    Majority,
    All,
}

impl QuorumArg {
    fn name(self) -> &'static str {
        match self {
            QuorumArg::One => "one",
            QuorumArg::Majority => "majority",
            QuorumArg::All => "all",
        }
    }

    fn to_quorum(self, k: usize) -> Quorum {
        match self {
            QuorumArg::One => Quorum::One,
            QuorumArg::Majority => {
                Quorum::N(NonZeroUsize::new(k / 2 + 1).expect("k/2+1 is non-zero"))
            }
            QuorumArg::All => Quorum::All,
        }
    }
}

#[derive(Debug, Clone)]
struct Args {
    bench: Bench,
    nodes: usize,
    k: usize,
    alpha: usize,
    concurrent_lookups: usize,
    query_timeout: u64,
    latency_ms: u64,
    jitter_ms: u64,
    churn_pct_per_min: f64,
    churn_downtime: u64,
    ttl: u64,
    republish_interval: u64,
    transport: String,
    seed: u64,
    keys: usize,
    providers_per_key: usize,
    sample_keys: usize,
    payload_bytes: usize,
    quorum: QuorumArg,
    bootstrap_links: usize,
    bootstrap_rate: f64,
    warmup: u64,
    converge: u64,
    measure_timeout: u64,
    background_interval: u64,
    retry_interval: u64,
    stats_interval: u64,
    checkpoints: Vec<u64>,
    run_id: String,
    out: Option<String>,
    csv: Option<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            bench: Bench::A,
            nodes: 200,
            k: 20,
            alpha: 3,
            concurrent_lookups: 8,
            query_timeout: 60,
            latency_ms: 0,
            jitter_ms: 0,
            churn_pct_per_min: 0.0,
            churn_downtime: 30,
            ttl: 7200,
            republish_interval: 5,
            transport: "sockets".into(),
            seed: 42,
            keys: 16,
            providers_per_key: 3,
            sample_keys: 8,
            payload_bytes: 64,
            quorum: QuorumArg::One,
            bootstrap_links: 20,
            bootstrap_rate: 200.0,
            warmup: 60,
            converge: 120,
            measure_timeout: 300,
            background_interval: 5,
            retry_interval: 5,
            stats_interval: 10,
            checkpoints: Vec::new(),
            run_id: String::new(),
            out: None,
            csv: None,
        }
    }
}

const USAGE: &str = "\
kad_bench — Kademlia scaling benchmark (see module docs)

  --bench a|b|c             benchmark: a=FIND_NODE, b=providers, c=records (default a)
  --nodes N                 network size (default 200)
  --k N                     replication factor (default 20)
  --alpha N                 lookup parallelism α (default 3)
  --concurrent-lookups N    parallel lookups per node (default 8)
  --query-timeout SECS      bench-side per-query timeout (default 60)
  --latency-ms MS           declared one-way tc-netem delay on lo (logged; default 0)
  --jitter-ms MS            declared tc-netem jitter (logged; default 0)
  --churn-pct-per-min PCT   % of nodes restarted per minute (default 0)
  --churn-downtime SECS     downtime before a churned node rejoins (default 30)
  --ttl SECS                record + provider record TTL (default 7200)
  --republish-interval SECS provider refresh / record re-put cadence (default 5)
  --transport sockets       transport (only 'sockets' = TCP loopback is supported)
  --seed N                  RNG seed (default 42)
  --keys M                  number of keys, bench b (default 16)
  --providers-per-key N     providers per key, bench b (default 3)
  --sample-keys N           keys sampled per node, bench b/c (default 8)
  --payload-bytes N         record payload size, bench c (default 64)
  --quorum one|majority|all put/get quorum, bench c (default one)
  --bootstrap-links N       random peers each node is seeded with (default 20)
  --bootstrap-rate N        bootstraps started per second (default 200)
  --warmup SECS             settle window after bootstrap (default 60)
  --converge SECS           publish convergence window, bench b/c (default 120)
  --measure-timeout SECS    DNF timeout for the measurement (default 300)
  --background-interval SECS background FIND_NODE cadence per node (default 5)
  --retry-interval SECS     retry cadence for unresolved units, bench b/c (default 5)
  --stats-interval SECS     per-node state sampling cadence (default 10)
  --checkpoints S1,S2,..    snapshot per-node avg routing-table size at these times (s)
                            into the measurement window, e.g. 120,300 (default none);
                            holds the measurement open until the last checkpoint
  --run-id STR              free-form label recorded in the result row
  --out PATH                append one JSON result row to this file
  --csv PATH                stream raw per-query samples to this CSV
";

fn parse_args() -> Args {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        if flag == "--help" || flag == "-h" {
            print!("{USAGE}");
            std::process::exit(0);
        }
        let value = it.next().unwrap_or_else(|| panic!("missing value for {flag}"));
        match flag.as_str() {
            "--bench" => {
                args.bench = match value.as_str() {
                    "a" | "A" => Bench::A,
                    "b" | "B" => Bench::B,
                    "c" | "C" => Bench::C,
                    other => panic!("unknown benchmark {other:?} (expected a|b|c)"),
                }
            }
            "--nodes" => args.nodes = value.parse().unwrap(),
            "--k" => args.k = value.parse().unwrap(),
            "--alpha" => args.alpha = value.parse().unwrap(),
            "--concurrent-lookups" => args.concurrent_lookups = value.parse().unwrap(),
            "--query-timeout" => args.query_timeout = value.parse().unwrap(),
            "--latency-ms" => args.latency_ms = value.parse().unwrap(),
            "--jitter-ms" => args.jitter_ms = value.parse().unwrap(),
            "--churn-pct-per-min" => args.churn_pct_per_min = value.parse().unwrap(),
            "--churn-downtime" => args.churn_downtime = value.parse().unwrap(),
            "--ttl" => args.ttl = value.parse().unwrap(),
            "--republish-interval" => args.republish_interval = value.parse().unwrap(),
            "--transport" => args.transport = value,
            "--seed" => args.seed = value.parse().unwrap(),
            "--keys" => args.keys = value.parse().unwrap(),
            "--providers-per-key" => args.providers_per_key = value.parse().unwrap(),
            "--sample-keys" => args.sample_keys = value.parse().unwrap(),
            "--payload-bytes" => args.payload_bytes = value.parse().unwrap(),
            "--quorum" => {
                args.quorum = match value.as_str() {
                    "one" => QuorumArg::One,
                    "majority" => QuorumArg::Majority,
                    "all" => QuorumArg::All,
                    other => panic!("unknown quorum {other:?} (expected one|majority|all)"),
                }
            }
            "--bootstrap-links" => args.bootstrap_links = value.parse().unwrap(),
            "--bootstrap-rate" => args.bootstrap_rate = value.parse().unwrap(),
            "--warmup" => args.warmup = value.parse().unwrap(),
            "--converge" => args.converge = value.parse().unwrap(),
            "--measure-timeout" => args.measure_timeout = value.parse().unwrap(),
            "--background-interval" => args.background_interval = value.parse().unwrap(),
            "--retry-interval" => args.retry_interval = value.parse().unwrap(),
            "--stats-interval" => args.stats_interval = value.parse().unwrap(),
            "--checkpoints" => {
                args.checkpoints = value
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse().expect("--checkpoints expects comma-separated seconds"))
                    .collect();
                args.checkpoints.sort_unstable();
                args.checkpoints.dedup();
            }
            "--run-id" => args.run_id = value,
            "--out" => args.out = Some(value),
            "--csv" => args.csv = Some(value),
            _ => panic!("unknown flag {flag} (see --help)"),
        }
    }

    assert!(args.nodes >= 2, "--nodes must be at least 2");
    assert!(args.k >= 1 && args.alpha >= 1 && args.concurrent_lookups >= 1);
    match args.transport.as_str() {
        "sockets" => {}
        "in-process" | "in_process" => {
            eprintln!(
                "error: litep2p has no in-process/memory transport; only `--transport sockets` \
                 (TCP over loopback) is supported"
            );
            std::process::exit(2);
        }
        other => panic!("unknown transport {other:?}"),
    }
    if args.bench == Bench::B {
        assert!(
            args.providers_per_key < args.nodes,
            "--providers-per-key must be < --nodes"
        );
    }
    if let Some(&last) = args.checkpoints.last() {
        // The measurement window must stay open long enough to reach every checkpoint.
        assert!(
            args.measure_timeout >= last,
            "--measure-timeout ({}s) must be >= the last --checkpoint ({last}s)",
            args.measure_timeout,
        );
    }
    args
}

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    Init,
    Bootstrap,
    Warmup,
    Publish,
    Converge,
    Measure,
    Drain,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bootstrap,
    Background,
    FindNode,
    GetProviders,
    GetRecord,
    PutRecord,
    Provide,
}

const KIND_COUNT: usize = 7;
const KIND_NAMES: [&str; KIND_COUNT] = [
    "bootstrap",
    "background",
    "find_node",
    "get_providers",
    "get_record",
    "put_record",
    "provide",
];

impl Kind {
    fn idx(self) -> usize {
        match self {
            Kind::Bootstrap => 0,
            Kind::Background => 1,
            Kind::FindNode => 2,
            Kind::GetProviders => 3,
            Kind::GetRecord => 4,
            Kind::PutRecord => 5,
            Kind::Provide => 6,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The attempt achieved the measurement goal (target found / full provider set /
    /// quorum-satisfying correct value).
    Resolved,
    /// Some, but not all, of the expected result was returned.
    Partial,
    /// The query succeeded but returned nothing useful.
    NotFound,
    /// The query failed (includes quorum failures).
    Failed,
    /// The bench-side query timeout expired.
    Timeout,
}

const OUTCOME_COUNT: usize = 5;
const OUTCOME_NAMES: [&str; OUTCOME_COUNT] =
    ["resolved", "partial", "not_found", "failed", "timeout"];

impl Outcome {
    fn idx(self) -> usize {
        match self {
            Outcome::Resolved => 0,
            Outcome::Partial => 1,
            Outcome::NotFound => 2,
            Outcome::Failed => 3,
            Outcome::Timeout => 4,
        }
    }
}

#[derive(Debug)]
struct NodeStatsMsg {
    elapsed_s: u32,
    rt_size: u32,
    num_records: u32,
    record_bytes: u64,
    provider_keys: u32,
    providers: u32,
    msgs_sent: u64,
    msgs_received: u64,
}

#[derive(Debug)]
struct NodeFinalMsg {
    msgs_sent: u64,
    msgs_received: u64,
    measure_msgs_sent: u64,
    measure_msgs_received: u64,
    measure_secs: f64,
    bytes_in: u64,
    bytes_out: u64,
    hops_hist: Vec<[u32; HOP_BUCKETS]>,
    restarts: u32,
    aborted_inflight: u32,
    value_mismatches: u32,
    rt_size: u32,
    num_records: u32,
    record_bytes: u64,
    provider_keys: u32,
    providers: u32,
}

#[derive(Debug)]
enum Report {
    Ready,
    BootstrapDone,
    PublishDone {
        ok: u32,
        failed: u32,
    },
    Sample {
        kind: Kind,
        outcome: Outcome,
        latency_us: u32,
    },
    UnitResolved {
        unit: u32,
        key_idx: u32,
        elapsed_ms: u32,
        attempts: u32,
    },
    NodeStats(NodeStatsMsg),
    /// Response to a `Ctl::Snapshot` checkpoint: the node's current routing-table size
    /// (number of distinct peers discovered) and locally-resolved target count.
    Checkpoint {
        idx: usize,
        rt_size: u32,
        resolved_local: u32,
    },
    NodeFinal(NodeFinalMsg),
}

#[derive(Debug)]
enum Ctl {
    Restart,
    /// Request an immediate routing-table-size snapshot for checkpoint `idx`.
    Snapshot {
        idx: usize,
    },
}

/// Static, deterministic description of the workload, shared by all node tasks.
struct Plan {
    peers: Vec<PeerId>,
    /// Benchmark B keys by index.
    bkeys: Vec<RecordKey>,
    /// Benchmark B: provider peers per key index.
    expected_providers: Vec<Vec<PeerId>>,
    /// Benchmark B: key indices provided per node.
    provide_keys: Vec<Vec<u32>>,
    /// Benchmark B/C: sampled key indices per node (B: into `bkeys`, C: owner node index).
    samples: Vec<Vec<u32>>,
    /// Benchmark B/C: global unit index of each node's first sample slot.
    unit_base: Vec<u32>,
    /// Total number of discovery units (A: nodes, B/C: (node, key) pairs).
    total_units: u32,
    /// Benchmark B/C: number of nodes sampling each key index.
    samplers_per_key: Vec<u32>,
}

/// Deterministic payload for benchmark C records of node `idx`.
fn record_payload(seed: u64, idx: usize, len: usize) -> Vec<u8> {
    let mut rng = StdRng::seed_from_u64(seed ^ 0x9e37_79b9_7f4a_7c15u64.wrapping_add(idx as u64));
    let mut payload = vec![0u8; len];
    rng.fill(payload.as_mut_slice());
    payload
}

/// Benchmark C record key of node `idx`: peer_id ++ "/test".
fn record_key(peers: &[PeerId], idx: usize) -> RecordKey {
    let mut key = peers[idx].to_bytes();
    key.extend_from_slice(b"/test");
    RecordKey::from(key)
}

fn build_plan(args: &Args, peers: Vec<PeerId>) -> Plan {
    let n = args.nodes;
    let mut rng = StdRng::seed_from_u64(args.seed.wrapping_mul(0x517c_c1b7_2722_0a95));

    let mut bkeys = Vec::new();
    let mut expected_providers = Vec::new();
    let mut provide_keys = vec![Vec::new(); n];
    if args.bench == Bench::B {
        for key_idx in 0..args.keys {
            bkeys.push(RecordKey::from(
                format!("kad-bench/key/{key_idx}").into_bytes(),
            ));
            let mut providers = HashSet::new();
            while providers.len() < args.providers_per_key {
                providers.insert(rng.gen_range(0..n));
            }
            let mut providers: Vec<usize> = providers.into_iter().collect();
            providers.sort_unstable();
            for &node in &providers {
                provide_keys[node].push(key_idx as u32);
            }
            expected_providers.push(providers.into_iter().map(|node| peers[node]).collect());
        }
    }

    // Sample assignment: per node, a deterministic random sample of keys it neither provides
    // (B) nor owns (C).
    let key_count = match args.bench {
        Bench::A => 0,
        Bench::B => args.keys,
        Bench::C => n,
    };
    let mut samples = vec![Vec::new(); n];
    let mut samplers_per_key = vec![0u32; key_count];
    if args.bench != Bench::A {
        for (node, node_samples) in samples.iter_mut().enumerate() {
            let own: HashSet<u32> = match args.bench {
                Bench::B => provide_keys[node].iter().copied().collect(),
                _ => std::iter::once(node as u32).collect(),
            };
            let mut candidates: Vec<u32> =
                (0..key_count as u32).filter(|key| !own.contains(key)).collect();
            candidates.shuffle(&mut rng);
            candidates.truncate(args.sample_keys);
            for &key in &candidates {
                samplers_per_key[key as usize] += 1;
            }
            *node_samples = candidates;
        }
    }

    let mut unit_base = vec![0u32; n];
    let mut total_units = 0u32;
    match args.bench {
        Bench::A => total_units = n as u32,
        _ => {
            for node in 0..n {
                unit_base[node] = total_units;
                total_units += samples[node].len() as u32;
            }
        }
    }

    Plan {
        peers,
        bkeys,
        expected_providers,
        provide_keys,
        samples,
        unit_base,
        total_units,
        samplers_per_key,
    }
}

// ---------------------------------------------------------------------------
// Node task
// ---------------------------------------------------------------------------

struct Pending {
    kind: Kind,
    started: Instant,
    /// Slot index for measurement attempts (B/C).
    slot: Option<usize>,
    /// Target peer index for measurement attempts (A).
    target: Option<u32>,
    /// Benchmark C: number of partial results carrying the expected payload.
    matching: u32,
}

struct Slot {
    key_idx: u32,
    resolved: bool,
    inflight: bool,
    next_attempt: Instant,
    attempts: u32,
}

struct Instance {
    litep2p: Litep2p,
    handle: KademliaHandle,
    bw: BandwidthSink,
    pending: HashMap<QueryId, Pending>,
}

#[derive(Default, Clone, Copy)]
struct Counters {
    msgs_sent: u64,
    msgs_received: u64,
    bytes_in: u64,
    bytes_out: u64,
}

enum DriveExit {
    Done,
    Restart,
}

struct NodeTask {
    idx: usize,
    keypair: Keypair,
    args: Arc<Args>,
    t0: Instant,
    plan: Arc<Plan>,
    book: Arc<RwLock<Vec<Vec<Multiaddr>>>>,
    tx: mpsc::Sender<Report>,
    phase_rx: watch::Receiver<Phase>,
    ctl_rx: mpsc::Receiver<Ctl>,
    rng: StdRng,

    phase: Phase,
    slots: Vec<Slot>,
    bootstrap_reported: bool,
    publish_reported: bool,
    publish_ok: u32,
    publish_failed: u32,
    publish_left: u32,
    base: Counters,
    /// Cumulative (msgs_sent, msgs_received) and timestamp captured when `Measure` started.
    measure_mark: Option<(Instant, u64, u64)>,
    measure_end: Option<(Instant, u64, u64)>,
    hops_hist: Vec<[u32; HOP_BUCKETS]>,
    issued: HashMap<QueryId, Kind>,
    restarts: u32,
    aborted_inflight: u32,
    value_mismatches: u32,
    final_sent: bool,

    // Benchmark A.
    perm: Vec<u32>,
    perm_pos: usize,
    resolved_local: HashSet<u32>,
    inflight_a: usize,

    next_background: Instant,
    next_republish: Instant,
    next_stats: Instant,
    last_snapshot: (u32, u32, u64, u32, u32),
}

impl NodeTask {
    fn quorum(&self) -> Quorum {
        self.args.quorum.to_quorum(self.args.k)
    }

    fn spawn_instance(&mut self) -> Instance {
        let (kad_config, handle) = KademliaConfigBuilder::new()
            .with_replication_factor(self.args.k)
            .with_parallelism_factor(self.args.alpha)
            .with_record_ttl(Duration::from_secs(self.args.ttl))
            .with_provider_record_ttl(Duration::from_secs(self.args.ttl))
            .with_provider_refresh_interval(Duration::from_secs(self.args.republish_interval))
            .with_max_records(65536)
            .with_max_provider_keys(65536)
            .build();
        let config = ConfigBuilder::new()
            .with_keypair(self.keypair.clone())
            .with_tcp(TcpConfig {
                listen_addresses: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
                ..Default::default()
            })
            .with_libp2p_kademlia(kad_config)
            .build();
        let litep2p = Litep2p::new(config).expect("failed to create litep2p node");
        let bw = litep2p.bandwidth_sink();

        let addresses: Vec<Multiaddr> = litep2p.listen_addresses().cloned().collect();
        self.book.write().expect("book lock").as_mut_slice()[self.idx] = addresses;

        Instance {
            litep2p,
            handle,
            bw,
            pending: HashMap::new(),
        }
    }

    /// Seed the routing table with random peers and start `FIND_NODE(self)`.
    async fn bootstrap(&mut self, st: &mut Instance, kind: Kind) {
        let links = self.args.bootstrap_links.min(self.args.nodes - 1);
        let mut chosen = HashSet::new();
        while chosen.len() < links {
            let other = self.rng.gen_range(0..self.args.nodes);
            if other != self.idx {
                chosen.insert(other);
            }
        }
        let links: Vec<(PeerId, Vec<Multiaddr>)> = {
            let book = self.book.read().expect("book lock");
            chosen
                .into_iter()
                .filter(|other| !book[*other].is_empty())
                .map(|other| (self.plan.peers[other], book[other].clone()))
                .collect()
        };
        for (peer, addresses) in links {
            st.handle.add_known_peer(peer, addresses).await;
        }

        let query_id = st.handle.find_node(self.plan.peers[self.idx]).await;
        self.track(
            st,
            query_id,
            Pending {
                kind,
                started: Instant::now(),
                slot: None,
                target: None,
                matching: 0,
            },
        );
    }

    fn track(&mut self, st: &mut Instance, query_id: QueryId, pending: Pending) {
        self.issued.insert(query_id, pending.kind);
        st.pending.insert(query_id, pending);
    }

    /// Announce / publish this node's content (benchmark B providers, benchmark C records).
    async fn publish(&mut self, st: &mut Instance, count_results: bool) {
        match self.args.bench {
            Bench::A => {}
            Bench::B => {
                for &key_idx in &self.plan.provide_keys[self.idx].clone() {
                    let key = self.plan.bkeys[key_idx as usize].clone();
                    let query_id = st.handle.start_providing(key, Quorum::One).await;
                    if count_results {
                        self.publish_left += 1;
                        self.track(
                            st,
                            query_id,
                            Pending {
                                kind: Kind::Provide,
                                started: Instant::now(),
                                slot: None,
                                target: None,
                                matching: 0,
                            },
                        );
                    } else {
                        self.issued.insert(query_id, Kind::Provide);
                    }
                }
            }
            Bench::C => {
                let key = record_key(&self.plan.peers, self.idx);
                let payload = record_payload(self.args.seed, self.idx, self.args.payload_bytes);
                let query_id = st.handle.put_record(Record::new(key, payload), self.quorum()).await;
                if count_results {
                    self.publish_left += 1;
                    self.track(
                        st,
                        query_id,
                        Pending {
                            kind: Kind::PutRecord,
                            started: Instant::now(),
                            slot: None,
                            target: None,
                            matching: 0,
                        },
                    );
                } else {
                    self.issued.insert(query_id, Kind::PutRecord);
                }
            }
        }

        if count_results && self.publish_left == 0 && !self.publish_reported {
            self.publish_reported = true;
            let _ = self.tx.send(Report::PublishDone { ok: 0, failed: 0 }).await;
        }
    }

    fn on_publish_result(&mut self, ok: bool) {
        if ok {
            self.publish_ok += 1;
        } else {
            self.publish_failed += 1;
        }
        self.publish_left = self.publish_left.saturating_sub(1);
        if self.publish_left == 0 && !self.publish_reported {
            self.publish_reported = true;
            let _ = self.tx.try_send(Report::PublishDone {
                ok: self.publish_ok,
                failed: self.publish_failed,
            });
        }
    }

    /// Cumulative message counters: accumulated bases from previous instances + a live
    /// snapshot of the current one. Also folds completed-lookup hop counts into histograms.
    async fn snapshot(&mut self, st: &mut Instance) -> (u64, u64) {
        let Some(metrics) = st.handle.metrics().await else {
            return (self.base.msgs_sent, self.base.msgs_received);
        };

        for (query_id, hops) in metrics.completed_lookups {
            if let Some(kind) = self.issued.remove(&query_id) {
                self.hops_hist[kind.idx()][(hops as usize).min(HOP_BUCKETS - 1)] += 1;
            }
        }
        self.last_snapshot = (
            metrics.routing_table_size as u32,
            metrics.num_records as u32,
            metrics.record_bytes as u64,
            metrics.num_provider_keys as u32,
            metrics.num_providers as u32,
        );

        (
            self.base.msgs_sent + metrics.messages_sent,
            self.base.msgs_received + metrics.messages_received,
        )
    }

    async fn send_stats(&mut self, st: &mut Instance) {
        let (msgs_sent, msgs_received) = self.snapshot(st).await;
        let (rt_size, num_records, record_bytes, provider_keys, providers) = self.last_snapshot;
        let _ = self
            .tx
            .send(Report::NodeStats(NodeStatsMsg {
                elapsed_s: self.t0.elapsed().as_secs() as u32,
                rt_size,
                num_records,
                record_bytes,
                provider_keys,
                providers,
                msgs_sent,
                msgs_received,
            }))
            .await;
    }

    async fn send_final(&mut self, st: &mut Instance) {
        if self.final_sent {
            return;
        }
        self.final_sent = true;

        let (msgs_sent, msgs_received) = self.snapshot(st).await;
        self.measure_end = Some((Instant::now(), msgs_sent, msgs_received));
        let (mark_at, mark_sent, mark_received) =
            self.measure_mark.unwrap_or((Instant::now(), msgs_sent, msgs_received));
        let (rt_size, num_records, record_bytes, provider_keys, providers) = self.last_snapshot;

        let _ = self
            .tx
            .send(Report::NodeFinal(NodeFinalMsg {
                msgs_sent,
                msgs_received,
                measure_msgs_sent: msgs_sent.saturating_sub(mark_sent),
                measure_msgs_received: msgs_received.saturating_sub(mark_received),
                measure_secs: mark_at.elapsed().as_secs_f64(),
                bytes_in: self.base.bytes_in + st.bw.inbound() as u64,
                bytes_out: self.base.bytes_out + st.bw.outbound() as u64,
                hops_hist: self.hops_hist.clone(),
                restarts: self.restarts,
                aborted_inflight: self.aborted_inflight,
                value_mismatches: self.value_mismatches,
                rt_size,
                num_records,
                record_bytes,
                provider_keys,
                providers,
            }))
            .await;
    }

    /// Issue one measurement attempt for benchmark A.
    async fn issue_find_node(&mut self, st: &mut Instance) {
        // Walk the shuffled permutation, skipping self and locally-resolved targets; if a
        // full cycle finds nothing unresolved, fall back to a random target to keep the
        // offered load constant.
        let mut target = None;
        for _ in 0..self.perm.len() {
            let candidate = self.perm[self.perm_pos];
            self.perm_pos = (self.perm_pos + 1) % self.perm.len();
            if candidate as usize != self.idx && !self.resolved_local.contains(&candidate) {
                target = Some(candidate);
                break;
            }
        }
        let target = target.unwrap_or_else(|| loop {
            let candidate = self.rng.gen_range(0..self.args.nodes) as u32;
            if candidate as usize != self.idx {
                break candidate;
            }
        });

        let query_id = st.handle.find_node(self.plan.peers[target as usize]).await;
        self.inflight_a += 1;
        self.track(
            st,
            query_id,
            Pending {
                kind: Kind::FindNode,
                started: Instant::now(),
                slot: None,
                target: Some(target),
                matching: 0,
            },
        );
    }

    /// Issue one measurement attempt for a benchmark B/C slot.
    async fn issue_slot(&mut self, st: &mut Instance, slot_idx: usize) {
        let key_idx = self.slots[slot_idx].key_idx;
        let (query_id, kind) = match self.args.bench {
            Bench::B => {
                let key = self.plan.bkeys[key_idx as usize].clone();
                (st.handle.get_providers(key).await, Kind::GetProviders)
            }
            _ => {
                let key = record_key(&self.plan.peers, key_idx as usize);
                (
                    st.handle.get_record(key, self.quorum()).await,
                    Kind::GetRecord,
                )
            }
        };
        self.slots[slot_idx].inflight = true;
        self.slots[slot_idx].attempts += 1;
        self.track(
            st,
            query_id,
            Pending {
                kind,
                started: Instant::now(),
                slot: Some(slot_idx),
                target: None,
                matching: 0,
            },
        );
    }

    async fn report_sample(&mut self, kind: Kind, outcome: Outcome, latency: Duration) {
        if self.phase == Phase::Measure {
            let _ = self
                .tx
                .send(Report::Sample {
                    kind,
                    outcome,
                    latency_us: latency.as_micros().min(u32::MAX as u128) as u32,
                })
                .await;
        }
    }

    /// A measurement attempt finished; update its slot and report.
    async fn finish_slot_attempt(
        &mut self,
        slot_idx: usize,
        kind: Kind,
        outcome: Outcome,
        latency: Duration,
    ) {
        self.report_sample(kind, outcome, latency).await;

        let retry = Duration::from_secs(self.args.retry_interval);
        let slot = &mut self.slots[slot_idx];
        slot.inflight = false;
        if outcome == Outcome::Resolved && !slot.resolved {
            slot.resolved = true;
            let attempts = slot.attempts;
            let key_idx = slot.key_idx;
            let elapsed_ms = self
                .measure_mark
                .map(|(at, _, _)| at.elapsed().as_millis().min(u32::MAX as u128) as u32)
                .unwrap_or(0);
            let _ = self
                .tx
                .send(Report::UnitResolved {
                    unit: self.plan.unit_base[self.idx] + slot_idx as u32,
                    key_idx,
                    elapsed_ms,
                    attempts,
                })
                .await;
        } else {
            slot.next_attempt = Instant::now() + retry;
        }
    }

    async fn on_kad_event(&mut self, st: &mut Instance, event: KademliaEvent) {
        let (query_id, payload) = match event {
            KademliaEvent::FindNodeSuccess {
                query_id, peers, ..
            } => (query_id, Ok(Some(peers))),
            KademliaEvent::GetProvidersSuccess {
                query_id,
                providers,
                ..
            } => {
                let Some(pending) = st.pending.remove(&query_id) else {
                    return;
                };
                let latency = pending.started.elapsed();
                if let Some(slot_idx) = pending.slot {
                    let expected =
                        &self.plan.expected_providers[self.slots[slot_idx].key_idx as usize];
                    let found = expected
                        .iter()
                        .filter(|peer| providers.iter().any(|provider| provider.peer == **peer))
                        .count();
                    let outcome = if found == expected.len() {
                        Outcome::Resolved
                    } else if found > 0 {
                        Outcome::Partial
                    } else {
                        Outcome::NotFound
                    };
                    self.finish_slot_attempt(slot_idx, pending.kind, outcome, latency).await;
                }
                return;
            }
            KademliaEvent::GetRecordPartialResult { query_id, record } => {
                if let Some(pending) = st.pending.get_mut(&query_id) {
                    if let Some(slot_idx) = pending.slot {
                        let owner = self.slots[slot_idx].key_idx as usize;
                        let expected =
                            record_payload(self.args.seed, owner, self.args.payload_bytes);
                        if record.record.value == expected {
                            pending.matching += 1;
                        }
                    }
                }
                return;
            }
            KademliaEvent::GetRecordSuccess { query_id } => {
                let Some(pending) = st.pending.remove(&query_id) else {
                    return;
                };
                let latency = pending.started.elapsed();
                if let Some(slot_idx) = pending.slot {
                    let outcome = if pending.matching > 0 {
                        Outcome::Resolved
                    } else {
                        self.value_mismatches += 1;
                        Outcome::Failed
                    };
                    self.finish_slot_attempt(slot_idx, pending.kind, outcome, latency).await;
                }
                return;
            }
            KademliaEvent::PutRecordSuccess { query_id, .. } => (query_id, Err(true)),
            KademliaEvent::AddProviderSuccess { query_id, .. } => (query_id, Err(true)),
            KademliaEvent::QueryFailed { query_id } => (query_id, Err(false)),
            _ => return,
        };

        let Some(pending) = st.pending.remove(&query_id) else {
            return;
        };
        let latency = pending.started.elapsed();

        match payload {
            // `FIND_NODE` results.
            Ok(Some(peers)) => match pending.kind {
                Kind::Bootstrap => {
                    if !self.bootstrap_reported {
                        self.bootstrap_reported = true;
                        let _ = self.tx.send(Report::BootstrapDone).await;
                    }
                }
                Kind::FindNode => {
                    self.inflight_a = self.inflight_a.saturating_sub(1);
                    let target = pending.target.expect("find_node attempts have a target");
                    let target_peer = self.plan.peers[target as usize];
                    let found = peers.iter().any(|(peer, _)| *peer == target_peer);
                    let outcome = if found {
                        self.resolved_local.insert(target);
                        Outcome::Resolved
                    } else if peers.is_empty() {
                        Outcome::NotFound
                    } else {
                        Outcome::Partial
                    };
                    self.report_sample(Kind::FindNode, outcome, latency).await;
                    if found {
                        let _ = self
                            .tx
                            .send(Report::UnitResolved {
                                unit: target,
                                key_idx: u32::MAX,
                                elapsed_ms: latency.as_millis().min(u32::MAX as u128) as u32,
                                attempts: 1,
                            })
                            .await;
                    }
                }
                _ => {}
            },
            // `PUT_VALUE` / `ADD_PROVIDER` success and `QueryFailed` for any kind.
            Err(success) => match pending.kind {
                Kind::Bootstrap => {
                    if !self.bootstrap_reported {
                        self.bootstrap_reported = true;
                        let _ = self.tx.send(Report::BootstrapDone).await;
                    }
                }
                Kind::Provide | Kind::PutRecord if !self.publish_reported => {
                    self.on_publish_result(success);
                }
                Kind::FindNode => {
                    self.inflight_a = self.inflight_a.saturating_sub(1);
                    self.report_sample(Kind::FindNode, Outcome::Failed, latency).await;
                }
                Kind::GetProviders | Kind::GetRecord => {
                    if let Some(slot_idx) = pending.slot {
                        self.finish_slot_attempt(slot_idx, pending.kind, Outcome::Failed, latency)
                            .await;
                    }
                }
                _ => {}
            },
            Ok(None) => unreachable!(),
        }
    }

    async fn on_phase_change(&mut self, st: &mut Instance, phase: Phase) {
        self.phase = phase;
        match phase {
            Phase::Warmup => {
                self.next_background = Instant::now()
                    + Duration::from_millis(
                        self.rng.gen_range(0..self.args.background_interval.max(1) * 1000),
                    );
                self.next_stats = Instant::now()
                    + Duration::from_millis(
                        self.rng.gen_range(0..self.args.stats_interval.max(1) * 1000),
                    );
            }
            Phase::Publish => {
                self.publish(st, true).await;
                self.next_republish =
                    Instant::now() + Duration::from_secs(self.args.republish_interval.max(1));
            }
            Phase::Measure => {
                let (msgs_sent, msgs_received) = self.snapshot(st).await;
                self.measure_mark = Some((Instant::now(), msgs_sent, msgs_received));
                let now = Instant::now();
                for slot in &mut self.slots {
                    // Spread the initial burst over a second.
                    slot.next_attempt = now + Duration::from_millis(self.rng.gen_range(0..1000));
                }
            }
            Phase::Drain => {
                self.send_final(st).await;
            }
            _ => {}
        }
    }

    async fn on_tick(&mut self, st: &mut Instance) {
        let now = Instant::now();
        let query_timeout = Duration::from_secs(self.args.query_timeout);

        // Sweep bench-side query timeouts.
        let expired: Vec<QueryId> = st
            .pending
            .iter()
            .filter(|(_, pending)| now.duration_since(pending.started) > query_timeout)
            .map(|(query_id, _)| *query_id)
            .collect();
        for query_id in expired {
            let pending = st.pending.remove(&query_id).expect("expired query is pending");
            self.issued.remove(&query_id);
            match pending.kind {
                Kind::Bootstrap => {
                    if !self.bootstrap_reported {
                        self.bootstrap_reported = true;
                        let _ = self.tx.send(Report::BootstrapDone).await;
                    }
                }
                Kind::Provide | Kind::PutRecord if !self.publish_reported => {
                    self.on_publish_result(false)
                }
                Kind::FindNode => {
                    self.inflight_a = self.inflight_a.saturating_sub(1);
                    self.report_sample(Kind::FindNode, Outcome::Timeout, query_timeout).await;
                }
                Kind::GetProviders | Kind::GetRecord => {
                    if let Some(slot_idx) = pending.slot {
                        self.finish_slot_attempt(
                            slot_idx,
                            pending.kind,
                            Outcome::Timeout,
                            query_timeout,
                        )
                        .await;
                    }
                }
                _ => {}
            }
        }

        let active = matches!(
            self.phase,
            Phase::Warmup | Phase::Publish | Phase::Converge | Phase::Measure
        );

        // Background FIND_NODE for a random peer every --background-interval seconds.
        if active && now >= self.next_background {
            self.next_background = now + Duration::from_secs(self.args.background_interval.max(1));
            let target = loop {
                let candidate = self.rng.gen_range(0..self.args.nodes);
                if candidate != self.idx {
                    break candidate;
                }
            };
            let query_id = st.handle.find_node(self.plan.peers[target]).await;
            self.track(
                st,
                query_id,
                Pending {
                    kind: Kind::Background,
                    started: now,
                    slot: None,
                    target: None,
                    matching: 0,
                },
            );
        }

        // Benchmark C: re-put own record every --republish-interval seconds.
        if self.args.bench == Bench::C
            && matches!(self.phase, Phase::Converge | Phase::Measure)
            && now >= self.next_republish
        {
            self.next_republish = now + Duration::from_secs(self.args.republish_interval.max(1));
            self.publish(st, false).await;
        }

        // Measurement load.
        if self.phase == Phase::Measure {
            match self.args.bench {
                Bench::A => {
                    while self.inflight_a < self.args.concurrent_lookups {
                        self.issue_find_node(st).await;
                    }
                }
                _ => {
                    let inflight = self.slots.iter().filter(|slot| slot.inflight).count();
                    let mut budget = self.args.concurrent_lookups.saturating_sub(inflight);
                    for slot_idx in 0..self.slots.len() {
                        if budget == 0 {
                            break;
                        }
                        let slot = &self.slots[slot_idx];
                        if !slot.resolved && !slot.inflight && now >= slot.next_attempt {
                            self.issue_slot(st, slot_idx).await;
                            budget -= 1;
                        }
                    }
                }
            }
        }

        // Periodic node state samples.
        if active && now >= self.next_stats {
            self.next_stats = now + Duration::from_secs(self.args.stats_interval.max(1));
            self.send_stats(st).await;
        }
    }

    /// Inner event loop on one litep2p instance. Returns on `Done` or on a churn restart.
    async fn drive(&mut self, st: &mut Instance) -> DriveExit {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = st.litep2p.next_event() => {}
                event = st.handle.next() => {
                    let Some(event) = event else { return DriveExit::Done };
                    self.on_kad_event(st, event).await;
                }
                changed = self.phase_rx.changed() => {
                    if changed.is_err() {
                        return DriveExit::Done;
                    }
                    let phase = *self.phase_rx.borrow_and_update();
                    self.on_phase_change(st, phase).await;
                    if phase == Phase::Done {
                        return DriveExit::Done;
                    }
                }
                ctl = self.ctl_rx.recv() => {
                    match ctl {
                        Some(Ctl::Restart) => return DriveExit::Restart,
                        Some(Ctl::Snapshot { idx }) => {
                            self.snapshot(st).await;
                            let _ = self
                                .tx
                                .send(Report::Checkpoint {
                                    idx,
                                    rt_size: self.last_snapshot.0,
                                    resolved_local: self.resolved_local.len() as u32,
                                })
                                .await;
                        }
                        None => {}
                    }
                }
                _ = tick.tick() => self.on_tick(st).await,
            }
        }
    }

    async fn run(mut self, bootstrap_delay: Duration) {
        let mut st = self.spawn_instance();
        let _ = self.tx.send(Report::Ready).await;

        // Wait for the coordinator to open the bootstrap phase, then stagger.
        while *self.phase_rx.borrow() < Phase::Bootstrap {
            if self.phase_rx.changed().await.is_err() {
                return;
            }
        }
        self.phase = *self.phase_rx.borrow_and_update();
        tokio::time::sleep(bootstrap_delay).await;
        self.bootstrap(&mut st, Kind::Bootstrap).await;
        // Catch up on phases that advanced while sleeping.
        let phase = *self.phase_rx.borrow();
        if phase > self.phase && phase != Phase::Done {
            for next in [
                Phase::Warmup,
                Phase::Publish,
                Phase::Converge,
                Phase::Measure,
            ] {
                if next <= phase && next > self.phase {
                    self.on_phase_change(&mut st, next).await;
                }
            }
        }

        loop {
            match self.drive(&mut st).await {
                DriveExit::Done => {
                    self.send_final(&mut st).await;
                    return;
                }
                DriveExit::Restart => {
                    // Leave: accumulate counters, abort in-flight work, drop the instance.
                    if let Some(metrics) = st.handle.metrics().await {
                        for (query_id, hops) in metrics.completed_lookups {
                            if let Some(kind) = self.issued.remove(&query_id) {
                                self.hops_hist[kind.idx()][(hops as usize).min(HOP_BUCKETS - 1)] +=
                                    1;
                            }
                        }
                        self.base.msgs_sent += metrics.messages_sent;
                        self.base.msgs_received += metrics.messages_received;
                    }
                    self.base.bytes_in += st.bw.inbound() as u64;
                    self.base.bytes_out += st.bw.outbound() as u64;
                    self.aborted_inflight += st.pending.len() as u32;
                    self.issued.clear();
                    self.inflight_a = 0;
                    for slot in &mut self.slots {
                        slot.inflight = false;
                    }
                    self.restarts += 1;
                    drop(st);

                    // Downtime; bail out early if the run finishes meanwhile.
                    let deadline = Instant::now() + Duration::from_secs(self.args.churn_downtime);
                    loop {
                        let phase = *self.phase_rx.borrow();
                        if phase >= Phase::Drain {
                            let report = Report::NodeFinal(self.final_offline());
                            let _ = self.tx.send(report).await;
                            return;
                        }
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(remaining) => break,
                            changed = self.phase_rx.changed() => {
                                if changed.is_err() {
                                    return;
                                }
                                self.phase_rx.borrow_and_update();
                            }
                        }
                    }

                    // Rejoin: fresh instance, fresh links, re-announce own content.
                    st = self.spawn_instance();
                    self.phase = *self.phase_rx.borrow();
                    self.bootstrap(&mut st, Kind::Background).await;
                    if self.phase >= Phase::Publish {
                        self.publish(&mut st, false).await;
                    }
                    let now = Instant::now();
                    for slot in &mut self.slots {
                        if !slot.resolved {
                            slot.next_attempt =
                                now + Duration::from_millis(self.rng.gen_range(0..2000));
                        }
                    }
                }
            }
        }
    }

    fn final_offline(&mut self) -> NodeFinalMsg {
        self.final_sent = true;
        let (mark_at, mark_sent, mark_received) = self.measure_mark.unwrap_or((
            Instant::now(),
            self.base.msgs_sent,
            self.base.msgs_received,
        ));
        NodeFinalMsg {
            msgs_sent: self.base.msgs_sent,
            msgs_received: self.base.msgs_received,
            measure_msgs_sent: self.base.msgs_sent.saturating_sub(mark_sent),
            measure_msgs_received: self.base.msgs_received.saturating_sub(mark_received),
            measure_secs: mark_at.elapsed().as_secs_f64(),
            bytes_in: self.base.bytes_in,
            bytes_out: self.base.bytes_out,
            hops_hist: self.hops_hist.clone(),
            restarts: self.restarts,
            aborted_inflight: self.aborted_inflight,
            value_mismatches: self.value_mismatches,
            rt_size: 0,
            num_records: 0,
            record_bytes: 0,
            provider_keys: 0,
            providers: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregation helpers
// ---------------------------------------------------------------------------

struct Dist {
    count: u64,
    sum: f64,
    max: f64,
    vals: Vec<f64>,
    rng: StdRng,
}

impl Dist {
    fn new(seed: u64) -> Self {
        Self {
            count: 0,
            sum: 0.0,
            max: 0.0,
            vals: Vec::new(),
            rng: StdRng::seed_from_u64(seed),
        }
    }

    fn add(&mut self, v: f64) {
        self.count += 1;
        self.sum += v;
        if v > self.max {
            self.max = v;
        }
        if self.vals.len() < RESERVOIR_CAP {
            self.vals.push(v);
        } else {
            let i = self.rng.gen_range(0..self.count);
            if (i as usize) < RESERVOIR_CAP {
                self.vals[i as usize] = v;
            }
        }
    }

    fn json(&self) -> serde_json::Value {
        if self.count == 0 {
            return json!(null);
        }
        let mut sorted = self.vals.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN samples"));
        let pct = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize];
        json!({
            "count": self.count,
            "mean": self.sum / self.count as f64,
            "p50": pct(0.50),
            "p95": pct(0.95),
            "p99": pct(0.99),
            "max": self.max,
        })
    }
}

/// Median / p95 over a fixed-bucket histogram.
fn hist_pct(hist: &[u64; HOP_BUCKETS], q: f64) -> Option<f64> {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return None;
    }
    let rank = ((total - 1) as f64 * q) as u64;
    let mut seen = 0u64;
    for (bucket, &count) in hist.iter().enumerate() {
        seen += count;
        if seen > rank {
            return Some(bucket as f64);
        }
    }
    Some((HOP_BUCKETS - 1) as f64)
}

struct KindAgg {
    outcomes: [u64; OUTCOME_COUNT],
    lat_ok: Dist,
    lat_all: Dist,
}

impl KindAgg {
    fn new(seed: u64) -> Self {
        Self {
            outcomes: [0; OUTCOME_COUNT],
            lat_ok: Dist::new(seed),
            lat_all: Dist::new(seed.wrapping_add(1)),
        }
    }
}

fn rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

fn nofile_soft_limit() -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur
    } else {
        0
    }
}

/// Current netem configuration on `lo`, if any (the sweep script applies RTT injection
/// externally; this is recorded for the result row).
fn netem_on_lo() -> Option<String> {
    let output = std::process::Command::new("tc")
        .args(["qdisc", "show", "dev", "lo"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find(|line| line.contains("netem"))
        .map(|line| line.trim().to_string())
}

// ---------------------------------------------------------------------------
// Coordinator
// ---------------------------------------------------------------------------

struct SeriesBin {
    rss: u64,
    rt: Dist,
    records: Dist,
    record_bytes: Dist,
    provider_keys: Dist,
    providers: Dist,
}

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
        )
        .try_init();

    let args = Arc::new(parse_args());
    let nofile = nofile_soft_limit();
    if nofile < (args.nodes as u64) * 20 {
        eprintln!(
            "warning: RLIMIT_NOFILE soft limit is {nofile}; raise it (`ulimit -n 1048576`) \
             for {} nodes",
            args.nodes
        );
    }
    let netem = netem_on_lo();
    if args.latency_ms > 0 && netem.is_none() {
        eprintln!(
            "warning: --latency-ms {} declared but no netem qdisc is active on lo; \
             the declared latency is NOT being applied",
            args.latency_ms
        );
    }
    println!("config: {args:?}");
    if let Some(netem) = &netem {
        println!("netem on lo: {netem}");
    }

    let started = Instant::now();

    // Deterministic identities and workload plan.
    let mut keypairs = Vec::with_capacity(args.nodes);
    let mut peers = Vec::with_capacity(args.nodes);
    for _ in 0..args.nodes {
        let keypair = Keypair::generate();
        peers.push(PeerId::from_public_key(&keypair.public().into()));
        keypairs.push(keypair);
    }
    let plan = Arc::new(build_plan(&args, peers));
    let book = Arc::new(RwLock::new(vec![Vec::new(); args.nodes]));

    let (report_tx, mut report_rx) = mpsc::channel::<Report>(65536);
    let (phase_tx, phase_rx) = watch::channel(Phase::Init);
    let mut ctl_txs = Vec::with_capacity(args.nodes);

    for (idx, keypair) in keypairs.into_iter().enumerate() {
        let (ctl_tx, ctl_rx) = mpsc::channel(4);
        ctl_txs.push(ctl_tx);
        let mut rng = StdRng::seed_from_u64(args.seed.wrapping_add(idx as u64));
        let mut perm: Vec<u32> = (0..args.nodes as u32).collect();
        perm.shuffle(&mut rng);
        let slots = plan.samples[idx]
            .iter()
            .map(|&key_idx| Slot {
                key_idx,
                resolved: false,
                inflight: false,
                next_attempt: Instant::now(),
                attempts: 0,
            })
            .collect();
        let task = NodeTask {
            idx,
            keypair,
            args: args.clone(),
            t0: started,
            plan: plan.clone(),
            book: book.clone(),
            tx: report_tx.clone(),
            phase_rx: phase_rx.clone(),
            ctl_rx,
            rng,
            phase: Phase::Init,
            slots,
            bootstrap_reported: false,
            publish_reported: false,
            publish_ok: 0,
            publish_failed: 0,
            publish_left: 0,
            base: Counters::default(),
            measure_mark: None,
            measure_end: None,
            hops_hist: vec![[0u32; HOP_BUCKETS]; KIND_COUNT],
            issued: HashMap::new(),
            restarts: 0,
            aborted_inflight: 0,
            value_mismatches: 0,
            final_sent: false,
            perm,
            perm_pos: 0,
            resolved_local: HashSet::new(),
            inflight_a: 0,
            next_background: Instant::now(),
            next_republish: Instant::now(),
            next_stats: Instant::now(),
            last_snapshot: (0, 0, 0, 0, 0),
        };
        let delay = Duration::from_secs_f64(idx as f64 / args.bootstrap_rate.max(1.0));
        tokio::spawn(task.run(delay));
    }
    drop(report_tx);
    drop(phase_rx);

    // ------------------------------------------------------------------
    // Phase machine + aggregation.
    // ------------------------------------------------------------------
    let total_units = plan.total_units;
    let mut phase = Phase::Init;
    let mut phase_start = Instant::now();
    let mut phase_secs: HashMap<&'static str, f64> = HashMap::new();

    let mut ready = 0usize;
    let mut bootstrap_done = 0usize;
    let mut publish_done = 0usize;
    let mut publish_ok = 0u64;
    let mut publish_failed = 0u64;

    let mut unit_bits = vec![false; total_units as usize];
    let mut resolved_units = 0u32;
    let mut measure_start: Option<Instant> = None;
    let mut time_to_pct: Vec<Option<f64>> = vec![None; THRESHOLDS.len()];

    // Per-node avg routing-table-size snapshots at the configured `--checkpoints` times
    // (seconds into the measurement window).
    let mut checkpoint_rt: Vec<Dist> = args
        .checkpoints
        .iter()
        .enumerate()
        .map(|(i, _)| Dist::new(args.seed ^ (0xc8_00 + i as u64)))
        .collect();
    let mut checkpoint_resolved_local: Vec<Dist> = args
        .checkpoints
        .iter()
        .enumerate()
        .map(|(i, _)| Dist::new(args.seed ^ (0xcc_00 + i as u64)))
        .collect();
    // `resolved_units` snapshotted (network-wide) when each checkpoint fired.
    let mut checkpoint_resolved_units: Vec<Option<u32>> = vec![None; args.checkpoints.len()];
    let mut checkpoint_fired = vec![false; args.checkpoints.len()];
    let mut unit_resolve_ms = Dist::new(args.seed ^ 0xa11);
    let mut unit_attempts = Dist::new(args.seed ^ 0xa12);
    let mut resolved_per_key = vec![0u32; plan.samplers_per_key.len()];

    let mut kind_aggs: Vec<KindAgg> =
        (0..KIND_COUNT).map(|i| KindAgg::new(args.seed ^ (0xb00 + i as u64))).collect();
    let mut series: Vec<(u32, SeriesBin)> = Vec::new();
    let mut max_rss = 0u64;

    let mut finals = 0usize;
    let mut node_msgs_sent = Dist::new(args.seed ^ 0xc01);
    let mut node_msgs_received = Dist::new(args.seed ^ 0xc02);
    let mut steady_sent_rate = Dist::new(args.seed ^ 0xc03);
    let mut steady_received_rate = Dist::new(args.seed ^ 0xc04);
    let mut node_bytes_in = Dist::new(args.seed ^ 0xc05);
    let mut node_bytes_out = Dist::new(args.seed ^ 0xc06);
    let mut final_rt = Dist::new(args.seed ^ 0xc07);
    let mut final_records = Dist::new(args.seed ^ 0xc08);
    let mut final_record_bytes = Dist::new(args.seed ^ 0xc09);
    let mut final_provider_keys = Dist::new(args.seed ^ 0xc0a);
    let mut final_providers = Dist::new(args.seed ^ 0xc0b);
    let mut hops: Vec<[u64; HOP_BUCKETS]> = vec![[0; HOP_BUCKETS]; KIND_COUNT];
    let mut total_msgs_sent = 0u64;
    let mut total_msgs_received = 0u64;
    let mut restarts_total = 0u64;
    let mut aborted_inflight_total = 0u64;
    let mut value_mismatches_total = 0u64;

    let mut csv = args.csv.as_ref().map(|path| {
        let mut file = std::io::BufWriter::new(std::fs::File::create(path).expect("create csv"));
        writeln!(file, "kind,outcome,latency_ms").unwrap();
        file
    });

    let mut churn_rng = StdRng::seed_from_u64(args.seed ^ 0xc4042);
    let mut churn_acc = 0.0f64;
    let mut churn_down_until: HashMap<usize, Instant> = HashMap::new();

    let bootstrap_cap = Duration::from_secs_f64(args.nodes as f64 / args.bootstrap_rate.max(1.0))
        + Duration::from_secs(90);
    let publish_cap = Duration::from_secs(120);
    let drain_secs = args.query_timeout.min(15);

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_print = Instant::now();
    let mut done_deadline: Option<Instant> = None;

    macro_rules! advance {
        ($next:expr) => {{
            let elapsed = phase_start.elapsed().as_secs_f64();
            phase_secs.insert(phase_name(phase), elapsed);
            println!(
                "[{:>7.1}s] phase {:?} -> {:?} ({:.1}s in phase)",
                started.elapsed().as_secs_f64(),
                phase,
                $next,
                elapsed
            );
            phase = $next;
            phase_start = Instant::now();
            if phase == Phase::Measure {
                measure_start = Some(Instant::now());
            }
            let _ = phase_tx.send(phase);
        }};
    }

    fn phase_name(phase: Phase) -> &'static str {
        match phase {
            Phase::Init => "init",
            Phase::Bootstrap => "bootstrap",
            Phase::Warmup => "warmup",
            Phase::Publish => "publish",
            Phase::Converge => "converge",
            Phase::Measure => "measure",
            Phase::Drain => "drain",
            Phase::Done => "done",
        }
    }

    loop {
        tokio::select! {
            report = report_rx.recv() => {
                let Some(report) = report else { break };
                match report {
                    Report::Ready => {
                        ready += 1;
                        if ready == args.nodes && phase == Phase::Init {
                            advance!(Phase::Bootstrap);
                        }
                    }
                    Report::BootstrapDone => {
                        bootstrap_done += 1;
                        if bootstrap_done == args.nodes && phase == Phase::Bootstrap {
                            advance!(Phase::Warmup);
                        }
                    }
                    Report::PublishDone { ok, failed } => {
                        publish_done += 1;
                        publish_ok += ok as u64;
                        publish_failed += failed as u64;
                        if publish_done == args.nodes && phase == Phase::Publish {
                            advance!(Phase::Converge);
                        }
                    }
                    Report::Sample { kind, outcome, latency_us } => {
                        let agg = &mut kind_aggs[kind.idx()];
                        agg.outcomes[outcome.idx()] += 1;
                        let ms = latency_us as f64 / 1000.0;
                        agg.lat_all.add(ms);
                        if outcome == Outcome::Resolved {
                            agg.lat_ok.add(ms);
                        }
                        if let Some(csv) = &mut csv {
                            writeln!(
                                csv,
                                "{},{},{:.3}",
                                KIND_NAMES[kind.idx()],
                                OUTCOME_NAMES[outcome.idx()],
                                ms
                            )
                            .unwrap();
                        }
                    }
                    Report::UnitResolved { unit, key_idx, elapsed_ms, attempts } => {
                        let unit = unit as usize;
                        if unit < unit_bits.len() && !unit_bits[unit] {
                            unit_bits[unit] = true;
                            resolved_units += 1;
                            unit_resolve_ms.add(elapsed_ms as f64);
                            unit_attempts.add(attempts as f64);
                            if (key_idx as usize) < resolved_per_key.len() {
                                resolved_per_key[key_idx as usize] += 1;
                            }
                            if let Some(measure_start) = measure_start {
                                let elapsed = measure_start.elapsed().as_secs_f64();
                                let pct = resolved_units as f64 * 100.0 / total_units as f64;
                                for (i, threshold) in THRESHOLDS.iter().enumerate() {
                                    if time_to_pct[i].is_none() && pct >= *threshold {
                                        time_to_pct[i] = Some(elapsed);
                                        println!(
                                            "[{:>7.1}s] {threshold}% of units resolved in {elapsed:.1}s",
                                            started.elapsed().as_secs_f64(),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Report::Checkpoint { idx, rt_size, resolved_local } => {
                        if let Some(dist) = checkpoint_rt.get_mut(idx) {
                            dist.add(rt_size as f64);
                            checkpoint_resolved_local[idx].add(resolved_local as f64);
                        }
                    }
                    Report::NodeStats(stats) => {
                        let bin_width = args.stats_interval.max(1) as u32;
                        let bin = stats.elapsed_s / bin_width * bin_width;
                        let entry = match series.iter_mut().find(|(t, _)| *t == bin) {
                            Some((_, entry)) => entry,
                            None => {
                                series.push((bin, SeriesBin {
                                    rss: rss_bytes(),
                                    rt: Dist::new(args.seed ^ 0xd01),
                                    records: Dist::new(args.seed ^ 0xd02),
                                    record_bytes: Dist::new(args.seed ^ 0xd03),
                                    provider_keys: Dist::new(args.seed ^ 0xd04),
                                    providers: Dist::new(args.seed ^ 0xd05),
                                }));
                                &mut series.last_mut().expect("just pushed").1
                            }
                        };
                        entry.rt.add(stats.rt_size as f64);
                        entry.records.add(stats.num_records as f64);
                        entry.record_bytes.add(stats.record_bytes as f64);
                        entry.provider_keys.add(stats.provider_keys as f64);
                        entry.providers.add(stats.providers as f64);
                        let _ = (stats.msgs_sent, stats.msgs_received);
                    }
                    Report::NodeFinal(f) => {
                        finals += 1;
                        node_msgs_sent.add(f.msgs_sent as f64);
                        node_msgs_received.add(f.msgs_received as f64);
                        if f.measure_secs > 1.0 {
                            steady_sent_rate.add(f.measure_msgs_sent as f64 / f.measure_secs);
                            steady_received_rate
                                .add(f.measure_msgs_received as f64 / f.measure_secs);
                        }
                        node_bytes_in.add(f.bytes_in as f64);
                        node_bytes_out.add(f.bytes_out as f64);
                        final_rt.add(f.rt_size as f64);
                        final_records.add(f.num_records as f64);
                        final_record_bytes.add(f.record_bytes as f64);
                        final_provider_keys.add(f.provider_keys as f64);
                        final_providers.add(f.providers as f64);
                        for (kind, hist) in f.hops_hist.iter().enumerate() {
                            for (bucket, &count) in hist.iter().enumerate() {
                                hops[kind][bucket] += count as u64;
                            }
                        }
                        total_msgs_sent += f.msgs_sent;
                        total_msgs_received += f.msgs_received;
                        restarts_total += f.restarts as u64;
                        aborted_inflight_total += f.aborted_inflight as u64;
                        value_mismatches_total += f.value_mismatches as u64;
                    }
                }
            }
            _ = tick.tick() => {
                max_rss = max_rss.max(rss_bytes());

                // Churn scheduling: restart a random share of nodes per minute while the
                // network is live (warm-up through measurement).
                if args.churn_pct_per_min > 0.0
                    && matches!(phase, Phase::Warmup | Phase::Publish | Phase::Converge | Phase::Measure)
                {
                    churn_acc += args.nodes as f64 * args.churn_pct_per_min / 100.0 / 60.0;
                    let now = Instant::now();
                    churn_down_until.retain(|_, until| *until > now);
                    while churn_acc >= 1.0 {
                        churn_acc -= 1.0;
                        for _ in 0..10 {
                            let victim = churn_rng.gen_range(0..args.nodes);
                            if !churn_down_until.contains_key(&victim) {
                                churn_down_until.insert(
                                    victim,
                                    now + Duration::from_secs(args.churn_downtime),
                                );
                                let _ = ctl_txs[victim].try_send(Ctl::Restart);
                                break;
                            }
                        }
                    }
                }

                // Fire discovery checkpoints: at each configured offset into the
                // measurement window, ask every node for a fresh routing-table-size
                // snapshot and record the network-wide resolved count.
                if let Some(measure_start) = measure_start {
                    let into_measure = measure_start.elapsed().as_secs();
                    for (idx, &at) in args.checkpoints.iter().enumerate() {
                        if !checkpoint_fired[idx] && into_measure >= at {
                            checkpoint_fired[idx] = true;
                            checkpoint_resolved_units[idx] = Some(resolved_units);
                            for ctl in &ctl_txs {
                                let _ = ctl.try_send(Ctl::Snapshot { idx });
                            }
                            println!(
                                "[{:>7.1}s] checkpoint @{at}s into measurement: \
                                 requesting routing-table snapshot ({resolved_units}/{total_units} \
                                 units resolved network-wide)",
                                started.elapsed().as_secs_f64(),
                            );
                        }
                    }
                }

                if last_print.elapsed() >= Duration::from_secs(5) {
                    last_print = Instant::now();
                    println!(
                        "[{:>7.1}s] phase={phase:?} ready={ready}/{n} bootstrap={bootstrap_done}/{n} \
                         publish={publish_done}/{n} units={resolved_units}/{total_units} \
                         finals={finals}/{n} rss={:.1} GiB",
                        started.elapsed().as_secs_f64(),
                        max_rss as f64 / (1024.0 * 1024.0 * 1024.0),
                        n = args.nodes,
                    );
                }

                match phase {
                    Phase::Init if phase_start.elapsed() > Duration::from_secs(120) => {
                        println!("ready cap hit ({ready}/{} ready)", args.nodes);
                        advance!(Phase::Bootstrap);
                    }
                    Phase::Bootstrap if phase_start.elapsed() > bootstrap_cap => {
                        println!("bootstrap cap hit ({bootstrap_done}/{} done)", args.nodes);
                        advance!(Phase::Warmup);
                    }
                    Phase::Warmup if phase_start.elapsed() > Duration::from_secs(args.warmup) => {
                        if args.bench == Bench::A {
                            advance!(Phase::Measure);
                        } else {
                            advance!(Phase::Publish);
                        }
                    }
                    Phase::Publish
                        if publish_done == args.nodes || phase_start.elapsed() > publish_cap =>
                    {
                        if publish_done < args.nodes {
                            println!("publish cap hit ({publish_done}/{} done)", args.nodes);
                        }
                        advance!(Phase::Converge);
                    }
                    Phase::Converge
                        if phase_start.elapsed() > Duration::from_secs(args.converge) =>
                    {
                        advance!(Phase::Measure);
                    }
                    Phase::Measure
                        if (resolved_units == total_units
                            && checkpoint_fired.iter().all(|&fired| fired))
                            || phase_start.elapsed()
                                > Duration::from_secs(args.measure_timeout) =>
                    {
                        advance!(Phase::Drain);
                    }
                    Phase::Drain if phase_start.elapsed() > Duration::from_secs(drain_secs) => {
                        advance!(Phase::Done);
                        done_deadline = Some(Instant::now() + Duration::from_secs(30));
                    }
                    Phase::Done => {
                        if finals >= args.nodes
                            || done_deadline.is_some_and(|deadline| Instant::now() > deadline)
                        {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }

        if phase == Phase::Done && finals >= args.nodes {
            break;
        }
    }

    let measure_secs = phase_secs.get("measure").copied().unwrap_or(0.0);
    let max_pct = resolved_units as f64 * 100.0 / total_units as f64;

    // ------------------------------------------------------------------
    // Output.
    // ------------------------------------------------------------------
    if let Some(csv) = &mut csv {
        csv.flush().unwrap();
    }

    let per_key_rates: Vec<f64> = resolved_per_key
        .iter()
        .zip(&plan.samplers_per_key)
        .map(|(resolved, samplers)| {
            if *samplers == 0 {
                1.0
            } else {
                *resolved as f64 / *samplers as f64
            }
        })
        .collect();

    let queries_json: serde_json::Value = (2..KIND_COUNT)
        .map(|kind| {
            let agg = &kind_aggs[kind];
            let total: u64 = agg.outcomes.iter().sum();
            let unresolved = total - agg.outcomes[Outcome::Resolved.idx()];
            (
                KIND_NAMES[kind].to_string(),
                json!({
                    "attempts": total,
                    "outcomes": OUTCOME_NAMES
                        .iter()
                        .zip(agg.outcomes.iter())
                        .map(|(name, count)| (name.to_string(), json!(count)))
                        .collect::<serde_json::Map<_, _>>(),
                    "unresolved_rate": if total > 0 {
                        json!(unresolved as f64 / total as f64)
                    } else {
                        json!(null)
                    },
                    "latency_ms_resolved": agg.lat_ok.json(),
                    "latency_ms_all": agg.lat_all.json(),
                }),
            )
        })
        .filter(|(_, value)| value["attempts"].as_u64() != Some(0))
        .collect::<serde_json::Map<_, _>>()
        .into();

    let hops_json: serde_json::Value = KIND_NAMES
        .iter()
        .enumerate()
        .filter_map(|(kind, name)| {
            let total: u64 = hops[kind].iter().sum();
            (total > 0).then(|| {
                (
                    name.to_string(),
                    json!({
                        "count": total,
                        "median": hist_pct(&hops[kind], 0.50),
                        "p95": hist_pct(&hops[kind], 0.95),
                    }),
                )
            })
        })
        .collect::<serde_json::Map<_, _>>()
        .into();

    let series_json: Vec<serde_json::Value> = series
        .iter()
        .map(|(t, bin)| {
            json!({
                "t": t,
                "rss_bytes": bin.rss,
                "routing_table": bin.rt.json(),
                "records": bin.records.json(),
                "record_bytes": bin.record_bytes.json(),
                "provider_keys": bin.provider_keys.json(),
                "providers": bin.providers.json(),
            })
        })
        .collect();

    let mut sorted_rates = per_key_rates.clone();
    sorted_rates.sort_by(|a, b| a.partial_cmp(b).expect("no NaN rates"));
    let per_key_json = if per_key_rates.is_empty() {
        json!(null)
    } else {
        json!({
            "mean": per_key_rates.iter().sum::<f64>() / per_key_rates.len() as f64,
            "min": sorted_rates.first(),
            "p10": sorted_rates[(sorted_rates.len() - 1) / 10],
            "p50": sorted_rates[(sorted_rates.len() - 1) / 2],
            "rates": if args.bench == Bench::B { json!(per_key_rates) } else { json!(null) },
        })
    };

    // Per-node avg routing-table-size snapshots at the requested checkpoint times.
    let checkpoints_json: Vec<serde_json::Value> = args
        .checkpoints
        .iter()
        .enumerate()
        .map(|(idx, &at)| {
            let units = checkpoint_resolved_units[idx];
            json!({
                "t_s": at,
                "fired": checkpoint_fired[idx],
                "avg_routing_table_size": checkpoint_rt[idx].json(),
                "nodes_reporting": checkpoint_rt[idx].count,
                "resolved_targets_per_node": checkpoint_resolved_local[idx].json(),
                "resolved_units_network": units,
                "resolved_pct_network": units
                    .map(|u| u as f64 * 100.0 / total_units as f64),
            })
        })
        .collect();

    let row = json!({
        "schema": 1,
        "run_id": args.run_id,
        "ts_unix": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        "bench": args.bench.name(),
        "litep2p_version": env!("CARGO_PKG_VERSION"),
        "params": {
            "nodes": args.nodes,
            "k": args.k,
            "alpha": args.alpha,
            "concurrent_lookups_per_node": args.concurrent_lookups,
            "query_timeout_s": args.query_timeout,
            "latency_ms": args.latency_ms,
            "jitter_ms": args.jitter_ms,
            "churn_pct_per_min": args.churn_pct_per_min,
            "churn_downtime_s": args.churn_downtime,
            "ttl_s": args.ttl,
            "republish_interval_s": args.republish_interval,
            "transport": "tcp-loopback",
            "seed": args.seed,
            "keys": args.keys,
            "providers_per_key": args.providers_per_key,
            "sample_keys": args.sample_keys,
            "payload_bytes": args.payload_bytes,
            "quorum": args.quorum.name(),
            "bootstrap_links": args.bootstrap_links,
            "warmup_s": args.warmup,
            "converge_s": args.converge,
            "measure_timeout_s": args.measure_timeout,
            "background_interval_s": args.background_interval,
            "retry_interval_s": args.retry_interval,
        },
        "env": {
            "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
            "nofile": nofile,
            "max_rss_bytes": max_rss,
            "netem_lo": netem,
            // 1-minute load average at the end of the run; values well above `cores`
            // mean the host was CPU-saturated and latencies measure the host, not the
            // protocol — rerun with fewer nodes or less offered load.
            "load_avg_1m": std::fs::read_to_string("/proc/loadavg")
                .ok()
                .and_then(|s| s.split_whitespace().next().map(str::to_string))
                .and_then(|s| s.parse::<f64>().ok()),
        },
        "phases": {
            "secs": phase_secs,
            "bootstrap_done": bootstrap_done,
            "publish_done": publish_done,
            "finals": finals,
        },
        "discovery": {
            "total_units": total_units,
            "resolved_units": resolved_units,
            "max_pct_reached": max_pct,
            "time_to_pct_s": THRESHOLDS
                .iter()
                .zip(time_to_pct.iter())
                .map(|(threshold, time)| (format!("{threshold}"), json!(time)))
                .collect::<serde_json::Map<_, _>>(),
            "dnf": resolved_units < total_units,
            "unit_resolve_ms": unit_resolve_ms.json(),
            "unit_attempts": unit_attempts.json(),
            "checkpoints": checkpoints_json,
        },
        "queries": queries_json,
        "hops": hops_json,
        "messages": {
            "per_node_sent": node_msgs_sent.json(),
            "per_node_received": node_msgs_received.json(),
            "steady_sent_per_s": steady_sent_rate.json(),
            "steady_received_per_s": steady_received_rate.json(),
            "per_node_bytes_in": node_bytes_in.json(),
            "per_node_bytes_out": node_bytes_out.json(),
            "total_sent": total_msgs_sent,
            "total_received": total_msgs_received,
        },
        "node_state_final": {
            "routing_table": final_rt.json(),
            "records": final_records.json(),
            "record_bytes": final_record_bytes.json(),
            "provider_keys": final_provider_keys.json(),
            "providers": final_providers.json(),
        },
        "series": series_json,
        "per_key_success": per_key_json,
        "puts": {
            "ok": publish_ok,
            "failed": publish_failed,
            "success_rate": if publish_ok + publish_failed > 0 {
                json!(publish_ok as f64 / (publish_ok + publish_failed) as f64)
            } else {
                json!(null)
            },
        },
        "value_mismatches": value_mismatches_total,
        "churn": {
            "restarts": restarts_total,
            "aborted_inflight": aborted_inflight_total,
        },
        "measure_secs": measure_secs,
    });

    if let Some(path) = &args.out {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open --out file");
        writeln!(file, "{row}").unwrap();
        println!("result row appended to {path}");
    }

    // Human-readable summary.
    println!(
        "\n=== kad_bench summary: bench={} nodes={} seed={} ===",
        args.bench.name(),
        args.nodes,
        args.seed
    );
    println!(
        "discovery: {resolved_units}/{total_units} units ({max_pct:.1}%){}",
        if resolved_units < total_units {
            "  [DNF]"
        } else {
            ""
        }
    );
    for (threshold, time) in THRESHOLDS.iter().zip(time_to_pct.iter()) {
        match time {
            Some(time) => println!("  time to {threshold:>5}%: {time:>8.1}s"),
            None => println!("  time to {threshold:>5}%:      DNF"),
        }
    }
    println!("unit resolve ms: {}", unit_resolve_ms.json());
    for (idx, &at) in args.checkpoints.iter().enumerate() {
        let rt = &checkpoint_rt[idx];
        let units = checkpoint_resolved_units[idx];
        let mean = if rt.count > 0 {
            rt.sum / rt.count as f64
        } else {
            0.0
        };
        println!(
            "checkpoint @{at:>4}s: avg routing-table size = {mean:.1} peers/node \
             (over {} nodes); network resolved {}/{total_units} ({:.1}%)",
            rt.count,
            units.map(|u| u.to_string()).unwrap_or_else(|| "-".into()),
            units.map(|u| u as f64 * 100.0 / total_units as f64).unwrap_or(0.0),
        );
    }
    for kind in 2..KIND_COUNT {
        let agg = &kind_aggs[kind];
        let total: u64 = agg.outcomes.iter().sum();
        if total == 0 {
            continue;
        }
        println!(
            "{:<14} attempts={total} resolved={} partial={} not_found={} failed={} timeout={} lat(ok)={}",
            KIND_NAMES[kind],
            agg.outcomes[0],
            agg.outcomes[1],
            agg.outcomes[2],
            agg.outcomes[3],
            agg.outcomes[4],
            agg.lat_ok.json(),
        );
    }
    println!(
        "messages/node: sent={} received={} steady sent/s={}",
        node_msgs_sent.json(),
        node_msgs_received.json(),
        steady_sent_rate.json(),
    );
    println!(
        "node state: rt={} records={} record_bytes={} providers={}",
        final_rt.json(),
        final_records.json(),
        final_record_bytes.json(),
        final_providers.json(),
    );
    if args.churn_pct_per_min > 0.0 {
        println!("churn: restarts={restarts_total} aborted_inflight={aborted_inflight_total}");
    }
}
