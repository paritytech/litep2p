#!/usr/bin/env bash
# Scaling sweep for examples/kad_bench.rs: runs benchmarks A (FIND_NODE convergence),
# B (provider discovery) and C (record storage) over a grid of network sizes and
# parameters, with >= 3 seeds per combination, appending one JSON row per run to
# $OUT/results.jsonl. Analyze with scripts/kad-bench-analyze.py.
#
# Usage (all knobs are environment variables):
#   BENCHES="a b c" NODES="200 500 1000 2000 5000" SEEDS="1 2 3" \
#       ./scripts/kad-bench-sweep.sh
#
# Knobs:
#   BENCHES      benchmarks to run                       (default "a b c")
#   NODES        network sizes                           (default "200 500 1000 2000 5000")
#   SEEDS        seeds, >= 3 recommended                 (default "1 2 3")
#   K            replication factor                      (default 20)
#   ALPHA        lookup parallelism                      (default 3)
#   CONCURRENT   concurrent lookups per node             (default 8)
#   QUORUMS      bench-c quorums                         (default "one majority")
#   PAYLOADS     bench-c payload bytes                   (default "64 1024")
#   KEYS         bench-b key count                       (default 16)
#   PROVIDERS    bench-b providers per key               (default 3)
#   SAMPLE_KEYS  keys sampled per node (b/c)             (default 8)
#   CHURN        churn %/min                             (default 0)
#   LATENCY_MS   one-way netem delay on lo, needs sudo   (default 0 = none)
#   JITTER_MS    netem jitter                            (default 0)
#   CHECKPOINTS  snapshot avg routing-table size at these measurement-window offsets,
#                e.g. "120,300" for 2 and 5 minutes                  (default none)
#   WARMUP / CONVERGE / MEASURE_TIMEOUT / QUERY_TIMEOUT  phase windows in seconds
#   EXTRA_ARGS   extra flags passed through to kad_bench
#   OUT          output directory                        (default kad-bench-sweep-<ts>)
#
# The sweep is resumable: combinations whose run_id already appears in
# $OUT/results.jsonl are skipped, so a crashed sweep can be re-run with OUT set.
#
# Latency injection: when LATENCY_MS > 0 a netem qdisc is installed on the loopback
# device (`tc qdisc replace dev lo root netem delay <LATENCY_MS>ms <JITTER_MS>ms`),
# giving every connection an RTT of ~2*LATENCY_MS. This requires root and affects ALL
# loopback traffic on the machine while the sweep runs; the qdisc is removed on exit.
set -euo pipefail

BENCHES="${BENCHES:-a b c}"
NODES="${NODES:-200 500 1000 2000 5000}"
SEEDS="${SEEDS:-1 2 3}"
K="${K:-20}"
ALPHA="${ALPHA:-3}"
CONCURRENT="${CONCURRENT:-8}"
QUORUMS="${QUORUMS:-one majority}"
PAYLOADS="${PAYLOADS:-64 1024}"
KEYS="${KEYS:-16}"
PROVIDERS="${PROVIDERS:-3}"
SAMPLE_KEYS="${SAMPLE_KEYS:-8}"
CHURN="${CHURN:-0}"
LATENCY_MS="${LATENCY_MS:-0}"
JITTER_MS="${JITTER_MS:-0}"
CHECKPOINTS="${CHECKPOINTS:-}"   # e.g. "120,300" → snapshot avg routing-table size at 2/5 min
WARMUP="${WARMUP:-60}"
CONVERGE="${CONVERGE:-120}"
MEASURE_TIMEOUT="${MEASURE_TIMEOUT:-300}"
QUERY_TIMEOUT="${QUERY_TIMEOUT:-60}"
EXTRA_ARGS="${EXTRA_ARGS:-}"
OUT="${OUT:-kad-bench-sweep-$(date +%Y%m%d-%H%M%S)}"

cd "$(dirname "$0")/.."
mkdir -p "$OUT"
RESULTS="$OUT/results.jsonl"
touch "$RESULTS"

max_nodes=0
for n in $NODES; do [ "$n" -gt "$max_nodes" ] && max_nodes=$n; done

ulimit -n "$(ulimit -Hn)" 2>/dev/null || true
if [ "$(ulimit -n)" -lt "$((max_nodes * 20))" ]; then
    echo "warning: ulimit -n is $(ulimit -n), want >= $((max_nodes * 20)) for $max_nodes nodes" >&2
fi

# Optional RTT injection via tc-netem on loopback.
netem_active=0
cleanup_netem() {
    if [ "$netem_active" = 1 ]; then
        echo "removing netem qdisc from lo"
        sudo tc qdisc del dev lo root 2>/dev/null || true
    fi
}
trap cleanup_netem EXIT
if [ "$LATENCY_MS" -gt 0 ]; then
    if sudo -n true 2>/dev/null; then
        echo "installing netem on lo: delay ${LATENCY_MS}ms ${JITTER_MS}ms (RTT ~$((LATENCY_MS * 2))ms)"
        sudo tc qdisc replace dev lo root netem delay "${LATENCY_MS}ms" "${JITTER_MS}ms"
        netem_active=1
    else
        echo "error: LATENCY_MS=$LATENCY_MS requires passwordless sudo for tc-netem" >&2
        exit 1
    fi
fi

cargo build --release --example kad_bench
BIN=./target/release/examples/kad_bench

run_one() { # bench nodes seed quorum payload
    local bench=$1 nodes=$2 seed=$3 quorum=$4 payload=$5
    local run_id="bench=$bench,n=$nodes,seed=$seed,k=$K,alpha=$ALPHA,cl=$CONCURRENT"
    run_id="$run_id,churn=$CHURN,lat=$LATENCY_MS,quorum=$quorum,payload=$payload,keys=$KEYS"

    if grep -Fq "\"run_id\":\"$run_id\"" "$RESULTS"; then
        echo "--- skip (already done): $run_id"
        return 0
    fi
    echo "=== run: $run_id ==="
    local log="$OUT/$(echo "$run_id" | tr ',=' '__').log"

    local checkpoint_args=()
    [ -n "$CHECKPOINTS" ] && checkpoint_args=(--checkpoints "$CHECKPOINTS")

    # shellcheck disable=SC2086
    "$BIN" \
        --bench "$bench" --nodes "$nodes" --seed "$seed" \
        --k "$K" --alpha "$ALPHA" --concurrent-lookups "$CONCURRENT" \
        --churn-pct-per-min "$CHURN" \
        --latency-ms "$LATENCY_MS" --jitter-ms "$JITTER_MS" \
        --keys "$KEYS" --providers-per-key "$PROVIDERS" --sample-keys "$SAMPLE_KEYS" \
        --quorum "$quorum" --payload-bytes "$payload" \
        --warmup "$WARMUP" --converge "$CONVERGE" \
        --measure-timeout "$MEASURE_TIMEOUT" --query-timeout "$QUERY_TIMEOUT" \
        "${checkpoint_args[@]}" \
        --run-id "$run_id" --out "$RESULTS" \
        $EXTRA_ARGS \
        2>&1 | tee "$log"
}

for bench in $BENCHES; do
    for nodes in $NODES; do
        if [ "$bench" = c ]; then
            for quorum in $QUORUMS; do
                for payload in $PAYLOADS; do
                    for seed in $SEEDS; do
                        run_one "$bench" "$nodes" "$seed" "$quorum" "$payload"
                    done
                done
            done
        else
            for seed in $SEEDS; do
                run_one "$bench" "$nodes" "$seed" one 64
            done
        fi
    done
done

echo
echo "sweep complete: $(wc -l < "$RESULTS") rows in $RESULTS"
echo "analyze with: python3 scripts/kad-bench-analyze.py $RESULTS --plots $OUT/plots"
