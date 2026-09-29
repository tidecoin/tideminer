"""Build the research-only two-hash pair kernel, validate every digest against
the independent scalar reference, then time it pinned to one CPU.

This is an experiment, not a production backend. The pair kernel interleaves two
independent hashes in one thread to overlap their dependent S-box chains.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent


def corpus(varied=256):
    vectors = json.loads(
        (HERE.parents[2] / "tests/fixtures/yespower.json").read_text())
    inputs = [bytes.fromhex(v["header"]) for v in vectors["vectors"]]
    for i in range(varied):
        inputs.append(b"".join(
            hashlib.sha256(f"research-{i}-{j}".encode()).digest()
            for j in range(3))[:80])
    return inputs


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--cpu", type=int, default=2)
    p.add_argument("--hashes", type=int, default=4096,
                   help="even number of hashes per timing mode")
    p.add_argument("--varied", type=int, default=256,
                   help="deterministic varied headers added to the fixture corpus")
    p.add_argument("--build-only", action="store_true")
    p.add_argument("--source", type=Path, required=True, help="optimized yespower C source directory")
    p.add_argument("--reference", type=Path, required=True, help="scalar yespower C reference directory")
    args = p.parse_args()
    assert args.cpu in os.sched_getaffinity(0)
    assert args.hashes % 2 == 0

    inputs = corpus(args.varied)
    raw = b"".join(inputs)
    result = {"cpu": args.cpu, "hashes": args.hashes,
              "reference_headers": len(inputs), "runs": []}

    with tempfile.TemporaryDirectory(prefix="tideminer-pair-") as tmp:
        root = Path(tmp)
        ref = root / "reference"
        subprocess.run(["gcc", "-O2", "-std=gnu99", "-I", str(args.reference),
                        str(HERE.parent / "driver.c"),
                        str(args.reference / "yespower-ref.c"),
                        str(args.reference / "sha256.c"), "-o", str(ref)],
                       check=True, capture_output=True)
        expected = subprocess.run([str(ref)], input=raw,
                                  capture_output=True, check=True).stdout
        assert len(expected) == 32 * len(inputs)

        folder = root / "pair"
        shutil.copytree(args.source, folder)
        shutil.copy(HERE / "pair_kernel.h", folder)
        with (folder / "yespower-opt.c").open("a") as f:
            f.write('\n#include "pair_kernel.h"\n')
        binary = folder / "bench_pair"
        flags = ["-O3", "-std=gnu99", "-funroll-loops",
                 "-fomit-frame-pointer", "-march=native"]
        subprocess.run(["gcc", *flags, "-I", str(folder),
                        str(HERE / "pair_driver.c"),
                        str(folder / "yespower-opt.c"),
                        str(folder / "sha256.c"), "-o", str(binary)],
                       check=True, capture_output=True)

        single = subprocess.run([str(binary)], input=raw,
                                capture_output=True, check=True).stdout
        assert single == expected, "single path differential mismatch"
        even = raw + raw[-80:]
        pair = subprocess.run([str(binary), "pairstdin"], input=even,
                              capture_output=True, check=True).stdout
        assert pair[:len(expected)] == expected, "pair path differential mismatch"
        result["parity"] = True
        print(f"validated single and pair paths against scalar reference: "
              f"{len(inputs)} headers", flush=True)
        if args.build_only:
            print(json.dumps(result, indent=2))
            return

        for trial in range(3):
            out = subprocess.check_output(
                ["taskset", "-c", str(args.cpu), str(binary), "bench",
                 str(args.hashes)], text=True)
            row = json.loads(out)
            row["trial"] = trial
            result["runs"].append(row)
            print(f"trial {trial + 1}: pair {row['pair_hps']:.1f} H/s, "
                  f"single {row['single_hps']:.1f} H/s, "
                  f"speedup {row['speedup']:.3f}", flush=True)

    result["summary"] = {
        "pair_median_hps": statistics.median(r["pair_hps"] for r in result["runs"]),
        "single_median_hps": statistics.median(r["single_hps"] for r in result["runs"]),
        "speedup_median": statistics.median(r["speedup"] for r in result["runs"]),
    }
    print(json.dumps(result["summary"], indent=2))


if __name__ == "__main__":
    main()
