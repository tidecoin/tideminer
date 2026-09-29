"""Validate and benchmark exact terminal pruning / lazy V rewrites against C.

Runs randomized adjacent baseline/variant pairs on one pinned CPU. These are
short exploratory measurements, not steady-state full-chip mining forecasts.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import statistics
import subprocess
import tempfile
from variants import transform

HERE = Path(__file__).resolve().parent


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--cpu", type=int, default=2)
    p.add_argument("--hashes", type=int, default=2048)
    p.add_argument("--trials", type=int, default=5)
    p.add_argument("--varied", type=int, default=256)
    p.add_argument("--output", type=Path, default=HERE.parent / "results/backtrace-cpu.json")
    p.add_argument("--source", type=Path, required=True, help="optimized yespower C source directory")
    p.add_argument("--reference", type=Path, required=True, help="scalar yespower C reference directory")
    args = p.parse_args()
    assert args.cpu in os.sched_getaffinity(0)
    assert args.hashes > 0 and args.trials > 0 and args.varied >= 0
    fixtures = json.loads((HERE.parents[2] / "tests/fixtures/yespower.json").read_text())["vectors"]
    inputs = [bytes.fromhex(v["header"]) for v in fixtures]
    inputs += [b"".join(hashlib.sha256(f"backtrace-variant-{i}-{j}".encode()).digest()
                        for j in range(3))[:80] for i in range(args.varied)]
    result = {"cpu": args.cpu, "hashes": args.hashes, "trials": args.trials,
              "reference_headers": len(inputs), "runs": [], "builds": {},
              "cpuinfo": Path("/proc/cpuinfo").read_text().split("\n\n")[0],
              "sources": {f.name: hashlib.sha256(f.read_bytes()).hexdigest()
                          for f in [HERE / "variants.py", HERE / "bench.py",
                                    args.source / "yespower-opt.c", args.reference / "yespower-ref.c"]}}
    with tempfile.TemporaryDirectory(prefix="tideminer-backtrace-bench-") as tmp:
        tmp = Path(tmp)
        ref = tmp / "reference"
        subprocess.run(["gcc", "-O2", "-I", str(args.reference), str(HERE.parent / "driver.c"),
                        str(args.reference / "yespower-ref.c"), str(args.reference / "sha256.c"),
                        "-o", str(ref)], check=True, capture_output=True)
        expected = subprocess.run([str(ref)], input=b"".join(inputs), capture_output=True, check=True).stdout
        assert len(expected) == len(inputs)*32
        for i, fixture in enumerate(fixtures):
            assert expected[32*i:32*(i+1)].hex() == fixture["hash"]
        binaries = {}
        flags = ["-O3", "-std=gnu99", "-funroll-loops", "-fomit-frame-pointer", "-march=native"]
        for name in ["baseline", "tail", "lazy_v"]:
            folder = tmp / name
            shutil.copytree(args.source, folder)
            code = transform((folder / "yespower-opt.c").read_text(), name)
            (folder / "yespower-opt.c").write_text(code)
            binary = folder / "bench"
            cmd = ["gcc", *flags, "-I", str(folder), str(HERE.parent / "driver.c"),
                   str(folder / "yespower-opt.c"), str(folder / "sha256.c"), "-o", str(binary)]
            build = subprocess.run(cmd, capture_output=True, text=True)
            if build.returncode:
                raise RuntimeError(name + "\n" + build.stderr)
            actual = subprocess.run([str(binary)], input=b"".join(inputs), capture_output=True, check=True).stdout
            assert actual == expected, f"{name}: scalar-reference mismatch"
            binaries[name] = binary
            result["builds"][name] = {"flags": flags, "parity": True,
                "compiler": subprocess.check_output(["gcc", "--version"], text=True).splitlines()[0],
                "transformed_sha256": hashlib.sha256(code.encode()).hexdigest()}
            print(f"{name}: {len(inputs)} reference headers passed", flush=True)
        rng = random.Random(20260914)
        for trial in range(args.trials):
            variants = ["tail", "lazy_v"]
            rng.shuffle(variants)
            for variant in variants:
                order = ["baseline", variant]
                rng.shuffle(order)
                rows = {}
                for name in order:
                    rows[name] = json.loads(subprocess.check_output(
                        ["taskset", "-c", str(args.cpu), str(binaries[name]), str(args.hashes)], text=True))
                assert rows["baseline"]["xor"] == rows[variant]["xor"]
                speedup = rows[variant]["hps"] / rows["baseline"]["hps"]
                result["runs"].append({"trial": trial, "variant": variant, "order": order,
                                       "rows": rows, "speedup": speedup})
                print(f"{trial+1}/{args.trials} {variant}: {speedup:.4f}x", flush=True)
        result["summary"] = {}
        for name in ["tail", "lazy_v"]:
            ratios = [r["speedup"] for r in result["runs"] if r["variant"] == name]
            result["summary"][name] = {"median_paired_speedup": statistics.median(ratios),
                                      "min": min(ratios), "max": max(ratios)}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result["summary"], indent=2))


if __name__ == "__main__":
    main()
