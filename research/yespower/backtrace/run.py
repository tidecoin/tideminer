"""Build an independent SMix dependency evaluator; check full digests and slice.

The slice is conservative at bit level and includes lookup-address dependencies.
It is an offline diagnostic, not a mining kernel or a proof of minimality.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--varied", type=int, default=8)
    p.add_argument("--output", type=Path, default=HERE.parent / "results/backtrace.json")
    p.add_argument("--reference", type=Path, required=True, help="scalar yespower C reference directory")
    args = p.parse_args()
    assert args.varied >= 0
    fixtures = json.loads((ROOT / "tests/fixtures/yespower.json").read_text())["vectors"]
    headers = [v["header"] for v in fixtures]
    headers += [b"".join(hashlib.sha256(f"backtrace-{i}-{j}".encode()).digest()
                         for j in range(3))[:80].hex() for i in range(args.varied)]
    result = {"parameters": {"version": "1.0", "N": 2048, "r": 8, "pers": None},
              "method": "conservative bit-mask dynamic slice including address dependencies",
              "source_sha256": {}, "headers": []}
    for path in [HERE / "trace.cpp", HERE / "run.py", args.reference / "yespower-ref.c",
                 args.reference / "sha256.c"]:
        result["source_sha256"][path.name] = hashlib.sha256(path.read_bytes()).hexdigest()
    with tempfile.TemporaryDirectory(prefix="tideminer-backtrace-") as tmp:
        tmp = Path(tmp)
        subprocess.run(["gcc", "-O2", "-I", str(args.reference), "-c",
                        str(args.reference / "sha256.c"), "-o", str(tmp / "sha.o")], check=True)
        subprocess.run(["g++", "-O2", "-std=c++17", "-Wall", "-Wextra", "-I", str(args.reference),
                        str(HERE / "trace.cpp"), str(tmp / "sha.o"), "-o", str(tmp / "trace")], check=True)
        subprocess.run(["gcc", "-O2", "-I", str(args.reference), str(HERE.parent / "driver.c"),
                        str(args.reference / "yespower-ref.c"), str(tmp / "sha.o"),
                        "-o", str(tmp / "reference")], check=True)
        expected = subprocess.run([str(tmp / "reference")], input=bytes.fromhex("".join(headers)),
                                  capture_output=True, check=True).stdout
        assert len(expected) == 32 * len(headers)
        for i, header in enumerate(headers):
            digest = expected[i*32:(i+1)*32].hex()
            if i < len(fixtures):
                assert digest == fixtures[i]["hash"]
            row = {"header": header, "digest": digest, "modes": {}}
            for mode in ["normal", "split"]:
                command = [str(tmp / "trace"), header] + (["split"] if mode == "split" else [])
                trace = json.loads(subprocess.check_output(command, text=True))
                assert trace.pop("digest") == digest, (i, mode, "digest mismatch")
                row["modes"][mode] = trace
            assert row["modes"]["normal"] == row["modes"]["split"], "slice differs after exact reorder"
            result["headers"].append(row)
            print(f"{i+1}/{len(headers)}: normal/split digests and slices agree", flush=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(f"saved {args.output}")


if __name__ == "__main__":
    main()
