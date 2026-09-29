# tidecoin-yespower

Pure-Rust Tidecoin yespower (yespower 1.0, N=2048, r=8, no personalization, 80-byte
header), written for WebAssembly first and usable natively. Output is bit-identical to
Openwall's reference and to `rust-yespower`'s optimized C.

- One kernel, four backends: WASM SIMD128, x86-64 SSE2, AArch64, portable 64-bit integers.
- `Hasher<K>` computes K independent hashes in lock-step. yespower is limited by the
  latency of data-dependent S-box loads, and interleaving independent hashes fills
  those stalls. K = 2 gives far more hashes per thread (per Web Worker) when the core
  has headroom.
- `scan()` runs the nonce loop inside the module; JavaScript calls it once per batch.
- Browser package: 99 KB `.wasm` (28 KB gzipped), plus a 119 KB scalar build: the
  fallback without SIMD128, and the faster one on ARM (Apple Silicon). Optional
  shared-memory variants of both keep every worker fast in Safari.

## Use

```rust
let mut h = tidecoin_yespower::Hasher::<2>::new();          // 2 lanes, ~4.2 MiB
let [a, b] = h.hash(&[header_a, header_b]);                 // raw digests
let (nonces, done) = h.scan(&header, start, count, &target_be); // digest (LE) <= target
let target = tidecoin_yespower::share_target(0.02).unwrap();    // cpuminer-compatible formula
```

JavaScript (one miner per Web Worker):

```js
import init, { YespowerMiner, shareTarget } from "./pkg-simd/tidecoin_yespower.js";
await init();
const miner = new YespowerMiner(2);                 // lanes: 1 or 2
const found = miner.scan(header80, start, 64, shareTarget(0.02)); // Uint32Array of nonces
```

## Build, test, benchmark

```sh
cargo test -p tidecoin-yespower                                     # parity vs vectors and C
taskset -c 2 cargo run --release -p tidecoin-yespower --example speed -- 3   # native, per lane width
./build-web.sh                  # web/pkg-{simd,scalar}, and pkg-shared-* with a nightly toolchain
python3 web/serve.py 8080          # open http://127.0.0.1:8080/ (or the printed LAN URL on a phone)
wasm-pack build --release --target nodejs --out-dir ../../target/yespower-pkg/node --features web
node bench/bench-node.mjs ../../target/yespower-pkg/node 4          # one thread, per lane width
node bench/bench-workers.mjs ../../target/yespower-pkg/node 32 1 20 # N workers, sustained
```

The page (`web/index.html`) measures which build is faster (Build: auto), verifies the
known-answer hash in every worker, sweeps workers × lanes and reports the best setup;
"Copy results" gives JSON. On a cross-origin-isolated page (`serve.py` sends the
COOP/COEP headers) with `pkg-shared-*` built, all workers share one WASM memory;
otherwise each worker has its own.

Correctness tests: the 19 independent scalar-reference vectors at K = 1, 3 and on the
portable backend; 160 random headers at K = 2 and 4 plus 40 at K = 1 against the C;
`scan` against the C including the 2^32 nonce wrap; unit tests for every SIMD op,
HMAC (RFC 4231) and PBKDF2 (RFC 7914).

## Measured (i9-13980HX, 2026-09-27)

Single thread, pinned to one core. Short runs vary by about ±4% with temperature.

| Build | P core (CPU 2) | E core (CPU 16) |
| --- | ---: | ---: |
| C, rust-yespower 0.3.0 (native SSE2) | 1,519-1,610 | 816-824 |
| Rust native, K = 1 (after the parity fixes, alternating with C) | 1,520 vs C 1,522 | 818 vs C 817 |
| Rust native, K = 2 | **2,070-2,084** | **1,107-1,149** |
| emscripten C, SIMD (Node 22) | 1,182-1,206 | |
| **Rust WASM SIMD, K = 1** (Node 22) | 1,340-1,407 | |
| **Rust WASM SIMD, K = 2** (Node 22) | 1,560-1,680 | |
| emscripten C, scalar (Node 22) | 1,087-1,137 | |
| Rust WASM no-SIMD fallback, K = 1 / 2 | 985 / 1,035 | |

Real browsers, the benchmark page, 10 s per setup (short runs include turbo):

| Setup | Chrome 147 | Firefox 156 |
| --- | ---: | ---: |
| 1 worker, 1 lane | 1,310 | 1,272 |
| 1 worker, 2 lanes | 1,640 | 1,581 |
| 16 workers, 2 lanes | 13,120 | 12,672 |
| 32 workers, 1 lane | 14,160 | 13,593 |

Whole machine, sustained (20 s after 3 s warmup, Node worker threads = Web Workers):

| Workers | emscripten C | Rust, 1 lane | Rust, 2 lanes |
| ---: | ---: | ---: | ---: |
| 8 | 7,752 | 8,384 (+8%) | **8,856 (+14%)** |
| 16 | 9,576 | **10,277 (+7%)** | 9,961 (+4%) |
| 32 | 11,029 | **11,710 (+6%)** | 8,983 (-19%) |

Two lanes win when cores are idle (phones with 2-4 workers, "use half my CPU"), and lose
at full load: 32 workers × 2 lanes need 134 MiB of scratch against a 36 MiB L3.

Same kernel, native threads vs WASM workers, 32 × 1 lane, 40 s each in A-B-B-A order:
native 12,498 / 13,079 vs WASM 12,097 / 12,294 in one session (95%), native 14,508 /
13,468 vs WASM 12,112 / 12,062 in another (86%). This laptop's thermal state moves
full-load results by 5-10% between sessions; single-thread WASM is 88-91% of native
with one lane and about 80% with two.

## Measured (Apple M3 Max, 14" MacBook Pro, 2026-09-28)

Three things decide WASM speed on Apple Silicon:

1. **Scalar beats SIMD128.** On x86 native and WASM run the same SIMD kernel (V8
   lowers SIMD128 almost 1:1 to SSE2), hence 86-95%. On ARM the fastest native kernel
   is 64-bit integer code (`Aarch64` backend; a NEON port measured 13-15% slower), and
   SIMD128 lowers to that slower NEON design. The page's Build `auto` measures both.
2. **Safari makes only 8 WASM memories per process fast.** Past 8
   (`maxNumWasmFastMemories`), a memory gets explicit bounds checks on every access:
   one worker per memory then runs 8 workers at full speed and the rest at -29%. One
   shared memory for all workers (`pkg-shared-*`) keeps every worker fast.
3. **What remains is WASM itself.** A WASM access is `memory base + one i32 index`, so
   every S-box load needs the table base added first (native: `ldr [base, offset]`),
   the +8 word a second add (arm64 has no `[reg + reg + imm]`), and there is no
   prefetch (a WASM prefetch proposal was dropped in 2021). Native code forced into that
   exact instruction pattern runs 1,532 H/s (77% of native); engine register pressure
   (V8 has 24 allocatable registers; its loop spills 11-26 times per sub-block against
   1 native) takes the rest.

1 lane per worker, 14 workers = all cores; native runs bracket the browser runs
(native 18-20.7 kH/s with heat):

| | 1 worker | 14 workers | vs native, 14 |
| --- | ---: | ---: | ---: |
| Native (`Aarch64` backend) | 1,980 | 18,000-20,700 | |
| Chrome 154: SIMD128 / scalar / **scalar, shared** | 1,065 / 1,361 | 12,087 / 13,522 / **14,250** | 62% / 70% / **~73%** |
| Safari 18.6: SIMD128 / scalar / **scalar, shared** | 898 / 1,356 | 10,065 / 12,250 / **14,300** | 52% / 63% / **~73%** |
| Node 26: SIMD128 / scalar / scalar, shared | 1,001 / 1,367 | 10,636 / 12,604 / 14,441 | |

Safari with separate memories, per worker: eight at ~1,010 H/s, six at ~720.

Tried and rejected (V8 via Node, bit-exact): a discarded load as prefetch (-0.7%: unlike
`prfm` it must retire), opaque second S-box bases (the extra add goes, spills rise:
-1.6%), a non-inlined BlockMix (more spills in the loop: -1.5 to -3%); V8 flags
`--no-wasm-loop-unrolling` (neutral) and `--turbo-instruction-scheduling` (-20%).

Tools: `node --print-wasm-code` shows V8's machine code. For Safari's engine,
`/System/Library/Frameworks/JavaScriptCore.framework/Versions/A/Helpers/jsc` runs the
web package (`jsc -m bench.mjs`, with `TextDecoder` shims) at Safari's speed and dumps
code with `--dumpOMGDisassembly=1 --numberOfWasmCompilerThreads=1`;
`--logWasmMemory=1` shows fast memories in use (`fast memories = 8/8`).

## Why the browser cannot reach native speed here

- **No even-lane widening multiply in WASM SIMD.** Each pwxform step needs
  `hi32 * lo32` per 64-bit lane: one `pshufd` + `pmuludq` natively. WASM can only say
  `i64x2.extmul_low_i32x4_u`, which V8 lowers to two unpacks + `pmuludq`, after two
  shuffles to gather the operands. That, plus a two-micro-op lane extract, makes a
  pwxform about 207 instructions in V8 against about 130 native. One lane sits at the
  boundary between latency and throughput, so it loses ~10%; two lanes are
  throughput-bound and lose ~20%; at full load the extra instructions cost power.
  Relaxed SIMD has no such multiply either. On ARM, `extmul_low` is a single `umull`,
  so phones and Apple Silicon should lose less (not measured yet).
- **15 usable vector registers in V8 on x86** (one pinned for the swizzle mask):
  two lanes spill to the stack.
- **No prefetch instruction** (natively worth ~6% here).

The dependency chain itself now matches native instruction for instruction (extract,
mask, load, add, xor); what remains is instruction count, which only a new WASM
instruction or a V8 pattern (`extmul` of two shuffles of one vector -> `pshufd` +
`pmuludq`) could remove. Firefox/WebKit numbers under Playwright's
patched builds are not representative (its Firefox runs the emscripten C at 122 H/s);
the Firefox figures above come from the regular Firefox 156 build.

## What made the WASM fast

1. **Real `pmuludq`.** pwxform needs `hi32 * lo32` per 64-bit lane. WASM spells that
   `i64x2.extmul_low_i32x4_u`, but LLVM rewrites the obvious even-lane shuffle into an
   AND mask, which breaks the pattern and emits `i64x2.mul`, which V8 emulates with
   3 multiplies and 5 fix-ups. Producing the even lanes with a constant
   `i8x16.swizzle` keeps the real extmul: +20% (K=1) and +38% (K=2).
2. **Byte-offset S-box addressing** (shift, and, add as in the C), instead of index
   arithmetic that became shift/and/shift chains on the critical path.
3. **S-box pointers and cursor in locals** during each BlockMix, so stores to the
   S-boxes cannot force reloads (also +4% native).
4. **Lanes innermost everywhere.** V8 emits code in source order; interleaving at
   instruction granularity is what exposes parallel S-box lookups. Lane-major order
   lost the whole K = 2 gain.
5. **No wasm-opt.** `-O`, `-O3`, `-O4` all made V8's code 2-5% slower.
6. **Native prefetch of V_j** (as the C does) closed 6% to the C natively.
7. **Native parity with the C, found by comparing machine code** (one lane was 3.9%
   behind on a P core, 2.0% on an E core):
   - *S-box address split*: the C does `movq; and rax, Smask2; mov ecx, eax;
     shr rax, 32`. Plain Rust makes LLVM mask each half separately, one more
     instruction per pwxform step (~1.05 M steps per hash). A 4-instruction `asm!`
     block on x86-64 matches the C. WASM keeps two masked 32-bit lane extracts,
     which V8 runs 3% faster than the 64-bit form.
   - *One XOR on the critical path*: BlockMix folds `b1[i] ^ b2[i]` into X. GCC
     computes `b1 ^ b2` from memory first and applies one XOR to X; LLVM re-associated
     it into `(X ^ b1) ^ b2`, two dependent XORs per block. llvm-mca (Golden Cove):
     smix1's loop 49.1 -> 46.2 cycles/block (C: 46.1). An `asm!` `pxor` keeps the
     grouping.
   - *Cache-line aligned scratch*: large allocations start 16 bytes past a page, so a
     1 KiB V entry spanned 17 cache lines instead of 16 (the C's scratch is
     page-aligned). All scratch is now 64-byte aligned (unit-tested).
   Result, 8 alternating rounds of 5 s: P core -0.12%, E core +0.18% (parity).

Measured and rejected: WASM "touch" loads in place of prefetch (-5%), `i64x2.mul`
and scalar multiply variants, `opt-level=2` (-25% at K = 2), fat LTO + strip (-2%),
3 and 4 lanes (x86 has 16 vector registers; V8 spills), masking both S-box indices
with one vector AND (-5%), and static S-boxes at link-time addresses with a
pwxform specialized per rotation phase: V8 then folds the table address into every
load (`[mem + index + CONST]`), but interleaved A/B runs showed no gain on P or E
cores while the module grew from 99 KB to 262 KB. (A quick non-interleaved run first
suggested +10%; it was thermal drift.) On ARM with 32 vector
registers wider lanes may behave differently: measure on the device.

## Where the time goes (native, i9-13980HX, measured 2026-09-27)

Diagnostic builds that deliberately break the output isolate each cost (speed only):

| Build | One E core | E cluster, 4 cores | One P core, both SMT threads | All 32 threads |
| --- | ---: | ---: | ---: | ---: |
| Real kernel | 807-821 | 2,065-2,162 | 1,924-2,017 | 12,659-15,390 |
| S-boxes confined to 4 KiB tables (fit L1) | 1,616-1,644 | 4,362-4,419 | 2,797-2,822 | |
| V confined to 64 entries (fits cache) | 1,020-1,025 | 4,188-4,241 | 2,518-2,539 | 21,283-22,109 |

- **S-box lookups missing L1 are the largest cost**: about half an E core's time and
  a third of a P core's. yespower's 3 x 32 KiB S-boxes cannot fit a 32 KiB (E) or
  48 KiB (P) L1, and each lookup address depends on the previous result, so it cannot
  be prefetched. This is by design of the algorithm.
- **The shared 4 MiB L2 of an E-core cluster** costs a third of its throughput with all
  four cores busy (1 core 819, 2: 1,517, 3: 1,982, 4: 2,156 total), at nearly the same
  clock (4 cores spread over 4 clusters: 790 each).
- **Keeping V out of the caches does not help**: streaming stores for V writes plus
  NTA prefetch for V reads lost 15-22% (V then comes back from DRAM). The hardware's
  caching of V is already the better trade.
- **SMT vs interleaving on a P core**: one core peaks at about 2,050 H/s either way
  (2 SMT threads x 1 lane 2,045-2,077; 1 thread x 2 lanes 1,952-2,022; both 1,970-1,994).
  "SMT off + 2 lanes" on all 8 P cores: 11,702 vs 12,116 with SMT on.
- **Layout**: all 32 threads beats P16 + 3 E per cluster (14,630 vs 14,371 average),
  so the fourth E core per cluster still earns its power.

## License

Original contributions are licensed under [MIT](LICENSE). The Rust kernel is
ported from Openwall yespower; derived portions retain its
[BSD-2-Clause notices](LICENSE-YESPOWER). Both sets of terms apply when distributing
the combined crate.
