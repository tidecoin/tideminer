// Whole-machine WASM throughput with N workers (each its own module instance and
// memory, like browser Web Workers), through the real `scan` API.
//   node bench/bench-workers.mjs <pkg-dir> <workers> <lanes> [seconds]
import { Worker, isMainThread, parentPort, workerData } from "node:worker_threads";
import { createRequire } from "node:module";
import path from "node:path";

if (isMainThread) {
  const [pkgDir, workers, lanes, seconds] = [
    path.resolve(process.argv[2]), Number(process.argv[3]), Number(process.argv[4]), Number(process.argv[5] ?? 20),
  ];
  const shared = new Int32Array(new SharedArrayBuffer(4));
  let ready = 0;
  const counts = [];
  const pool = Array.from({ length: workers }, (_, id) => {
    const w = new Worker(new URL(import.meta.url), { workerData: { pkgDir, lanes, id, shared } });
    w.on("message", (m) => {
      if (m === "ready" && ++ready === workers) {
        setTimeout(() => Atomics.store(shared, 0, 1), 3000); // 3 s warmup, then measure
        setTimeout(() => Atomics.store(shared, 0, 2), 3000 + seconds * 1000);
      } else if (typeof m === "object") {
        counts.push(m.hashes);
        if (counts.length === workers) {
          const total = counts.reduce((a, b) => a + b, 0) / seconds;
          console.log(`workers=${workers} lanes=${lanes}: ${total.toFixed(0)} H/s total, ${(total / workers).toFixed(0)} per worker`);
          pool.forEach((p) => p.terminate());
        }
      }
    });
    return w;
  });
} else {
  const require = createRequire(import.meta.url);
  const { pkgDir, lanes, id, shared } = workerData;
  const pkg = require(path.join(pkgDir, "tidecoin_yespower.js"));
  const miner = new pkg.YespowerMiner(lanes);
  const header = new Uint8Array(80).fill(id);
  const never = new Uint8Array(32);
  const batch = 4 * lanes;
  let n = 0;
  miner.scan(header, 0, batch, never);
  parentPort.postMessage("ready");
  while (Atomics.load(shared, 0) === 0) miner.scan(header, n += batch, batch, never);
  let hashes = 0;
  while (Atomics.load(shared, 0) === 1) {
    miner.scan(header, n += batch, batch, never);
    hashes += batch;
  }
  parentPort.postMessage({ hashes });
}
