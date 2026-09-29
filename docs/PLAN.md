# Tidecoin miner implementation and optimization plan

Prepared 2026-09-13 from the local codebases. Objective: maximize **valid accepted
Tidecoin work per second**, with an additional efficiency mode optimizing H/J.
Peak synthetic H/s is a diagnostic, not the product's success metric.

## 1. Scope and decisions

- Tidecoin yespower only: v1.0, N=2048, r=8, no personalization; serialized pure
  80-byte headers. Changing these changes PoW and is not an optimization.
- Stratum V1 pool mining first, with cpuminer-compatible `-o`, `-u`, `-t` ergonomics.
  TCP first, certificate-validated TLS before a general release. No universal coin
  registry, algorithm negotiation, fee mining, wallet custody or pool server.
- Linux x86-64 is the first performance target. Preserve a portable CPU baseline;
  add AArch64/NEON and other OS packaging after correctness and CPU performance gates.
  Port and tuning notes: [AUTOTUNE_AND_MOBILE.md](AUTOTUNE_AND_MOBILE.md). A
  browser/WASM distribution target reuses the same core and is proposed in
  [WEB_MINER.md](WEB_MINER.md); it does not change the native-first priority.
- Use Rust for networking, job ownership, scheduling, configuration and metrics.
  Keep the existing C yespower implementation as the initial fast kernel and oracle.
  A pure Rust kernel is an experiment with explicit parity and speed gates.
- No fixed multiple of cpuminer speed is promised. Establish a fair baseline,
  profile it, and retain improvements that beat measurement noise on relevant CPUs.
- GPU work proceeds as a separate backend experiment after CPU correctness tooling.
  It must earn inclusion through valid unique hashes, stale latency and H/J.

The local Tidecoin node has height-dependent scrypt/AuxPoW rules. In the inspected
checkout, mainnet's AuxPoW start is `AUXPOW_DISABLED`, testnet's is 1000 and regtest's
is 0. This project targets the yespower regime. Do not use a default post-AuxPoW
regtest job as a yespower oracle. Before live acceptance testing, confirm the target
pool's deployed chain/rules; local source is not proof of deployed network state.
No automatic scrypt fallback is planned.

## 2. Evidence from the existing implementations

| Source inspected | Finding | Design consequence |
| --- | --- | --- |
| `rust-yespower/src/lib.rs`, `build.rs` | Small safe API over vendored optimized C, one fixed Tidecoin hash and one known vector | Start here; expand independent correctness coverage before changing the kernel |
| `rust-yespower/depends/yespower/yespower-opt.c` | C TLS scratch reused; no TLS destructor; full `SHA256_Buf(src, 80)` per hash | Persistent workers now; explicit owned contexts and prepared-header hashing later |
| Same file, `yespower()` | V=2,097,152 B; S=98,304 B; B=1,024 B; XY=1,088 B | 2,197,568 B (~2.096 MiB) per active hash, before allocator/stack overhead |
| `yespower-platform.c` | Huge-page attempt starts at 12 MiB | This Tidecoin allocation never enters that path; huge pages require a deliberate experiment |
| cpuminer `algo/yespower/yespower-gate.c` | First 64 header bytes prehashed once per scan; nonce loop checks restart | Prepared SHA state is a concrete candidate; cancellation belongs in or near the scan loop |
| Same file | `register_yespower_algo` defaults to r=32, `opt_target_factor=65536` | cpuminer baseline must explicitly set N=2048, r=8; never benchmark its generic defaults |
| cpuminer `cpu-miner.c`, `util.c` | Job difficulty snapshots `next_diff`, then divides by target factor; header/submit use intermediate endian conversions | Differential test complete notify-to-header-to-submit fixtures; simple string reversal guesses are unsafe |
| GPU `yespowertide_cuda_baseline.cu` | Warp-8, register, shared-S, split, persistent and queue variants already exist | Reuse experiments; do not start another naive one-thread port |
| GPU harness and CPU harness | Print the first hash only; some modes repeat the same work and CUDA timing omits host-side costs | Need all-output parity, unique nonce counting and end-to-end timing |

The GPU README's single-thread performance paragraph describes only its simplest
baseline; the source and mode list are more advanced. Untracked `yes.cu`, `yes1.cu`
and `yes2.cu` also exist: inventory their differences and preserve provenance before
choosing a kernel. None has been validated by this project yet.

Upstream explicitly designs yespower for CPU cache behavior and GPU unfriendliness:
[Openwall design overview](https://www.openwall.com/yespower/). That makes GPU speedup
a hypothesis to test, rather than an architectural assumption. Also compare the
vendored code with [upstream changes](https://github.com/openwall/yespower/blob/main/CHANGES),
including AVX512VL rotates on capable CPUs; inspect the actual code and benchmark
before transplanting anything.

Source revisions inspected (working-tree contents take precedence):

| Repository | HEAD |
| --- | --- |
| `rust-yespower` | `14ee90e814376a3ae9570ece4c639ccb2d8d8d8d57` |
| `cpuminer-opt` | `b34565bfaca5da9ed894a326e3ba0dfe459550a4` |
| `yespower` | `b7447d385da2f2d272b8fd74f9e281709d5d9503` |
| `tidecoin` | `039dba3f2e46ed9f006d91284897e9f9b50d74d1` |

## 3. Architecture

Start as a single Cargo package with a library and CLI. Split crates only when a
stable backend boundary, fuzz target or independent package makes that useful.

```text
config / CLI -> pool supervisor -> Stratum session -> work coordinator
                                      ^                    |
                                      |              immutable Job epoch
                                 share queue               |
                                      ^              fixed CPU workers
                                      |                    |
                                      +--- candidates <- yespower contexts

metrics observer <- batched worker counters + session/job/share events
```

Networking is separate from hashing. One socket owner handles request IDs, framing,
deadlines and writes. A dedicated blocking I/O thread is sufficient for one pool;
choose an async runtime if TLS/failover management warrants it, with CPU workers
remaining dedicated OS threads. Do not run yespower on networking executors.

Planned module responsibilities:

| Module | Responsibility |
| --- | --- |
| `config` | CLI/TOML merge, pool list, password environment/file input, CPU policy |
| `stratum` | Typed V1 messages, session state, reconnect/backoff, response routing |
| `work` | Coinbase/merkle/header construction, exact targets, job snapshots |
| `scheduler` | Epochs, unique extranonce ownership, nonce ranges, cancellation |
| `cpu` | Long-lived threads, topology/affinity, backend contexts and scanning |
| `pow` | Fixed Tidecoin contract, baseline/prepared hash APIs, self-tests |
| `stats` | H/s windows, accepted/rejected/stale shares, effective work, errors |
| `benchmark` | Deterministic workloads, machine-readable results, variant comparison |
| optional `gpu` | Bounded work batches, device health, candidate verification |

Immutable jobs carry session generation, job ID as an opaque string, extranonce1,
extranonce2 size, coinbase parts, branches, version/time/bits, share target, network
target and clean-job generation. Workers own their header bytes and hash scratch.
Share candidates retain the exact job/session/target snapshot used for hashing.
Never read a mutable global target while checking a completed hash.

## 4. Stratum compatibility and correctness

Implement the session as explicit states: Disconnected -> Connecting -> Subscribing
-> Authorizing -> Ready -> Backoff. Subscriptions and jobs may arrive before auth
completes; buffer valid state but do not mine until authorized. Reconnect creates a
new session generation, invalidates prior extranonce/work and clears request state.

Required behavior:

1. `mining.subscribe` and `mining.authorize`, distinct request IDs, strict response
   validation, bounded pending requests and response deadlines.
2. `mining.set_difficulty`: positive finite input; snapshot on the next notify to
   match the inspected cpuminer. Difficulty changes do not retroactively change old
   candidate targets. Support decimal/exponent parsing without doing per-hash floats.
3. `mining.notify`: validate field lengths/types and bounded coinbase/branches;
   construct `coinbase1 || extranonce1 || extranonce2 || coinbase2`, SHA256d it,
   then SHA256d each merkle concatenation. Keep raw digest bytes distinct from display
   hex and Stratum word-swapped fields.
4. Difficulty convention (cpuminer-opt
   `diff_to_hash` with target factor 65536): `high = (1/(D/65536)) * 2^96` as u128,
   target = `high` in the top 16 bytes followed by 16 bytes of `0xff`. Difficulty 1 is
   `2^240 + 2^128 - 1`; the earlier `0x1d00ffff * 65536` formula in this plan was
   wrong. Implemented and golden-tested in `src/target.rs`.
5. Decode compact network nBits separately; compare the raw 32-byte yespower digest
   as a little-endian 256-bit integer, accepting equality with target. High-word
   fast rejection is allowed only with full comparison for ties. Test target-1,
   target and target+1, signed/overflow compact targets, and reversed-byte traps.
6. `mining.submit`: `[worker, job_id, extranonce2_hex, ntime_hex, nonce_hex]` from
   the candidate's original template. Derive byte order through golden fixtures
   traced through cpuminer's `std_build_block_header`, `scanhash_yespower` and
   `std_le_build_stratum_request`; its in-memory work words are not wire bytes.
7. `clean_jobs=true` invalidates all earlier work promptly. `false` keeps older
   jobs submit-eligible while scheduling the newest template; bound retained jobs
   by age/count. Opaque IDs can repeat across sessions and must never be parsed as
   integers for identity. Track explicit generations independently of strings.
8. `mining.set_extranonce` and negotiated extranonce subscription: validate size,
   reset the allocator and invalidate old templates. Handle version/ping requests.
   Unknown notifications may be ignored; unknown requests get a bounded error.
9. Bounded newline frames, partial/multiple reads, idle timeouts, clean EOF handling,
   exponential reconnect with jitter and a cap, user-configured pool failover.
   Never blindly replay a timed-out submission: its acceptance is unknown.
10. TLS verifies hostname/certificates and never silently downgrades. Logs redact
    credentials and bound/escape server text. Server reconnect requests are hints
    handled under the configured endpoint policy, not arbitrary credential forwarding.

Golden tests must prove every intermediate byte string and final submit payload.
Build a mock pool capable of validating low-difficulty shares independently, injecting
clean jobs, difficulty changes, authorization errors, disconnects, duplicate IDs,
malformed lines, partial writes, delayed responses and stale share races. Then use a
yespower-capable local node/pool to validate full blocks before a real-pool soak.

## 5. Effective CPU use

First production worker design:

- Create a fixed thread set once. Affinitize before allocating and first-touching
  scratch. Keep one exclusive yespower context per worker; add Rust RAII around a
  small C create/hash/free API in rust-yespower rather than duplicating C ABI layouts.
- Worker hot loop changes nonce bytes, hashes, compares target, checks cancellation
  and batches counters. No heap allocation, logging, locks, JSON or job cloning per
  nonce. Cache-align per-worker published counters to prevent false sharing.
- Use a cheap atomic epoch check per hash initially; compare cancellation batches
  of 1/4/16 only after measuring overhead versus stale latency. Cancellation delay
  target: p99 below 50 ms under normal CPU workloads, or explicitly document the
  slowest single-hash bound on low-end hardware. No cancellation result becomes a
  completed valid hash.
- Allocate non-overlapping nonce leases using wide counters with exclusive `2^32`
  endpoints. Start with ranges; introduce adaptive chunk leases for heterogeneous
  workers if they improve utilization. No atomic read-modify-write per nonce.
- On nonce exhaustion allocate a new extranonce2 and rebuild coinbase/merkle/header.
  Extranonce2 must be unique within a session/template; detect exhaustion without
  wrapping. Do not roll time or version unless the pool's rules allow it.
- Use a bounded candidate queue. Never block hashing indefinitely on a broken
  connection. Prioritize potential block solutions, expose overflow, and invalidate
  obsolete candidates using their session/clean generation. Test the queue under
  an artificially easy target.

Topology policy must observe the allowed CPU set, physical core/SMT siblings,
shared L2/L3 groups, NUMA nodes, and hybrid core classes. Expose `--threads` and
`--cpu-list` overrides and report the actual assignment. Available logical CPUs are
only a starting point. For this i9-13980HX, test P-core physical threads, P+SMT,
E-core subsets, P+E physical cores, and all allowed logical CPUs. Determine IDs from
sysfs/topology; do not assume CPU-number ranges or uniform core speeds.

Autotuning compares a bounded set of layouts under warm load, keeps a conservative
fallback, and caches by CPU/topology, cpuset, backend build and power policy. Offer
throughput and efficiency profiles; avoid retuning during live job churn. Sample
temperature/frequency and energy where supported, with no privileged system changes
required. On NUMA systems measure local versus remote placement and inter-socket
scaling before selecting memory policy. Signal sources and the calibration and
control-loop design: [AUTOTUNE_AND_MOBILE.md](AUTOTUNE_AND_MOBILE.md).

## 6. yespower optimization research, in order

Every candidate must pass independent reference vectors and randomized differential
testing. Keep generic and optimized paths callable on the same input. A Rust rewrite
alone cannot establish a performance improvement. Shortcut status and the remaining
research programme are tracked in [YESPOWER_BACKTRACE.md](YESPOWER_BACKTRACE.md),
[YESPOWER_SHORTCUT_ANALYSIS.md](YESPOWER_SHORTCUT_ANALYSIS.md) and
[CRYPTANALYSIS_AGENDA.md](CRYPTANALYSIS_AGENDA.md).

| Priority | Experiment | Hypothesis and measurement |
| --- | --- | --- |
| P0 | Correct N/r and equivalent compiler flags for cpuminer/C/Rust wrapper | Remove invalid baselines; record compiler/ISA for C as well as Rust |
| P0 | Persistent workers and owned scratch | Avoid lifecycle leaks and allocator churn; measure steady RSS and restart soak |
| P1 | Topology, SMT, NUMA, hybrid scheduling | Cache contention and power limits may dominate; compare aggregate H/s and H/J |
| P1 | GCC versus Clang, native C codegen, LTO/PGO variants | Inspect assembly and counter changes; Rust LTO does not automatically optimize the C archive |
| P1 | Prepared first-64-byte SHA256 state + SHA extensions | Match cpuminer's existing midstate idea; benchmark the entire hash, not only SHA |
| P2 | Tidecoin-specialized version/N/r/input-length entry point | Remove generic branches/constants where compiler cannot; verify whether it already specializes |
| P2 | PWXform/Salsa instruction scheduling, register pressure, ISA dispatch | Profile cycles, dependency stalls, spills; compare SSE2/AVX encodings and capable-CPU rotates |
| P2 | Scratch alignment/layout, TLB and huge pages | Compare base pages/THP/explicit huge pages with graceful fallback; 2.10 MiB may round to 4 MiB |
| P3 | Software prefetch and interleaving 2+ independent hashes | May hide latency but doubles scratch/cache pressure; use counters and per-core throughput |
| P3 | Pure Rust SIMD kernel | Adopt only if bit-identical and faster/maintainable; preserve C oracle and fallback |

Profile the actual mix of initial SHA/PBKDF2, S initialization, SMix1, read/write
SMix2, PWXform, Salsa and final HMAC. Expect data-dependent memory and dependency
chains to matter; do not assume wider SIMD or higher DRAM bandwidth solves them.
Measure each stage's fraction before prioritizing it (Amdahl's law). Since the
nonce changes the initial SHA and dependent S/V state, do not reuse S-boxes or
scratch contents as precomputed work across nonces without an equivalence proof.
Scratch allocation can be reused; hash-dependent initialization cannot be skipped.

The proposed fast API should eventually be `context.prepare(header_prefix)` plus
`context.scan(nonce_range, target, cancellation)` or a small Rust loop over a prepared
hash API. Benchmark both: batching across FFI may save little for a millisecond-scale
kernel and must not conceal cancellation/errors. Audit return conventions:
rust-yespower's C API returns **0 on success**, whereas cpuminer's mining hash path
uses a truthy success convention. Do not interchange those wrappers.

Portable releases need runtime CPU-feature dispatch, including OS vector-state
support. Native builds are explicitly local. Rust `target-cpu=native` does not by
itself select matching C preprocessor paths; set and record C flags separately.
Do not require AVX-512 globally or assume it wins under frequency throttling.

## 7. GPU track

1. Inventory baseline and untracked variants by hash/revision, work distribution,
   memory layout, barriers/shuffles, PWX duplicate-index handling and timed region.
2. Extend the harness to export every digest; compare deterministic random 80-byte
   headers and boundary nonces with independent scalar C and Rust baseline. Include
   collision-heavy PWX cases and repeated runs to reveal races. Use CUDA memory/race
   tools on supported hardware. A matching first digest is insufficient.
3. Measure unique changing nonces, warmup separately, and complete transfer+kernel+
   result-filtering latency. `warp8fullpersist` repeating the same hash is a kernel
   experiment, not mining throughput. Queue modes must prove no duplicates/gaps.
4. Profile occupancy, registers/spills, warp stalls, actual memory transactions,
   S-box cache behavior, shared-memory capacity/bank conflicts, and coalescing of V.
   Compare 8/16/32 cooperative lanes, register/global layouts, shared-S and persistent
   queues. Respect serial read/write dependencies when parallelizing PWXform.
5. Try job-resident headers/midstates, device nonce allocation and on-device target
   filtering; return bounded candidates only. CPU-rehash every reported candidate.
   Retain the job epoch, cap batch latency, discard obsolete results and recover from
   device errors without corrupting CPU mining.
6. Ship only after it improves total system accepted throughput or H/J on a named
   device after CPU feeding/verification cost and job-change losses. CUDA remains
   optional; CPU builds must not require its toolchain. Consider OpenCL only after
   the algorithmic GPU approach is validated.

## 8. Milestones and acceptance gates

| Milestone | Deliverable | Completion gate |
| --- | --- | --- |
| M0 — foundation (this change) | Cargo project, fixed PoW API/self-test, 19 scalar reference vectors/generator, parallel benchmark, TCP Stratum probe, plan | Build/tests/lint, known/reference vectors, local handshake, real release benchmark; explicitly no share mining |
| M1 — correctness harness | Reference corpus; typed jobs; exact targets; notify-to-submit fixtures; independent validating mock pool | Header/merkle/target/submit parity including endian and boundary cases |
| M2 — CPU mining MVP | Owned contexts, long-lived workers, work epochs/extranonces, submit/response tracking, reconnect, Ctrl-C | Accepted shares against mock and yespower local pool; clean job/reconnect/exhaustion tests; no duplicate work |
| M3 — production CPU miner | TLS, failover, affinity/topology, metrics, bounded autotuning, configuration, releases | 24-hour soak with stable memory; cancellation latency measured; accepted/rejected/stale accounting reconciled |
| M4 — CPU acceleration | Fair cpuminer baseline, profiles, prepared SHA, winning kernel/layout variants, runtime dispatch | Full parity; repeated end-to-end gain beyond noise; no acceptance/stability regression |
| M5 — GPU decision | All-output validated kernels, unique-work benchmark, end-to-end CPU+GPU trial | Named-hardware throughput/H/J benefit and bounded stale latency; otherwise keep experimental |

M1 precedes M2; M2 provides the acceptance harness needed to judge M4's final results.
Profiling/standalone kernel experiments can start during M1, but cannot replace its
correctness work. M5 reuses the same corpus and job model. No calendar estimate is
meaningful before baseline hardware measurements and pool fixtures are established.

For M3+ performance changes: aim for no more than 2% regression on supported baseline
CPU configurations; investigate anything outside run-to-run noise. Promote tuning
only when repeated paired runs show a reliable gain. These are proposed engineering
gates, not measured achievements. Report absolute H/s, relative change, H/J where
available, CPU/compiler settings, and uncertainty.

## 9. Immediate next implementation slice

1. Expand the independent C reference corpus (random headers, nonce boundaries,
   same-prefix batches), preserving generator and source provenance.
2. Add owned context lifecycle to rust-yespower with creation/drop and thread tests.
3. Capture a sanitized Tidecoin Stratum transcript and derive golden bytes against
   cpuminer and the pool's own share validator.
4. Implement `work::{Job, Header, Target, ExtranonceAllocator}` and mock validation.
5. Connect those pieces to the worker loop and expose `tideminer mine` only once it
   can submit valid shares and handle clean jobs and reconnects.

No endpoint or credentials were supplied for this initialization. Public-pool
acceptance, deployed chain compatibility and GPU advantage remain unverified.
