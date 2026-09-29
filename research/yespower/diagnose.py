"""Diagnostic-only variants that isolate where CPU time goes in yespower.

These deliberately change the digest; they are not correctness experiments and
must never be used for mining. Each variant answers a question:

  baseline  unmodified optimized C
  d1        S-box indices forced to 0: removes the data-dependent S-load chain
            and all S-table cache traffic
  d2        V history index forced to 0: removes random V access traffic
  d4        S indices masked to 4 KiB: keeps the load dependency but makes the
            tables L1-resident
  d5        only the S1 index forced to 0: removes one of the two loads from
            the dependency chain
  h0        explicit per-round load hoisting with no hazard check: measures the
            upper bound of software-reordering PWXFORM (digest-breaking when a
            same-round write/read conflict occurs)
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent

PATCHES = {
    "d1": lambda s: s.replace(
        "uint32_t lo = x = EXTRACT64(X) & Smask2reg;",
        "uint32_t lo = x = 0; (void)Smask2reg;"),
    "d4": lambda s: s.replace(
        "uint32_t lo = x = EXTRACT64(X) & Smask2reg;",
        "uint32_t lo = x = EXTRACT64(X) & 0x00003ff000003ff0ULL;"),
    "d5": lambda s: s.replace(
        "uint32_t lo = x = EXTRACT64(X) & Smask2reg;",
        "uint32_t lo = x = (EXTRACT64(X) & 0x7ff000000000ULL);"),
}


def killv(s):
    before = s
    s = s.replace("j = integerify(X, r);", "j = 0; (void)integerify(X, r);")
    s = s.replace("j = blockmix_xor(X, V_j, Y, r, ctx);",
                  "j = blockmix_xor(X, V_j, Y, r, ctx) * 0;")
    s = s.replace("j = blockmix_xor(Y, V_j, X, r, ctx);",
                  "j = blockmix_xor(Y, V_j, X, r, ctx) * 0;")
    s = s.replace("j = blockmix_xor_save(X, V_j, r, ctx) & (N - 1);",
                  "j = blockmix_xor_save(X, V_j, r, ctx) * 0;")
    assert s != before
    return s


PATCHES["d2"] = killv


def hoist(s):
    s = s.replace(
        '#define PWXFORM_SIMD_WRITE(X, Sw)',
        '#include "hoist_round.h"\n\n#define PWXFORM_SIMD_WRITE(X, Sw)')
    old = ('\tPWXFORM_ROUND_WRITE4 PWXFORM_ROUND_WRITE2 PWXFORM_ROUND_WRITE2 '
           '\\\n')
    new = ('\t{ __m128i xx[4] = {X0,X1,X2,X3}; \\\n'
           '\t  hoist_round(xx,S0,S1,&w,1); '
           'hoist_round(xx,S0,S1,&w,0); '
           'hoist_round(xx,S0,S1,&w,0); \\\n'
           '\t  X0=xx[0]; X1=xx[1]; X2=xx[2]; X3=xx[3]; } \\\n')
    assert s.count(old) == 1
    return s.replace(old, new)


PATCHES["h0"] = hoist


def build(cc, folder, name, source):
    binary = folder / name
    subprocess.run([cc, "-O3", "-std=gnu99", "-funroll-loops",
                    "-fomit-frame-pointer", "-march=native", "-I", str(folder),
                    str(HERE / "driver.c"), str(folder / "yespower-opt.c"),
                    str(folder / "sha256.c"), "-o", str(binary)],
                   check=True, capture_output=True)
    return binary


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--cpu", type=int, default=2)
    p.add_argument("--hashes", type=int, default=2048)
    p.add_argument("--trials", type=int, default=3)
    p.add_argument("--variants", default="baseline,d1,d2,d4,d5")
    p.add_argument("--output", type=Path, default=None)
    p.add_argument("--source", type=Path, required=True, help="optimized yespower C source directory")
    args = p.parse_args()
    assert args.cpu in os.sched_getaffinity(0)

    variants = ["baseline"] + [v for v in args.variants.split(",") if v != "baseline"]
    summary = {}
    with tempfile.TemporaryDirectory(prefix="tideminer-diag-") as tmp:
        root = Path(tmp)
        for name in variants:
            folder = root / name
            shutil.copytree(args.source, folder)
            shutil.copy(HERE / "hoist_round.h", folder)
            if name != "baseline":
                src = (folder / "yespower-opt.c").read_text()
                patched = PATCHES[name](src)
                assert patched != src, name
                (folder / "yespower-opt.c").write_text(patched)
            binary = build("gcc", folder, "bench", None)
            rates = []
            for _ in range(args.trials):
                out = subprocess.check_output(
                    ["taskset", "-c", str(args.cpu), str(binary),
                     str(args.hashes)], text=True)
                rates.append(float(out.split('"hps":')[1].split(",")[0]))
            print(f"{name:9} median {statistics.median(rates):8.1f} H/s "
                  f"(min {min(rates):.1f}, max {max(rates):.1f})")
            summary[name] = {"median_hps": statistics.median(rates),
                             "min_hps": min(rates), "max_hps": max(rates)}
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(
            {"cpu": args.cpu, "hashes": args.hashes, "trials": args.trials,
             "variants": summary}, indent=2) + "\n")


if __name__ == "__main__":
    main()
