# Reproducible performance work

## Baseline contract

Use yespower 1.0, N=2048, r=8, no personalization and 80-byte headers everywhere.
Record repository commits **and uncommitted diffs**, C compiler and flags, Rust
version/flags, CPU model/microcode, affinity/cpuset, NUMA, memory policy, OS, power
profile and cooling. Do not run competing benchmarks simultaneously.

Build the portable reference:

```sh
cargo build --release --locked
./target/release/tideminer self-test
./target/release/tideminer bench -t 1 --hashes 16384 --warmup 64
```

Use a separate target directory to avoid mixing native and generic artifacts:

```sh
CFLAGS='-O3 -march=native' RUSTFLAGS='-C target-cpu=native' \
  CARGO_TARGET_DIR=target/native cargo build --release --locked
./target/native/release/tideminer self-test
./target/native/release/tideminer bench -t 1 --hashes 16384 --warmup 64
```

Run at least five paired/interleaved trials per candidate, after a stable warmup,
with enough hashes for 30–60 seconds per trial. Choose sample size from a pilot,
and keep the same total unique input set within each comparison. Save full JSON,
report median and spread (and paired differences), and examine per-worker times.
Longer thermal soaks and a 24-hour mining soak follow short kernel trials.

Current benchmark: zero header with nonce in bytes 76..80 little endian, fixed
total hashes split contiguously across workers. This makes digest XORs reducible
across worker counts for the same corpus. Startup and warmup are excluded; start
signal fanout and finishing the slowest worker are included. XOR is a diagnostic,
not a substitute for comparing all outputs in correctness tests. This small harness
does not yet pin cores, collect power, schedule fixed-duration runs or autotune.

Run configurations as separate processes because the existing C TLS allocator has
no scratch cleanup at thread exit. It is unsuitable for repeated in-process thread
sweeps until the owned-context API is implemented.

## Compare the right things

1. Scalar/reference C: correctness oracle, not the competitive speed baseline.
2. Optimized standalone C with the same corpus and compiler settings: estimates
   wrapper/scheduling cost separately from kernel differences.
3. Existing cpuminer-opt in benchmark mode, explicitly passing
   `--algo yespower --param-n 2048 --param-r 8`, the same thread count/affinity and
   build ISA. Confirm available duration options with that binary's `--help`.
4. tideminer generic and native builds; then each proposed optimization separately.
5. Local pool test: identical difficulty/job replay over equal durations, report
   valid attempts, accepted/rejected/stale work and job-switch latency.

cpuminer's synthetic header and timing may differ from this harness: label that
comparison as miner-level rather than identical-input kernel throughput. Build a
shared corpus scanner when isolating kernel speed. Do not compare debug Rust with
optimized C, or the GPU folder's reference CPU executable with optimized CPU mining.
The CPU harness prints `(ref)` regardless of which implementation was linked.

## Measurements and acceptance

Capture H/s, cycles/hash, instructions/hash, cache/TLB misses, branch misses,
allocation count/RSS, utilization and frequency. `perf stat`/`perf record` with the
profiling build are useful when permitted by local kernel settings. Hardware event
availability varies; record unsupported counters instead of fabricating zeroes.
Check generated code for scalar fallback, spills and the intended ISA.

For power, report the measurement domain: package energy is not whole-system wall
power. Compute hashes/joule over the same timed interval; when reporting incremental
power, state the idle subtraction. GPU comparisons include CPU feeding, transfers,
candidate verification and both devices' energy where measurable.

Tuning matrix: worker count; physical/SMT/hybrid layout; affinity; generic/native
compiler/ISA; context layout/pages; cancellation interval; one versus multiple
interleaved hashes. Change one variable at a time, then validate combinations.
Maximum logical CPU count and maximum SIMD width are hypotheses, not winners.

Before accepting an optimization: all-output parity against independent reference,
no gaps or duplicate timed nonces, no failed hash counted as success, consistent
performance outside noise, stable memory, unchanged share validity and acceptable
stale latency. The initial scaffold makes no cpuminer speedup claim.

## Results 2026-09-27: tideminer vs cpuminer-opt, accepted work

Machine: i9-13980HX laptop (8 P cores with SMT = CPUs 0-15, 16 E cores = CPUs 16-31),
Linux 6.14, power/thermal-limited under all-core load (~2.0-2.4 GHz). cpuminer-opt
v26.1 `b34565b` (AVX2 build, `-a yespower -N 2048 -R 8`); tideminer with crates.io
rust-yespower 0.3.0 (portable SSE2 build).

Method: both miners mine the same job on `examples/mockpool.rs`, which re-hashes every
share and counts valid unique shares in a 60 s window after 20 s warmup (difficulty
0.002, ~100 shares/s, ~1.4% 1-sigma per run). Implied H/s = valid shares / P(share) / 60.
Runs alternate miners with 20 s cool-downs; three rounds each.

| Threads | tideminer (3 runs) | cpuminer-opt (3 runs) | tideminer gain |
| ---: | --- | --- | ---: |
| 32 | 12174, 11993, 12000 (mean 12056) | 11683, 11783, 11818 (mean 11761) | +2.5% |
| 8 | 8137, 8172, 8199 (mean 8169) | 6291, 6326, 6385 (mean 6334) | +29.0% |

No invalid or duplicate shares from either miner. At 32 threads the gain is small but
held in every round (pooled 16,556 vs 16,152 valid shares, ~2.2 sigma). At 8 threads
it is placement: tideminer uses one thread on each of the 8 P cores; cpuminer pins
thread i to CPU i, i.e. both SMT siblings of 4 P cores. Same effect at 2 threads
(2726 vs 2005 H/s, single short run).

cpuminer's own "Total" line over-reports relative to shares it actually produced here
(12.8-13.2 kH/s printed versus ~11.8 kH/s delivered). Compare miners only through a
pool-side share count, never through their self-reported rates.

Kernel variants measured with a bare C loop over the same crate source, 32 pinned
threads, 45 s, three alternating rounds: `-O2` 13,564; `-O3 -fPIC` (Cargo's flags)
13,591; `-O3 -march=native` (AVX path) 13,347 H/s. Native ISA flags do not help.
Transparent huge pages with V aligned to a 2 MiB page: no change single-core
(P 1,466-1,561 vs 1,471-1,561; E 804-824 vs 807-824) or all-core (20.6-20.9k burst
both). The kernel is dependency-latency bound, as the research documents predicted.

Sustained layout study (bare C loop, 60 s each, one clean round): all 32 logical 14.5k;
P16+E8 13.2k; P16+E4 12.7k; P16 11.5k; P8 physical + E16 11.5k; P8 physical + E8
11.0k; P8 physical 8.7k; E16 7.2k. Hence the default of all CPUs and the `-t` priority
order P physical, P SMT, then E.

## Kernel switch 2026-09-27: Rust kernel vs C kernel in the miner engine

`tideminer bench --seconds 30 --warmup 5`, 32 pinned threads, alternating 30 s runs
(R C C R R C): Rust 12,373 / 12,577 / 11,820 (mean 12,257) vs C 11,614 / 11,764 /
12,513 (mean 11,964). Equal within this laptop's thermal noise, Rust not slower; it
became the default. Single-thread and per-core-type analysis: `crates/yespower/README.md`.
