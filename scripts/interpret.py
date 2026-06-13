#!/usr/bin/env python3
"""Plot a single kad_bench run: how peer discovery progresses over time.

Reads one row produced by `cargo run --example kad_bench -- ... --out results.json`
and draws two stacked panels sharing the measurement-time x-axis:

  * top    -- avg peers per node in the routing table over time (mean line plus a
              p50..p95 spread band and the max), i.e. "how many peers each node has
              discovered so far".
  * bottom -- network-wide discovery completion (% of lookup units resolved), built
              from the time-to-X% milestones and the routing-table checkpoints, with
              vertical marks at the 50/80/90/95/99/100% milestones.

`results.json` may be a single JSON object or a JSON-lines file with one run per
line (as appended by repeated bench runs); use --bench/--seed/--index to pick a row.

Usage:
    python3 scripts/interpret.py results.json
    python3 scripts/interpret.py results.json --bench a --seed 1 --out discovery.png
"""

import argparse
import json
import sys

THRESHOLDS = ["50", "80", "90", "95", "99", "100"]


def load_rows(path):
    """Load results.json whether it is a single object or one-object-per-line."""
    text = open(path).read().strip()
    if not text:
        sys.exit(f"{path}: empty")
    # Try a single JSON value first (object or array), then fall back to JSONL.
    try:
        val = json.loads(text)
        return val if isinstance(val, list) else [val]
    except json.JSONDecodeError:
        rows = []
        for ln in text.splitlines():
            ln = ln.strip()
            if ln:
                rows.append(json.loads(ln))
        return rows


def pick_row(rows, bench, seed, index):
    sel = rows
    if bench is not None:
        sel = [r for r in sel if str(r.get("bench")) == str(bench)]
    if seed is not None:
        sel = [r for r in sel if r.get("params", {}).get("seed") == seed]
    if not sel:
        sys.exit("no run matched the given --bench/--seed filters")
    return sel[index]


def build_discovery_curve(disc):
    """Return monotonically-increasing (time_s, pct) anchors for the completion curve."""
    ttp = disc.get("time_to_pct_s", {})
    pts = [(0.0, 0.0)]
    for thr in THRESHOLDS:
        t = ttp.get(thr)
        if t is not None:
            pts.append((float(t), float(thr)))
    # Checkpoints record the network-wide resolved pct at a known time.
    for cp in disc.get("checkpoints", []):
        if cp.get("fired") and cp.get("t_s") is not None:
            pts.append((float(cp["t_s"]), float(cp["resolved_pct_network"])))
    # Sort by time and keep the curve non-decreasing in pct.
    pts.sort()
    out, best = [], -1.0
    for t, p in pts:
        p = max(p, best)
        best = p
        out.append((t, p))
    return out


def main():
    ap = argparse.ArgumentParser(description="Plot peer discovery over time for one kad_bench run.")
    ap.add_argument("results", help="results.json (single object or JSON-lines)")
    ap.add_argument("--bench", help="select run by bench id (a/b/c)")
    ap.add_argument("--seed", type=int, help="select run by seed")
    ap.add_argument("--index", type=int, default=-1, help="which matching row (default: last)")
    ap.add_argument("--out", help="output image path (default: derived from results filename)")
    ap.add_argument("--show", action="store_true", help="open an interactive window instead of saving")
    args = ap.parse_args()

    rows = load_rows(args.results)
    row = pick_row(rows, args.bench, args.seed, args.index)

    disc = row["discovery"]
    series = row.get("series", [])
    params = row.get("params", {})
    nodes = params.get("nodes", "?")
    bench = row.get("bench", "?")
    seed = params.get("seed", "?")

    try:
        import matplotlib
        if not args.show:
            matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:
        sys.exit("matplotlib is required: pip install matplotlib")

    # ---- routing-table growth (peers per node) -------------------------------
    t_rt = [s["t"] for s in series]
    rt_mean = [s["routing_table"]["mean"] for s in series]
    rt_p50 = [s["routing_table"]["p50"] for s in series]
    rt_p95 = [s["routing_table"]["p95"] for s in series]
    rt_max = [s["routing_table"]["max"] for s in series]

    # ---- discovery completion curve ------------------------------------------
    curve = build_discovery_curve(disc)
    c_t = [t for t, _ in curve]
    c_p = [p for _, p in curve]
    ttp = disc.get("time_to_pct_s", {})

    fig, (ax_rt, ax_dc) = plt.subplots(
        2, 1, figsize=(11, 8.5), sharex=True, gridspec_kw={"height_ratios": [1, 1.1]}
    )

    title = (
        f"kad_bench discovery over time  --  bench {bench}, "
        f"{nodes} nodes, seed {seed}, k={params.get('k', '?')}, "
        f"alpha={params.get('alpha', '?')}, concurrent={params.get('concurrent_lookups_per_node', '?')}"
    )
    fig.suptitle(title, fontsize=12, fontweight="bold")

    # Panel 1: routing table size.
    ax_rt.fill_between(t_rt, rt_p50, rt_p95, alpha=0.20, color="tab:blue", label="p50–p95 spread")
    ax_rt.plot(t_rt, rt_max, color="tab:blue", lw=0.9, ls=":", alpha=0.7, label="max")
    ax_rt.plot(t_rt, rt_mean, color="tab:blue", lw=2.2, label="mean")
    ax_rt.set_ylabel("active peers per node\n(routing table)")
    ax_rt.set_ylim(bottom=0)
    ax_rt.grid(True, alpha=0.3)
    ax_rt.legend(loc="lower right", fontsize=9)
    final_mean = rt_mean[-1] if rt_mean else 0
    ax_rt.set_title(f"Active peers in DHT — stabilises at ~{final_mean:.0f}", fontsize=10)

    # Panel 2: network discovery completion with milestone marks.
    ax_dc.plot(c_t, c_p, color="tab:green", lw=2.2, marker="o", ms=4, label="resolved (network)")

    # Milestones cluster in a narrow time window, so fan the labels out into the
    # open space to the left of the rise and connect each with a leader line.
    milestone_colors = {"50": "#2ca02c", "80": "#bcbd22", "90": "#ff7f0e", "95": "#d62728", "99": "#9467bd"}
    reached = [(thr, ttp[thr]) for thr in ["50", "80", "90", "95", "99"] if ttp.get(thr) is not None]
    if reached:
        max_t = max(c_t) if c_t else 1.0
        last_t = reached[-1][1]
        # Park the labels in the open area to the right of the rise (well clear of
        # both the y-axis labels and the clustered milestone lines), stacked from
        # bottom to top and connected to each marker with a leader line.
        label_x = last_t + 0.16 * max_t
        lo, hi = 20.0, 78.0
        for i, (thr, t) in enumerate(reached):
            col = milestone_colors[thr]
            for ax in (ax_rt, ax_dc):
                ax.axvline(t, color=col, ls="--", lw=1.2, alpha=0.8)
            label_y = lo + (hi - lo) * (i / max(len(reached) - 1, 1))
            ax_dc.annotate(
                f"{thr}% @ {t:.1f}s",
                xy=(t, float(thr)),
                xytext=(label_x, label_y),
                ha="left", va="center",
                fontsize=9, color=col, fontweight="bold",
                arrowprops=dict(arrowstyle="->", color=col, lw=1.0, alpha=0.8),
            )

    # Plot checkpoints distinctly.
    for cp in disc.get("checkpoints", []):
        if cp.get("fired"):
            ax_dc.plot(cp["t_s"], cp["resolved_pct_network"], marker="D", ms=8,
                       color="black", zorder=5)
            ax_dc.annotate(
                f"checkpoint @{cp['t_s']}s\n{cp['resolved_pct_network']:.1f}%",
                xy=(cp["t_s"], cp["resolved_pct_network"]),
                xytext=(cp["t_s"], cp["resolved_pct_network"] - 14),
                ha="center", fontsize=8, color="black",
            )

    # Note DNF milestones (99/100 never reached).
    dnf = [thr for thr in ["99", "100"] if ttp.get(thr) is None]
    if dnf:
        ax_dc.text(
            0.99, 0.04,
            f"DNF: {', '.join(t + '%' for t in dnf)} not reached "
            f"(max {disc.get('max_pct_reached', '?')}%)",
            transform=ax_dc.transAxes, ha="right", va="bottom",
            fontsize=9, color="#d62728", style="italic",
            bbox=dict(boxstyle="round", fc="white", ec="#d62728", alpha=0.8),
        )

    ax_dc.set_ylabel("lookup units resolved\n(% of network)")
    ax_dc.set_xlabel("time into measurement phase (s)")
    ax_dc.set_ylim(0, 105)
    ax_dc.set_xlim(left=0)
    ax_dc.grid(True, alpha=0.3)
    ax_dc.legend(loc="center", fontsize=9)

    fig.tight_layout(rect=[0, 0, 1, 0.97])

    if args.show:
        plt.show()
    else:
        out = args.out or (args.results.rsplit(".", 1)[0] + ".discovery.png")
        fig.savefig(out, dpi=140)
        print(f"wrote {out}")


if __name__ == "__main__":
    main()
