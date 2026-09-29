"""Regenerate fixtures with independent scalar C, never the optimized Rust backend."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile

DRIVER = r"""
#include <stdio.h>
#include "yespower.h"
int main(void) {
    uint8_t header[80];
    yespower_binary_t output;
    yespower_params_t params = {YESPOWER_1_0, 2048, 8, NULL, 0};
    size_t n;
    while ((n = fread(header, 1, sizeof(header), stdin)) != 0) {
        if (n != sizeof(header)) return 2;
        if (yespower_tls(header, sizeof(header), &params, &output)) return 3;
        if (fwrite(&output, 1, 32, stdout) != 32) return 4;
    }
    return ferror(stdin) ? 5 : 0;
}
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True, help="yespower opt/ directory")
    parser.add_argument("--cc", default="cc")
    args = parser.parse_args()
    source = args.source.resolve()
    inputs = [("zero", bytes(80)), ("ones", bytes([255]) * 80),
              ("ascending", bytes(range(80)))]
    for nonce in [1, 255, 256, 65535, 65536, 0x7fffffff, 0x80000000, 0xffffffff]:
        inputs.append((f"nonce-{nonce:08x}", bytes(76) + nonce.to_bytes(4, "little")))
    for i in range(8):
        header = b"".join(hashlib.sha256(f"tideminer-vector-{i}-{j}".encode()).digest()
                          for j in range(3))[:80]
        inputs.append((f"sha256-derived-{i}", header))
    with tempfile.TemporaryDirectory(prefix="tideminer-vectors-") as directory:
        temporary = Path(directory)
        driver = temporary / "driver.c"
        driver.write_text(DRIVER)
        binary = temporary / "oracle"
        subprocess.run([args.cc, "-O2", "-std=c99", "-I", str(source), str(driver),
                        str(source / "yespower-ref.c"), str(source / "sha256.c"),
                        "-o", str(binary)], check=True)
        output = subprocess.run([str(binary)], input=b"".join(h for _, h in inputs),
                                capture_output=True, check=True).stdout
    assert len(output) == 32 * len(inputs)
    files = ["yespower-ref.c", "yespower.h", "sha256.c", "sha256.h",
             "sysendian.h", "insecure_memzero.h"]
    fixture = {
        "oracle": "scalar yespower-ref.c, YESPOWER_1_0, N=2048, r=8, no personalization",
        "source_sha256": {f: hashlib.sha256((source / f).read_bytes()).hexdigest() for f in files},
        "vectors": [{"name": name, "header": header.hex(),
                     "hash": output[i * 32:(i + 1) * 32].hex()}
                    for i, (name, header) in enumerate(inputs)],
    }
    path = Path(__file__).resolve().parents[1] / "tests/fixtures/yespower.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(fixture, indent=2) + "\n")
    print(f"Wrote {len(inputs)} independent vectors to {path}")


if __name__ == "__main__":
    main()
