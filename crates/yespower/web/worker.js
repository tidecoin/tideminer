// One benchmark/mining worker: its own WASM instance and ~2.1 MiB per lane, in its own
// memory or, with `module` given, in the page's shared memory (pkg-shared-*).
const KAT_HEADER = "0000002009f42768de3cfb4e58fc56368c1477f87f60e248d7130df3fb8acd7f6208b83a72f90dd3ad8fe06c7f70d73f256f1e07185dcc217a58b9517c699226ac0297d2ad60ba61b62a021d9b7700f0";
const KAT_HASH = "9d90c21b5a0bb9566d2999c5d703d7327ee3ac97c020d387aa2dfd0700000000";
const fromHex = (s) => Uint8Array.from(s.match(/../g), (b) => parseInt(b, 16));
const toHex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");

let miner;
let exports;
let joinedMemory = false;

onmessage = async ({ data }) => {
  try {
    if (data.type === "init") {
      const build = data.simd ? "simd" : "scalar";
      let memory;
      let simdEnabled;
      if (data.module) {
        // Shared memory: the first worker creates it (without `memory`) and returns it.
        const pkg = await import(`./pkg-shared-${build}/tidecoin_yespower.js`);
        exports = pkg.initSync({ module: data.module, memory: data.memory });
        joinedMemory = Boolean(data.memory);
        memory = exports.memory;
        miner = new pkg.YespowerMiner(data.lanes);
        simdEnabled = pkg.simdEnabled();
      } else {
        const pkg = await import(`./pkg-${build}/tidecoin_yespower.js`);
        await pkg.default();
        miner = new pkg.YespowerMiner(data.lanes);
        simdEnabled = pkg.simdEnabled();
      }
      const kat = toHex(miner.hash(fromHex(KAT_HEADER))) === KAT_HASH;
      postMessage({ type: "ready", kat, simd: simdEnabled, memory });
    } else if (data.type === "run") {
      // Distinct header per worker; the target of all zeros is never met.
      const header = new Uint8Array(80);
      header[0] = data.id & 0xff;
      header[1] = data.id >> 8;
      const never = new Uint8Array(32);
      const batch = 4 * miner.lanes;
      let nonce = 0;
      const t0 = performance.now();
      while (performance.now() - t0 < data.warmupMs) {
        miner.scan(header, nonce, batch, never);
        nonce += batch;
      }
      const t1 = performance.now();
      let hashes = 0;
      while (performance.now() - t1 < data.ms) {
        miner.scan(header, nonce, batch, never);
        nonce += batch;
        hashes += batch;
      }
      postMessage({ type: "result", hashes, seconds: (performance.now() - t1) / 1000 });
    } else if (data.type === "free") {
      // In a shared memory, return the scratch and this thread's stack before terminate().
      // Only joining threads: the creating thread runs on the module's static stack, and
      // destroying it corrupts the allocator (the next workers then hang).
      miner?.free();
      miner = undefined;
      if (joinedMemory) exports.__wbindgen_thread_destroy();
      postMessage({ type: "freed" });
    }
  } catch (error) {
    postMessage({ type: "error", message: String(error) });
  }
};
