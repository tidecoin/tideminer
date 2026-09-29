#!/usr/bin/env python3
"""Tidecoin main-chain timing statistics from raw block files.

Parses the Bitcoin-style blk*.dat records, links headers by previous-hash,
selects the maximum-work chain and reports interval, retarget-window, time-of-day
and burstiness statistics. The chain files are read-only inputs; nothing is
written except the requested JSON output.

Only the timing of the chain is measured. Proof-of-work validity is not checked.
"""
import argparse
import hashlib
import json
import statistics
import struct
import time
from collections import Counter, defaultdict
from pathlib import Path

MAINNET_MAGIC = bytes.fromhex("ecfacea5")
TARGET_SPACING = 60          # consensus.nPowTargetSpacing
TARGET_TIMESPAN = 5 * 24 * 3600  # consensus.nPowTargetTimespan (old rules)
RETARGET_INTERVAL = TARGET_TIMESPAN // TARGET_SPACING  # 7200

POW_LIMIT = int.from_bytes(
    bytes.fromhex("01ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"),
    "little")


def target_from_bits(bits):
    exponent = bits >> 24
    mantissa = bits & 0x007FFFFF
    if exponent <= 3:
        return mantissa >> (8 * (3 - exponent))
    return mantissa << (8 * (exponent - 3))


def work_from_bits(bits):
    target = target_from_bits(bits)
    return (1 << 256) // (target + 1) if target else 0


def scan_file(path, headers):
    data = path.read_bytes()
    n = len(data)
    off = 0
    while off + 8 <= n:
        if data[off:off + 4] != MAINNET_MAGIC:
            nxt = data.find(MAINNET_MAGIC, off + 1)
            if nxt < 0:
                break
            off = nxt
            continue
        size = int.from_bytes(data[off + 4:off + 8], "little")
        end = off + 8 + size
        if size < 80 or end > n:
            break
        raw = data[off + 8:off + 88]
        prev = raw[4:36]
        time = struct.unpack_from("<I", raw, 68)[0]
        bits = struct.unpack_from("<I", raw, 72)[0]
        nonce = struct.unpack_from("<I", raw, 76)[0]
        h = hashlib.sha256(hashlib.sha256(raw).digest()).digest()
        headers.append({"hash": h, "prev": prev, "time": time,
                        "bits": bits, "nonce": nonce})
        off = end
    return headers


def build_active_chain(headers):
    by_hash = {h["hash"]: i for i, h in enumerate(headers)}
    is_parent = {h["prev"] for h in headers}
    tips = [i for i, h in enumerate(headers) if h["hash"] not in is_parent]
    best, best_work = None, -1
    for tip in tips:
        idx = tip
        work = 0
        seen = 0
        while idx is not None:
            work += work_from_bits(headers[idx]["bits"])
            seen += 1
            idx = by_hash.get(headers[idx]["prev"])
            if seen > len(headers):
                raise RuntimeError("cycle in block graph")
        if work > best_work:
            best, best_work = tip, work
    chain = []
    idx = best
    while idx is not None:
        chain.append(headers[idx])
        idx = by_hash.get(headers[idx]["prev"])
    chain.reverse()
    return chain


def interval_stats(times):
    d = [b - a for a, b in zip(times, times[1:])]
    s = sorted(d)

    def q(p):
        return s[min(len(s) - 1, int(p * len(s)))]

    return {
        "count": len(d),
        "mean": statistics.fmean(d),
        "median": statistics.median(d),
        "std": statistics.pstdev(d),
        "cv": statistics.pstdev(d) / statistics.fmean(d),
        "p01": q(0.01), "p10": q(0.10), "p90": q(0.90), "p99": q(0.99),
        "min": min(d), "max": max(d),
        "fraction_negative": sum(1 for x in d if x < 0) / len(d),
        "fraction_under_30": sum(1 for x in d if 0 <= x < 30) / len(d),
        "fraction_over_120": sum(1 for x in d if x > 120) / len(d),
    }


def window_stats(chain):
    windows = []
    for start in range(0, len(chain) - RETARGET_INTERVAL, RETARGET_INTERVAL):
        block = chain[start:start + RETARGET_INTERVAL + 1]
        if len(block) < RETARGET_INTERVAL + 1:
            break
        duration = block[-1]["time"] - block[0]["time"]
        windows.append({
            "height": start,
            "start_time": block[0]["time"],
            "end_time": block[-1]["time"],
            "duration": duration,
            "ratio_to_target": duration / TARGET_TIMESPAN,
            "mean_interval": duration / RETARGET_INTERVAL,
        })
    return windows


def fano_and_time_of_day(times):
    by_hour = Counter(t // 3600 for t in times)
    hours = sorted(by_hour)
    span = [by_hour.get(h, 0) for h in range(hours[0], hours[-1] + 1)]
    mean = statistics.fmean(span)
    fano = statistics.pvariance(span) / mean if mean else 0
    tod = Counter((t // 3600) % 24 for t in times)
    tod_counts = [tod.get(h, 0) for h in range(24)]
    tod_mean = statistics.fmean(tod_counts)
    tod_chi = sum((c - tod_mean) ** 2 / tod_mean for c in tod_counts)
    dow = Counter((t // 86400 + 4) % 7 for t in times)  # epoch was a Thursday
    dow_counts = [dow.get(d, 0) for d in range(7)]
    dow_mean = statistics.fmean(dow_counts)
    dow_chi = sum((c - dow_mean) ** 2 / dow_mean for c in dow_counts)
    hourly = [by_hour.get(h, 0) for h in range(hours[0], hours[-1] + 1)]
    return {
        "hours_observed": len(hourly),
        "mean_blocks_per_hour": mean,
        "fano_factor_hourly": fano,
        "time_of_day_counts_utc": tod_counts,
        "time_of_day_chi2_23df": tod_chi,
        "day_of_week_counts": dow_counts,
        "day_of_week_chi2_6df": dow_chi,
    }, hourly


def autocorrelation(x, lags):
    n = len(x)
    mean = statistics.fmean(x)
    var = statistics.pvariance(x)
    if var == 0:
        return {lag: 0.0 for lag in lags}
    out = {}
    for lag in lags:
        s = sum((x[i] - mean) * (x[i + lag] - mean)
                for i in range(n - lag))
        out[lag] = s / ((n - lag) * var)
    return out


def detrended_time_of_day(times):
    """Average each UTC hour's share of its day, removing daily totals.

    Reports the peak-to-trough diurnal amplitude and the standard error of the
    per-hour mean across days, so an amplitude can be compared with sampling
    noise (the chi-square version is inflated by within-day clustering).
    """
    by_day = defaultdict(Counter)
    for t in times:
        by_day[t // 86400][(t // 3600) % 24] += 1
    days = sorted(by_day)
    fractions = [[0.0] * 24 for _ in days]
    for i, day in enumerate(days):
        total = sum(by_day[day].values())
        for h in range(24):
            fractions[i][h] = by_day[day].get(h, 0) / total
    hour_mean = [statistics.fmean(f[h] for f in fractions) for h in range(24)]
    hour_se = [statistics.pstdev(f[h] for f in fractions) / len(days) ** 0.5
               for h in range(24)]
    grand = statistics.fmean(hour_mean)
    z = [(hour_mean[h] - grand) / hour_se[h] for h in range(24)]
    return {
        "hour_fraction_mean": [round(x, 6) for x in hour_mean],
        "hour_fraction_se": [round(x, 8) for x in hour_se],
        "amplitude_max_minus_min": max(hour_mean) - min(hour_mean),
        "max_abs_z": max(abs(x) for x in z),
        "peak_hour_utc": max(range(24), key=lambda h: hour_mean[h]),
        "trough_hour_utc": min(range(24), key=lambda h: hour_mean[h]),
    }


def per_year(chain):
    years = defaultdict(lambda: {"blocks": 0, "first_time": None, "last_time": None})
    for b in chain:
        y = time.gmtime(b["time"]).tm_year
        e = years[y]
        e["blocks"] += 1
        if e["first_time"] is None:
            e["first_time"] = b["time"]
        e["last_time"] = b["time"]
    out = []
    for y in sorted(years):
        e = years[y]
        span = max(1, e["last_time"] - e["first_time"])
        out.append({"year": y, "blocks": e["blocks"],
                    "span_days": round(span / 86400, 2),
                    "mean_interval": round(span / max(1, e["blocks"] - 1), 3)})
    return out


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--blocks", type=Path,
                   required=True, help="Tidecoin mainnet block snapshot directory")
    p.add_argument("--output", type=Path, default=None)
    p.add_argument("--max-files", type=int, default=0)
    args = p.parse_args()

    files = sorted(args.blocks.glob("blk*.dat"))
    if args.max_files:
        files = files[:args.max_files]
    headers = []
    for path in files:
        scan_file(path, headers)
        print(f"scanned {path.name}: {len(headers)} records", flush=True)

    chain = build_active_chain(headers)
    times = [b["time"] for b in chain]
    bits = [b["bits"] for b in chain]
    retargets = [i for i in range(1, len(bits)) if bits[i] != bits[i - 1]]
    windows = window_stats(chain)
    tod, hourly = fano_and_time_of_day(times)

    result = {
        "block_source": "Tidecoin mainnet block snapshot",
        "files": len(files),
        "records_scanned": len(headers),
        "active_chain_blocks": len(chain),
        "height_first": 0,
        "height_last": len(chain) - 1,
        "time_first": times[0],
        "time_last": times[-1],
        "wall_seconds": times[-1] - times[0],
        "nbits_changes": len(retargets),
        "nbits_change_heights": retargets[:20],
        "nbits_change_spacing_median": (
            statistics.median([b - a for a, b in zip(retargets, retargets[1:])])
            if len(retargets) > 1 else 0),
        "intervals": interval_stats(times),
        "retarget_windows": {
            "count": len(windows),
            "duration_min": min(w["duration"] for w in windows),
            "duration_median": statistics.median(w["duration"] for w in windows),
            "duration_max": max(w["duration"] for w in windows),
            "ratio_min": min(w["ratio_to_target"] for w in windows),
            "ratio_median": statistics.median(w["ratio_to_target"] for w in windows),
            "ratio_max": max(w["ratio_to_target"] for w in windows),
            "slowest": sorted(windows, key=lambda w: -w["duration"])[:5],
            "fastest": sorted(windows, key=lambda w: w["duration"])[:5],
        },
        "time_of_day": tod,
        "detrended_time_of_day": detrended_time_of_day(times),
        "per_year": per_year(chain),
        "window_ratios": [round(w["ratio_to_target"], 4) for w in windows],
        "window_times": [[w["start_time"], w["end_time"],
                          round(w["ratio_to_target"], 6)] for w in windows],
        "hourly_autocorrelation": autocorrelation(hourly, [1, 2, 3, 24, 168]),
        "window_ratio_autocorrelation": autocorrelation(
            [w["ratio_to_target"] for w in windows], [1, 2, 3]),
        "hourly_counts": hourly,
    }
    text = json.dumps(result, indent=2)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(text + "\n")
        print(f"wrote {args.output}")
    print(json.dumps({k: v for k, v in result.items()
                      if k not in ("hourly_counts",)}, indent=2)[:4000])


if __name__ == "__main__":
    main()
