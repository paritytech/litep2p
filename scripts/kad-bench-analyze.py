#!/usr/bin/env python3
"""Aggregate and plot kad_bench results.

Reads the results.jsonl produced by scripts/kad-bench-sweep.sh (one JSON row per
(benchmark, N, params, seed) run), aggregates across seeds as mean +/- 95% CI, and emits:

  * <out>/summary.csv          one aggregated row per (bench, N, params) group
  * stdout                     per-benchmark scaling tables and a small-DHT vs
                               ecosystem-DHT contrast table (log-log extrapolated for
                               network sizes that were not run, marked "extrap.")
  * <out>/*.png                scaling plots overlaying benchmarks A/B/C
                               (only if matplotlib is installed)

Usage:
    python3 scripts/kad-bench-analyze.py results.jsonl [--plots OUTDIR]
"""

import argparse
import csv
import json
import math
import os
import sys
from collections import defaultdict

# Two-sided 95% t-values by degrees of freedom (falls back to 1.96).
T95 = {1: 12.706, 2: 4.303, 3: 3.182, 4: 2.776, 5: 2.571, 6: 2.447, 7: 2.365, 8: 2.306, 9: 2.262}

THRESHOLDS = ["50", "80", "90", "95", "99", "100"]
SMALL_BAND = (500, 2000)
ECO_BAND = (10_000, 50_000)
ECO_POINTS = [10_000, 20_000, 50_000]


def mean_ci(values):
    values = [v for v in values if v is not None]
    if not values:
        return (None, None, 0)
    n = len(values)
    mean = sum(values) / n
    if n < 2:
        return (mean, None, n)
    var = sum((v - mean) ** 2 for v in values) / (n - 1)
    t = T95.get(n - 1, 1.96)
    return (mean, t * math.sqrt(var / n), n)


def get(row, *path):
    cur = row
    for key in path:
        if cur is None:
            return None
        cur = cur.get(key) if isinstance(cur, dict) else None
    return cur


def group_key(row):
    p = row["params"]
    variant = ""
    if row["bench"] == "c":
        variant = f"/q={p['quorum']}/{p['payload_bytes']}B"
    return (
        row["bench"] + variant,
        p["nodes"],
        p["k"],
        p["alpha"],
        p["concurrent_lookups_per_node"],
        p["churn_pct_per_min"],
        p["latency_ms"],
    )


# Metric extractors: name -> (callable(row) -> float|None).
def hops_kind(row):
    return {"a": "find_node", "b": "get_providers", "c": "get_record"}[row["bench"]]


METRICS = {
    "max_pct": lambda r: get(r, "discovery", "max_pct_reached"),
    "dnf": lambda r: 1.0 if get(r, "discovery", "dnf") else 0.0,
    **{
        f"ttp{t}_s": (lambda t: lambda r: get(r, "discovery", "time_to_pct_s", t))(t)
        for t in THRESHOLDS
    },
    "unit_resolve_p50_ms": lambda r: get(r, "discovery", "unit_resolve_ms", "p50"),
    "unit_resolve_p95_ms": lambda r: get(r, "discovery", "unit_resolve_ms", "p95"),
    "lookup_p50_ms": lambda r: _lookup_lat(r, "p50"),
    "lookup_p95_ms": lambda r: _lookup_lat(r, "p95"),
    "lookup_p99_ms": lambda r: _lookup_lat(r, "p99"),
    "lookup_max_ms": lambda r: _lookup_lat(r, "max"),
    "unresolved_rate": lambda r: _kind_field(r, "unresolved_rate"),
    "hops_median": lambda r: get(r, "hops", hops_kind(r), "median"),
    "hops_p95": lambda r: get(r, "hops", hops_kind(r), "p95"),
    "msgs_sent_per_node": lambda r: get(r, "messages", "per_node_sent", "mean"),
    "steady_msgs_per_s": lambda r: get(r, "messages", "steady_sent_per_s", "mean"),
    "bytes_out_per_node": lambda r: get(r, "messages", "per_node_bytes_out", "mean"),
    "routing_table_mean": lambda r: get(r, "node_state_final", "routing_table", "mean"),
    "records_per_node": lambda r: get(r, "node_state_final", "records", "mean"),
    "stored_bytes_per_node": lambda r: get(r, "node_state_final", "record_bytes", "mean"),
    "providers_per_node": lambda r: get(r, "node_state_final", "providers", "mean"),
    "per_key_success_mean": lambda r: get(r, "per_key_success", "mean"),
    "per_key_success_min": lambda r: get(r, "per_key_success", "min"),
    "put_success_rate": lambda r: get(r, "puts", "success_rate"),
    "max_rss_gib": lambda r: (get(r, "env", "max_rss_bytes") or 0) / 2**30 or None,
    # Host CPU saturation indicator; latencies from runs with load/core >> 1 measure the
    # host, not the protocol.
    "load_per_core": lambda r: (
        get(r, "env", "load_avg_1m") / get(r, "env", "cores")
        if get(r, "env", "load_avg_1m") and get(r, "env", "cores")
        else None
    ),
}


def _measure_kind(row):
    return {"a": "find_node", "b": "get_providers", "c": "get_record"}[row["bench"]]


def _lookup_lat(row, field):
    return get(row, "queries", _measure_kind(row), "latency_ms_resolved", field)


def _kind_field(row, field):
    return get(row, "queries", _measure_kind(row), field)


def checkpoint(row, t, *path):
    """Field from the discovery checkpoint taken at `t` seconds, or None."""
    for c in get(row, "discovery", "checkpoints") or []:
        if c.get("t_s") == t:
            return get(c, *path)
    return None


def checkpoint_times(rows):
    """Sorted union of all checkpoint times present in the rows."""
    times = set()
    for row in rows:
        for c in get(row, "discovery", "checkpoints") or []:
            if c.get("t_s") is not None:
                times.add(c["t_s"])
    return sorted(times)


def fmt(value, ci=None, unit="", missing="n/a"):
    if value is None:
        return missing
    if abs(value) >= 1000:
        text = f"{value:,.0f}"
    elif abs(value) >= 10:
        text = f"{value:.1f}"
    else:
        text = f"{value:.3g}"
    if ci is not None:
        text += f" ±{ci:.3g}"
    return text + unit


def loglog_fit(points):
    """Least-squares fit log(y) = a*log(x) + b over (x, y) pairs; returns predict(x)."""
    pts = [(x, y) for x, y in points if x and y and y > 0]
    if len(pts) < 2:
        return None
    lx = [math.log(x) for x, _ in pts]
    ly = [math.log(y) for _, y in pts]
    n = len(pts)
    mx, my = sum(lx) / n, sum(ly) / n
    denom = sum((x - mx) ** 2 for x in lx)
    if denom == 0:
        return None
    a = sum((x - mx) * (y - my) for x, y in zip(lx, ly)) / denom
    b = my - a * mx
    return lambda x: math.exp(a * math.log(x) + b), a


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", help="results.jsonl from kad-bench-sweep.sh")
    parser.add_argument("--plots", default=None, help="directory for plots + summary.csv")
    args = parser.parse_args()

    rows = []
    with open(args.results) as f:
        for line in f:
            line = line.strip()
            if line:
                rows.append(json.loads(line))
    if not rows:
        sys.exit("no rows in input")

    out_dir = args.plots or os.path.dirname(os.path.abspath(args.results))
    os.makedirs(out_dir, exist_ok=True)

    # Per-checkpoint discovery metrics (avg routing-table size = nodes discovered per node,
    # and network-wide resolved %), one pair per checkpoint time present in the data.
    ckpt_times = checkpoint_times(rows)
    for t in ckpt_times:
        METRICS[f"disc@{t}s_rt"] = (
            lambda t: lambda r: checkpoint(r, t, "avg_routing_table_size", "mean")
        )(t)
        METRICS[f"disc@{t}s_resolved_pct"] = (
            lambda t: lambda r: checkpoint(r, t, "resolved_pct_network")
        )(t)

    # ------------------------------------------------------------------
    # Aggregate across seeds.
    # ------------------------------------------------------------------
    groups = defaultdict(list)
    for row in rows:
        groups[group_key(row)].append(row)

    agg = {}  # key -> {metric: (mean, ci, n)}
    for key, members in sorted(groups.items()):
        agg[key] = {name: mean_ci([fn(r) for r in members]) for name, fn in METRICS.items()}
        agg[key]["seeds"] = (len(members), None, len(members))

    # summary.csv
    csv_path = os.path.join(out_dir, "summary.csv")
    fields = ["bench", "nodes", "k", "alpha", "concurrent", "churn", "latency_ms", "seeds"]
    for name in METRICS:
        fields += [name, f"{name}_ci95"]
    with open(csv_path, "w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(fields)
        for key, metrics in agg.items():
            row = list(key) + [metrics["seeds"][0]]
            for name in METRICS:
                mean, ci, _ = metrics[name]
                row += [mean, ci]
            writer.writerow(row)
    print(f"wrote {csv_path} ({len(agg)} aggregated groups)")

    # ------------------------------------------------------------------
    # Per-benchmark scaling tables.
    # ------------------------------------------------------------------
    benches = sorted({key[0] for key in agg})
    # A "section" is everything but N: one scaling table per parameter combination.
    sections = sorted({(key[0],) + key[2:] for key in agg})
    table_metrics = [
        ("ttp90_s", "t→90% (s)"),
        ("ttp100_s", "t→100% (s)"),
        ("max_pct", "max %"),
        ("lookup_p50_ms", "lookup p50 (ms)"),
        ("lookup_p95_ms", "lookup p95 (ms)"),
        ("hops_median", "hops p50"),
        ("steady_msgs_per_s", "msgs/s/node"),
        ("stored_bytes_per_node", "stored B/node"),
        ("unresolved_rate", "unresolved"),
        ("load_per_core", "load/core"),
    ]
    for section in sections:
        bench, k, alpha, concurrent, churn, latency = section
        keys = sorted(
            (key for key in agg if (key[0],) + key[2:] == section), key=lambda key: key[1]
        )
        print(f"\n## bench {bench}  (k={k} alpha={alpha} "
              f"concurrent={concurrent} churn={churn} latency={latency}ms)\n")
        header = ["N"] + [label for _, label in table_metrics] + ["seeds", "DNFs"]
        print("| " + " | ".join(header) + " |")
        print("|" + "|".join("---" for _ in header) + "|")
        for key in keys:
            metrics = agg[key]
            cells = [str(key[1])]
            for name, _ in table_metrics:
                mean, ci, _n = metrics[name]
                cells.append(fmt(mean, ci, missing="DNF" if name.startswith("ttp") else "n/a"))
            n_seeds = metrics["seeds"][0]
            dnfs = int(round((metrics["dnf"][0] or 0) * n_seeds))
            cells += [str(n_seeds), str(dnfs)]
            print("| " + " | ".join(cells) + " |")

    # ------------------------------------------------------------------
    # Discovery-over-time checkpoints (avg peers known per node at each checkpoint).
    # ------------------------------------------------------------------
    if ckpt_times:
        print("\n## discovery at checkpoints — avg routing-table size "
              "(distinct peers known per node, mean ± 95% CI)\n")
        cols = []
        for t in ckpt_times:
            cols += [f"peers@{t}s", f"resolved%@{t}s"]
        header = ["bench", "N"] + cols + ["seeds"]
        print("| " + " | ".join(header) + " |")
        print("|" + "|".join("---" for _ in header) + "|")
        for section in sections:
            keys = sorted(
                (key for key in agg if (key[0],) + key[2:] == section), key=lambda key: key[1]
            )
            for key in keys:
                metrics = agg[key]
                if all(metrics[f"disc@{t}s_rt"][0] is None for t in ckpt_times):
                    continue
                cells = [section[0], str(key[1])]
                for t in ckpt_times:
                    rt_mean, rt_ci, _ = metrics[f"disc@{t}s_rt"]
                    pct_mean, _pct_ci, _ = metrics[f"disc@{t}s_resolved_pct"]
                    cells.append(fmt(rt_mean, rt_ci))
                    cells.append(fmt(pct_mean, unit="%") if pct_mean is not None else "n/a")
                cells.append(str(metrics["seeds"][0]))
                print("| " + " | ".join(cells) + " |")

    # ------------------------------------------------------------------
    # Small-DHT vs ecosystem-DHT contrast.
    # ------------------------------------------------------------------
    contrast_metrics = [
        ("ttp90_s", "time to 90% (s)"),
        ("max_pct", "max % reached"),
        ("lookup_p95_ms", "lookup p95 (ms)"),
        ("hops_median", "hops (median)"),
        ("steady_msgs_per_s", "msgs/s per node"),
        ("stored_bytes_per_node", "stored bytes/node"),
    ]
    print("\n## small-DHT (500–2000 nodes) vs ecosystem-DHT (10k–50k nodes)\n")
    print("Values are means over measured sizes in each band; ecosystem values are "
          "log-log extrapolations from the measured N range when not run (marked *).\n")
    header = ["bench", "metric", f"small {SMALL_BAND[0]}–{SMALL_BAND[1]}",
              f"ecosystem {ECO_BAND[0]}–{ECO_BAND[1]}", "ratio"]
    print("| " + " | ".join(header) + " |")
    print("|" + "|".join("---" for _ in header) + "|")
    for bench in benches:
        keys = sorted((k for k in agg if k[0] == bench), key=lambda k: k[1])
        for name, label in contrast_metrics:
            points = [(k[1], agg[k][name][0]) for k in keys if agg[k][name][0] is not None]
            small = [y for x, y in points if SMALL_BAND[0] <= x <= SMALL_BAND[1]]
            eco = [y for x, y in points if ECO_BAND[0] <= x <= ECO_BAND[1]]
            small_v = sum(small) / len(small) if small else None
            extrapolated = False
            if eco:
                eco_v = sum(eco) / len(eco)
            else:
                fit = loglog_fit(points)
                if fit:
                    predict, _slope = fit
                    eco_v = sum(predict(n) for n in ECO_POINTS) / len(ECO_POINTS)
                    extrapolated = True
                else:
                    eco_v = None
            if name == "max_pct" and eco_v is not None:
                eco_v = min(eco_v, 100.0)
            ratio = (eco_v / small_v) if (small_v and eco_v is not None and small_v != 0) else None
            print("| " + " | ".join([
                bench,
                label,
                fmt(small_v),
                (fmt(eco_v) + ("*" if extrapolated else "")) if eco_v is not None else "n/a",
                f"{ratio:.2f}x" if ratio is not None else "n/a",
            ]) + " |")

    # ------------------------------------------------------------------
    # Plots.
    # ------------------------------------------------------------------
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:
        print("\nmatplotlib not installed -> skipping plots "
              "(pip install matplotlib, then re-run)")
        return

    def plot(metric, ylabel, fname, logy=True, extra_metrics=None):
        fig, ax = plt.subplots(figsize=(7, 4.5))
        for bench in benches:
            keys = sorted((k for k in agg if k[0] == bench), key=lambda k: k[1])
            for name, style in [(metric, "-")] + (extra_metrics or []):
                xs, ys, errs = [], [], []
                for key in keys:
                    mean, ci, _ = agg[key][name]
                    if mean is not None:
                        xs.append(key[1])
                        ys.append(mean)
                        errs.append(ci or 0)
                if xs:
                    label = bench if name == metric else f"{bench} ({name})"
                    ax.errorbar(xs, ys, yerr=errs, marker="o", capsize=3,
                                linestyle=style, label=label)
        ax.set_xscale("log")
        if logy:
            ax.set_yscale("log")
        ax.set_xlabel("network size N")
        ax.set_ylabel(ylabel)
        ax.grid(True, which="both", alpha=0.3)
        ax.legend()
        fig.tight_layout()
        path = os.path.join(out_dir, fname)
        fig.savefig(path, dpi=140)
        plt.close(fig)
        print(f"wrote {path}")

    plot("ttp90_s", "time to 90% resolved (s)", "time-to-90pct-vs-n.png",
         extra_metrics=[("ttp100_s", "--")])
    plot("max_pct", "max % units resolved", "max-pct-vs-n.png", logy=False)
    plot("steady_msgs_per_s", "steady-state messages/s per node", "msgs-per-node-vs-n.png")
    plot("stored_bytes_per_node", "stored record bytes per node", "stored-bytes-vs-n.png")
    plot("lookup_p95_ms", "lookup latency p95 (ms)", "lookup-p95-vs-n.png")
    plot("hops_median", "peers queried per lookup (median)", "hops-vs-n.png", logy=False)


if __name__ == "__main__":
    main()
