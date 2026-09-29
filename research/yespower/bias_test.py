"""Per-nonce and per-time uniformity probe for Tidecoin yespower.

Builds bias.c against the vendored optimized C, runs one process per CPU in
--cpus (each with a disjoint start offset), merges the histograms and reports
chi-square statistics for the most significant digest byte (the byte the target
comparison reads first). This does not prove the output is unbiased; it bounds
gross structure at the sample size used.
"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--cpus", default="0,2,4,6,8,10,12,14")
    p.add_argument("--per-cpu", type=int, default=131072)
    p.add_argument("--output", type=Path,
                   default=HERE / "results/bias.json")
    p.add_argument("--source", type=Path, required=True, help="optimized yespower C source directory")
    args = p.parse_args()
    cpus = [int(c) for c in args.cpus.split(",")]

    with tempfile.TemporaryDirectory(prefix="tideminer-bias-") as tmp:
        root = Path(tmp)
        shutil.copytree(args.source, root / "yespower")
        folder = root / "yespower"
        binary = folder / "bias"
        subprocess.run(
            ["gcc", "-O3", "-std=gnu99", "-funroll-loops",
             "-fomit-frame-pointer", "-march=native", "-I", str(folder),
             str(HERE / "bias.c"), str(folder / "yespower-opt.c"),
             str(folder / "sha256.c"), "-o", str(binary)],
            check=True, capture_output=True)

        result = {"per_cpu": args.per_cpu, "cpus": cpus, "modes": {}}
        for mode in ("nonce", "time"):
            procs = []
            for i, cpu in enumerate(cpus):
                out = root / f"{mode}_{i}.json"
                procs.append((out, subprocess.Popen(
                    ["taskset", "-c", str(cpu), str(binary), mode,
                     str(i * args.per_cpu), str(args.per_cpu)],
                    stdout=out.open("w"))))
            for _, proc in procs:
                assert proc.wait() == 0
            top = [0] * 256
            cond = [[0] * 256 for _ in range(16)]
            adjacent_equal = 0
            n = 0
            for out, _ in procs:
                row = json.loads(out.read_text())
                for i, v in enumerate(row["top"]):
                    top[i] += v
                for j in range(16):
                    for i, v in enumerate(row["cond"][j]):
                        cond[j][i] += v
                adjacent_equal += row["adjacent_equal"]
                n += row["count"]
            expected = n / 256
            chi_top = sum((x - expected) ** 2 / expected for x in top)
            ec = n / 4096
            chi_cond = sum((cond[j][i] - ec) ** 2 / ec
                           for j in range(16) for i in range(256))
            result["modes"][mode] = {
                "hashes": n,
                "chi2_top_byte_df255": chi_top,
                "chi2_dp4_x_top_df4095": chi_cond,
                "adjacent_equal_rate": adjacent_equal / n,
                "expected_adjacent_equal_rate": 1 / 256,
                "top_histogram": top,
            }
            print(f"{mode}: {n} hashes, top-byte chi2 {chi_top:.1f} (df 255), "
                  f"cond chi2 {chi_cond:.1f} (df 4095)", flush=True)

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(f"wrote {args.output}")


if __name__ == "__main__":
    main()
