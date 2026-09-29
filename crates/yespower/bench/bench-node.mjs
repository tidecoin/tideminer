// Single-thread WASM throughput per lane width, through the real `scan` API.
//   node bench/bench-node.mjs <pkg-dir> [seconds]
import { createRequire } from "node:module";
import path from "node:path";

const require = createRequire(import.meta.url);
const pkg = require(path.resolve(process.argv[2] ?? "../../target/yespower-pkg/node", "tidecoin_yespower.js"));
const seconds = Number(process.argv[3] ?? 4);

const hex = (s) => Uint8Array.from(s.match(/../g).map((b) => parseInt(b, 16)));
const toHex = (b) => Buffer.from(b).toString("hex");
const header = hex("0000002009f42768de3cfb4e58fc56368c1477f87f60e248d7130df3fb8acd7f6208b83a72f90dd3ad8fe06c7f70d73f256f1e07185dcc217a58b9517c699226ac0297d2ad60ba61b62a021d9b7700f0");
const expected = "9d90c21b5a0bb9566d2999c5d703d7327ee3ac97c020d387aa2dfd0700000000";
const never = new Uint8Array(32); // target 0: nothing qualifies

console.log(`simd128: ${pkg.simdEnabled()}`);
for (const lanes of (process.argv[4] ?? "1,2,3,4").split(",").map(Number)) {
  const miner = new pkg.YespowerMiner(lanes);
  const got = toHex(miner.hash(header));
  if (got !== expected) throw new Error(`KAT failed for lanes=${lanes}: ${got}`);
  miner.scan(header, 0, 8 * lanes, never); // warm up JIT tiers
  const batch = 16 * lanes;
  let n = 0;
  const t0 = performance.now();
  while (performance.now() - t0 < seconds * 1000) {
    miner.scan(header, n, batch, never);
    n += batch;
  }
  const rate = n / ((performance.now() - t0) / 1000);
  console.log(`lanes=${lanes}  ${rate.toFixed(0)} H/s  (KAT ok)`);
  miner.free();
}
