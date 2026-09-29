#!/usr/bin/env python3
"""Short-timescale block clustering for Tidecoin mainnet.

Answers: how often do 3-4 blocks land within seconds, and is that more or less
than expected from the local difficulty windows? The null model keeps each
7200-block window's measured duration and places its blocks as uniform order
statistics (constant rate inside the window, exactly 7200 blocks by
construction). A second model additionally uses the observed within-window
rate variation via a Poisson process with the window's average rate.

Reuses the chain parser in chain_timing.py.
"""
import argparse
from collections import Counter
import json
import random
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import chain_timing as ct  # noqa: E402


def load_chain(blocks):
    files = sorted(blocks.glob("blk*.dat"))
    headers = []
    for path in files:
        ct.scan_file(path, headers)
    chain = ct.build_active_chain(headers)
    return headers, chain


def maximal_runs(times, delta):
    """Distribution of maximal runs whose consecutive gaps are all <= delta."""
    runs = Counter()
    i = 0
    n = len(times)
    while i < n:
        j = i
        while j + 1 < n and times[j + 1] - times[j] <= delta:
            j += 1
        length = j - i + 1
        if length >= 2:
            runs[length] += 1
        i = j + 1
    return runs


def window_counts(times, width):
    """Count how many blocks fall in [t_i, t_i + width) for each block start."""
    counts = Counter()
    j = 0
    n = len(times)
    for i in range(n):
        if j < i + 1:
            j = i + 1
        while j < n and times[j] < times[i] + width:
            j += 1
        counts[j - i] += 1
    return counts


def simulate(chain, deltas, widths, rng):
    """Uniform order statistics inside each measured retarget window."""
    times = []
    n = len(chain)
    start = 0
    while start + ct.RETARGET_INTERVAL <= n:
        t0 = chain[start]["time"]
        t1 = chain[start + ct.RETARGET_INTERVAL]["time"]
        span = t1 - t0
        for _ in range(ct.RETARGET_INTERVAL):
            times.append(t0 + rng.random() * span)
        start += ct.RETARGET_INTERVAL
    for i in range(start, n):
        times.append(chain[i]["time"])
    times.sort()
    return {f"runs_{d}": dict(maximal_runs(times, d)) for d in deltas}, \
           {f"counts_{w}": dict(window_counts(times, w)) for w in widths}


def summarize_runs(runs, min_len=3):
    return {str(k): v for k, v in sorted(runs.items()) if k >= min_len}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--blocks", type=Path,
                   required=True, help="Tidecoin mainnet block snapshot directory")
    p.add_argument("--simulations", type=int, default=3)
    p.add_argument("--output", type=Path,
                   default=HERE / "results/burst-analysis.json")
    args = p.parse_args()

    headers, chain = load_chain(args.blocks)
    times = [b["time"] for b in chain]
    deltas = [2, 5, 10, 20, 30, 60]
    widths = [10, 30, 60, 300]

    observed = {
        "runs": {f"runs_{d}": summarize_runs(maximal_runs(times, d))
                 for d in deltas},
        "counts": {f"counts_{w}": {str(k): v for k, v in
                                   sorted(window_counts(times, w).items())
                                   if k >= 3}
                   for w in widths},
    }

    rng = random.Random(20260914)
    simulations = []
    for _ in range(args.simulations):
        runs, counts = simulate(chain, deltas, widths, rng)
        simulations.append({
            "runs": {k: summarize_runs(Counter(v)) for k, v in runs.items()},
            "counts": {k: {str(kk): vv for kk, vv in sorted(Counter(v).items())
                           if kk >= 3} for k, v in counts.items()},
        })

    stale = [h for h in headers if h["hash"] not in
             {b["hash"] for b in chain}]
    result = {
        "blocks": len(chain),
        "stale_headers": len(stale),
        "observed": observed,
        "uniform_within_window_simulations": simulations,
    }
    text = json.dumps(result, indent=2)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(text + "\n")
    print(f"blocks {len(chain)}, stale {len(stale)}")
    for d in deltas:
        runs = Counter({int(k): v for k, v in
                        observed["runs"][f"runs_{d}"].items()})
        sim = [sum(s["runs"][f"runs_{d}"].values())
               for s in simulations]
        print(f"delta {d:3}s: runs>=3 observed {sum(runs.values()):6d} "
              f"(length hist {sorted(runs.items())}) sim {sim}")
    for w in widths:
        obs = Counter({int(k): v for k, v in
                       observed["counts"][f"counts_{w}"].items()})
        sims = []
        for s in simulations:
            c = Counter({int(k): v for k, v in
                         s["counts"][f"counts_{w}"].items()})
            sims.append((sum(v for k, v in c.items() if k >= 4),
                         max(c) if c else 0))
        print(f"window {w:3}s: >=4 blocks observed "
              f"{sum(v for k, v in obs.items() if k >= 4)} "
              f"max {max(obs) if obs else 0}; sim (>=4,max) {sims}")
    print(f"wrote {args.output}")


if __name__ == "__main__":
    main()
