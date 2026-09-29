# Tidecoin yespower: CPU implementation, GPU experiments, and optimization opportunities

> Follow-up measurements of the CPU kernel, including the S-box dependency
> diagnosis and the two-hash pair experiment, are in
> [YESPOWER_CPU_LATENCY.md](YESPOWER_CPU_LATENCY.md).
>
> The [mathematical backtrace](YESPOWER_BACKTRACE.md) adds an executable bit-mask
> dependency analysis, exact lazy-V and terminal-pruning prototypes, and
> qualifications to the latency note's stronger conclusions.

## Findings

The existing CPU implementation is already a carefully optimized implementation of
a deliberately difficult workload. The largest opportunity is in **PWXform/SMix
execution, cache placement, and CPU scheduling**, rather than SHA-256, Rust versus C,
or a routine switch to wider SIMD. There is room for research, but the experiments
below do not establish a new production speedup.

The most consequential findings are:

1. **An active Tidecoin hash requires 2,197,568 bytes of reusable scratch**, about
   2.096 MiB, before page rounding and thread overhead. Of this, 2 MiB is history
   and 96 KiB is a frequently read and modified S-box structure.
2. **Approximately 97% of measured P-core execution time is in the main SMix passes.**
   Initial SHA-256, PBKDF2 and final HMAC together consume about 0.36%. Accelerating
   those three stages infinitely would still improve this measured workload by
   less than 0.4%.
3. **One hash executes 43,726 PWX transformations**, including about 1.05 million
   unsigned 32×32→64-bit multiplications. It makes about 16.01 MiB of logical S-box
   reads and 5.34 MiB of S-box writes, in addition to history/state traffic. Logical
   accesses are not equivalent to DRAM transfers.
4. **The GPU conflict-detection idea transfers correctly to CPU code, but the tested
   translation loses performance.** It matched all 275 independent reference
   headers; its median throughput was about 11.3% below the native C baseline on a
   P-core and 7.5% below on an E-core. This is evidence against this implementation,
   not a proof that every such approach must lose.
5. **The conflicts are rare per round but common per complete hash.** An instrumented
   CPU translation observed conflicts in 0.2603% of PWX rounds, averaging 341.4
   conflicting rounds per hash. Ignoring them would not be an acceptable shortcut.
6. **The CUDA project contains substantial implementation work, but no verifiable
   GPU speedup result was found in the supplied files.** It has cooperative groups,
   conflict handling, register-state variants, split kernels and persistent work
   loops. Its benchmark validation and error handling are insufficient to certify
   either full correctness or useful mining throughput.
7. **Hopper resource problems are visible without running the GPU.** Compiling
   `yes2.cu` for `sm_90` produces a 168-register monolithic warp-8 kernel, versus an
   88-register split SMix kernel. The default persistent launch uses just one
   64-thread block per SM. The shared-S variant allocates 96 KiB for a hash but uses
   only eight active lanes in a one-warp block.
8. **Mathematical equivalence is the boundary for this miner.** Reordering independent
   operations, changing representation, exact instruction substitution, and safe
   precomputation can preserve Tidecoin. Reducing rounds, shrinking S-boxes, changing
   N/r, dropping writes, or replacing the function changes consensus. No new
   hash-preserving mathematical shortcut has been demonstrated here.

These findings combine local source analysis, reproducible CPU experiments, a
Hopper-target compilation, and primary documentation. The report distinguishes
observations, derived counts, and proposed experiments throughout.

## 1. Scope, source versions, and strength of evidence

The algorithm examined is **yespower 1.0, N=2048, r=8, no personalization**, applied
to the serialized 80-byte Tidecoin header. This is the contract exposed by the
local Rust crate. The Rust API calls C; rewriting the caller in Rust does not itself
change the cost of the kernel. The separate Tidecoin chain-height/AuxPoW discussion
in the miner plan remains relevant to selecting valid live jobs, but it does not
change the kernel analyzed here. [C1] [C2]

| Implementation | Revision or identification | Role in this report |
| --- | --- | --- |
| `rust-yespower` | `14ee90e814376a3ae9570ece4c639ccb2d8d8d8d57` | CPU kernel under test; optimized C and Rust API |
| `cpuminer-opt` | `b34565bfaca5da9ed894a326e3ba0dfe459550a4` | Mining-oriented CPU comparison by source |
| `yespower` | `b7447d385da2f2d272b8fd74f9e281709d5d9503` plus untracked CUDA variants | Independent scalar reference and GPU experiments |
| Openwall upstream | Current raw sources retrieved for comparison; SHA-256 checksums archived | Check which improvements are actually missing locally |

The CUDA variants `yes.cu`, `yes1.cu` and `yes2.cu` were untracked, so repository HEAD
does not identify their contents. The research bundle records file checksums and
line counts. It also records CPU source checksums, compiler flags, raw trial results,
and the complete Hopper compilation log. [R1] [R2] [R3]

CPU observations come from an Intel i9-13980HX, with a P-core pinned to Linux CPU 2
and an E-core pinned to CPU 16. GCC was 14.2.0 and Clang was 20.1.2. The host was not
an isolated, frequency-locked benchmark appliance: trial variation is material.
Hardware counters were inaccessible with `perf_event_paranoid=4`; therefore this
report does **not** claim measured cache-miss rates, IPC, DRAM bandwidth, or exact
stall attribution.

No Hopper model, remote endpoint, or prior benchmark log was available. The local
NVIDIA driver was unavailable. CUDA Toolkit 12.2 could nevertheless compile
`yes2.cu` for `sm_90`, allowing inspection of static register and stack requirements.
Compilation is not runtime validation, and no GPU H/s figure is claimed. [R3]

## 2. What yespower actually computes

### 2.1 The outer pipeline

The following describes the v1.0/no-personalization branch, not generic yescrypt
and not yespower 0.5:

```text
80-byte serialized header
       |
       v
SHA-256(header) -> 32-byte initial key
       |
       v
PBKDF2-HMAC-SHA256(key, empty salt, iterations=1, output=128 bytes)
       |                         |
       |                         +--> save first 32 bytes for final HMAC message
       v
Initialize 96 KiB S-box state with a small Salsa-based SMix1
       |
       v
Expand the 128-byte state to 1,024 bytes; build 2 MiB history V
using PWXform, Salsa20/2 and dependent reads of prior V entries
       |
       v
684 read/write SMix2 iterations over V
       |
       v
HMAC-SHA256(key = last 64 bytes of final B,
            message = saved 32-byte value)
       |
       v
32-byte proof-of-work digest
```

The PBKDF2 iteration count is **one**. It is not the usual password-stretching
scenario in which thousands of PBKDF2 iterations dominate runtime. The expensive
work is the memory-mixing construction between PBKDF2 and the final HMAC. [C2]

The saved 32-byte value is copied before SMix mutates B. It is not interchangeable
with the initial SHA-256 digest. Likewise, the last HMAC takes the final state's
last 64 bytes as its key; reversing the key/message roles changes every result.
These are useful intermediate checkpoints for future differential tests. [C2]

### 2.2 Two compiled algorithm versions are hidden in one C file

`yespower-opt.c` includes itself for a second preprocessing pass. Macros rename
`blockmix`, `smix1`, `smix2`, and related functions into their v1.0 forms, while
changing the PWX and Salsa definitions. Reading only the first definition of a
macro gives the wrong algorithm for Tidecoin. [C2]

In particular, the first pass contains six PWX rounds and Salsa20/8; the v1.0 pass
uses **three PWX rounds with writes and S-box rotation, and Salsa20/2**. The public
entry point selects the appropriate compiled implementation. This organization
creates some generic source complexity, but it does not mean both versions execute
for every Tidecoin hash. [C2] [C3]

### 2.3 S-box initialization

There are three 32 KiB tables: S0, S1 and S2. Their combined storage is initialized
using `smix1(B, 1, 768, S, XY, NULL)`. With no PWX context, BlockMix uses Salsa
instead of PWXform. The v1.0 recursive pass selects Salsa20/2 here as well. [C2]

The value 768 follows directly from `98,304 / 128`. Although the main N must be a
power of two, this internal initialization length is not 1,024. Replacing it with
a convenient power-of-two loop changes the function. The optimized SMix1 schedule
explicitly handles its final partial range.

### 2.4 History construction: SMix1

The state width is `128*r = 1,024` bytes, or sixteen 64-byte sub-blocks. Only the
first 128 bytes come from PBKDF2. Seven r=1 PWX BlockMix operations expand that seed
to the full state. [C2]

The main SMix1 builds N=2,048 history entries. It is more involved than “write an
array sequentially”: after the first entries, new state is combined with selected
older entries. The selected address depends on the evolving state. The optimized
implementation uses a doubling-window schedule with masks and offsets rather than
a general integer remainder at each access. [C2] [C3]

That matters for optimization. Sequential output stores can be prefetched or
streamed conventionally, but the old-state input needed for the next operation is
not known arbitrarily far in advance. The input state and mutable S tables evolve
together, so a cheap independent reconstruction of a missing V entry is not obvious.

### 2.5 Read/write mixing: SMix2

The v1.0 loop count is:

```text
ceil(N/3), rounded up to an even integer
= ceil(2048/3), rounded up to even
= 684
```

Each iteration derives an index from the low word of the final state sub-block,
masks it with `N-1`, combines the selected V entry with the current state, updates
that V entry, and runs PWX BlockMix. `blockmix_xor_save` fuses the XOR, history
writeback and new-state generation. The address for the next iteration comes from
the newly computed state. [C2]

The historical entries are therefore **mutable during this pass**. Treating V as
a read-only table, skipping writeback, or running independent iteration chunks from
the same initial state is incorrect.

## 3. Memory: footprint, locality, and traffic

### 3.1 Exact per-worker scratch

The optimized allocator computes the following sizes. The CPU driver observed the
same 2,197,568-byte `aligned_size` at runtime. [C2] [C4] [R1]

| Region | Formula | Bytes | Role |
| --- | --- | ---: | --- |
| B | `128*r` | 1,024 | Current serialized work state |
| V | `128*r*N` | 2,097,152 | 2,048 history entries |
| XY | `128*r + 64` | 1,088 | In-place working state and conversion temporary |
| S0+S1+S2 | `3 * 2^11 * 2 * 8` | 98,304 | Mutable PWX lookup tables |
| **Total** | | **2,197,568** | **2.095764 MiB** |

The total excludes the C/Rust thread stack, TLS metadata, allocator metadata,
header/digest storage and page rounding. On a 4 KiB page system the region spans
537 pages when page-aligned; the requested size is not an exact page multiple.
Calling it “2 MB per hash” is useful shorthand, but misses the most important hot
96 KiB and obscures cache-fit boundaries.

| Concurrent hashing contexts | Scratch only |
| ---: | ---: |
| 1 | 2.096 MiB |
| 8 | 16.766 MiB |
| 16 | 33.532 MiB |
| 24 | 50.298 MiB |
| 32 | 67.064 MiB |
| 64 | 134.129 MiB |

Allocation is reused across calls. Hash-dependent **contents must still be rebuilt**
for each nonce. The TLS wrapper does not register a destructor for its allocated
region, so repeatedly creating and destroying hashing threads retains allocations
until process exit. The C API already provides explicit init/free functions; a
Rust owned context should use them. This improves lifecycle behavior, not the
arithmetic cost of steady-state hashing. [C2] [C4]

### 3.2 Why cache geometry matters more than the headline RAM figure

The measured P-core has 48 KiB L1 data cache and 2 MiB private/shared-with-SMT L2.
The E-core has 32 KiB L1 data cache and shares a 4 MiB L2 with three other E-cores.
Both use the chip's 36 MiB shared L3. These values were read from Linux sysfs for
the tested CPUs. [R4]

The full 96 KiB S state does not fit in either L1. The 2 MiB V array alone fills
the P-core's nominal L2 capacity, leaving no capacity for S, code-related effects,
other data or its SMT sibling. Four E-core hashes require roughly 8.38 MiB of
scratch behind a shared 4 MiB L2. These are capacity calculations, not measured
miss rates, but they explain why thread count and shared-cache topology require
measurement rather than a default of one worker per logical CPU.

The hot footprint is also not a uniform random 2.096 MiB at every instant. S0/S1
are read repeatedly, S writes advance through small offsets, table roles rotate,
V writes are mostly sequential during filling, and V selections are dependent.
Cache replacement, access latency and execution overlap matter alongside capacity.

Upstream's historical benchmark explicitly reports a yespower 1.0 slowdown when
SMT increases contention on its tested Haswell system. Those old H/s figures are
not predictions for this CPU; the transferable lesson is to test topology and
thread count. [Openwall PERFORMANCE](https://raw.githubusercontent.com/openwall/yespower/main/PERFORMANCE)

### 3.3 Scratch size is not bandwidth demand

For the current Tidecoin parameters, the logical PWX S-box traffic alone is:

```text
43,726 PWX calls
  * 3 rounds
  * 4 groups
  * 2 loads of 16 bytes
  = 16,790,784 bytes read  (~16.01 MiB)

43,726 PWX calls
  * (4 + 2 + 2) stores of 16 bytes
  = 5,596,928 bytes written (~5.34 MiB)
```

This is about **21.35 MiB of logical S-table access per hash**, repeatedly touching
only 96 KiB. These values are derived from the algorithm, not a hardware counter.
They must not be multiplied by H/s and reported as measured DRAM bandwidth. Much
of that traffic may be served by caches and forwarding; misses can move larger
cache lines than the requested 16 bytes. [C2] [C3]

V adds its initial 2 MiB of writes, dependent reads during filling, and 684 selected
1 KiB read/write operations during SMix2, as well as accesses to the current state.
The CPU code already fuses several accesses to avoid a separate generic copy/XOR
pass. A full hardware traffic model must count cache-line transfers and writeback,
not just add source-level `memcpy` sizes.

### 3.4 Huge pages: useful experiment, not an existing optimization

The vendored platform allocator only attempts explicit 2 MiB huge pages when the
requested region is at least 12 MiB. Tidecoin's 2.096 MiB allocation never reaches
that threshold. An optimization claim that this path already provides huge pages
for Tidecoin would be false. [C4]

Rounding the entire region to explicit 2 MiB pages would consume 4 MiB per worker.
An alternative layout can allocate an aligned 2 MiB V region separately and put
the roughly 98 KiB remainder on ordinary pages. That could reduce translation
pressure without almost doubling committed scratch, but complicates allocation and
needs measurement. Transparent huge pages are policy-dependent and are not
guaranteed by the current allocator.

Huge pages reduce translation overhead; they do not make S lookups independent or
turn 96 KiB into L1-resident data. The current evidence does not establish that TLB
misses are the limiting factor.

## 4. PWXform: the arithmetic and the dependencies

### 4.1 The basic operation

A 64-byte PWX state is represented as four groups. Each group contains two 64-bit
values. The first value of the pair determines two aligned S-table addresses; both
values use the corresponding two entries. For each value x:

```text
lo = low_32_bits(x)
hi = high_32_bits(x)

y = ((uint64(hi) * uint64(lo)) + S0[index0]) XOR S1[index1]
     \________________ modulo 2^64 ________________/
```

The product is an unsigned **32×32→64-bit** product, not a general 64×64→128-bit
multiply. The low/high terms are extracted from each x, while the shared indices
come from the first x of the pair. Mask `0x7ff0` selects a 16-byte-aligned pair
within a 32 KiB table. [C2] [C3]

The optimized SSE2 code uses `PMULUDQ`-style operations to compute the two products
in a pair together, followed by 64-bit addition and XOR. A 128-bit register thus
maps naturally to the pair. There are four such X registers, creating some
instruction-level parallelism within each PWX transformation. [C2]

### 4.2 The state changes while the groups are processed

Round zero writes all four groups, alternating between S0 and S1 and advancing
the write offset. Rounds one and two write only the first two groups. After the
three rounds, S0/S1/S2 rotate roles and the write position wraps. [C2] [C3]

A later group can read a location just written by an earlier group. Consequently,
the four groups are not unconditionally independent. In v1.0, moving all loads to
the start of the round can change the answer unless those dependencies are handled.
Read-after-write hazards here are part of the algorithm, not incidental thread
synchronization around a shared cache.

The next PWX round derives addresses from the previous round's output. The next
64-byte sub-block also incorporates the current transformed X. Finally, the next
selected V entry depends on the completed BlockMix. These nested dependencies are
why increasing arithmetic throughput does not directly translate into proportionate
hash throughput.

### 4.3 Exact work counts

The main r=8 BlockMix executes sixteen PWX calls. Main SMix1 and SMix2 contribute
2,048 and 684 such BlockMix calls, respectively; the seven r=1 expansion calls add
fourteen PWX calls. [C2]

| Operation | Count per Tidecoin hash | Derivation |
| --- | ---: | --- |
| PWX transformations | 43,726 | `(2048+684)*16 + 7*2` |
| PWX rounds | 131,178 | `43,726*3`; also confirmed by instrumentation |
| Pair transformations | 524,712 | `43,726*3*4` |
| 32×32→64 multiplications | 1,049,424 | Two per pair |
| 64-bit additions inside PWX | 1,049,424 | One per multiplied value |
| 64-bit XORs inside PWX | 1,049,424 | One per multiplied value |
| 16-byte S loads | 1,049,424 | Two per pair |
| 16-byte S stores | 349,808 | Eight per PWX |
| Salsa20/2 calls | 4,275 | `768*2 + 2048 + 684 + 7` |

These are algorithmic operations, not CPU instruction counts. SIMD combines
multiple scalar operations, memory-source instructions fuse a load with an ALU
operation, and compiler scheduling changes the instruction mix. SHA, indexing,
state XOR/copy, address generation and loop bookkeeping are additional work.

### 4.4 Optimizations already present

The baseline already includes several techniques that a fresh implementation would
otherwise need to rediscover: SIMD-oriented word shuffling, 128-bit arithmetic,
merged XOR/writeback BlockMix variants, in-place SMix2 state, prefetching selected
history blocks, strength-reduced index calculations, and reusable allocation.
The source also contains old compiler-specific register constraints and a non-AVX
x86-64 inline-assembly PWX path. [C2]

The non-AVX assembly reads table memory through pointer registers. The nearby
compiler memory barriers preserve ordering around S writes. Removing those barriers
without giving the compiler an equally correct model of the assembly's memory
accesses can break hashing. “Fewer barriers” is only a valid optimization after an
aliasing and memory-order audit, not an independent switch to benchmark blindly.

The native AVX path still operates on **128-bit pairs**. `-march=native` does not
magically make the algorithm eight-way AVX2 or sixteen-way AVX-512. A different
layout and access strategy are needed to exploit wider vectors across pairs or
independent hashes.

## 5. CPU experiments and measured stage costs

### 5.1 Method

The research harness copies the CPU source into temporary directories, builds each
variant, and compares **every output byte** against a separately compiled scalar
`yespower-ref.c` implementation. The corpus contains the existing 19 reference
headers plus 256 deterministic varied headers, totaling 275. All tested variants
passed. Timed runs also use different nonces and verify a common digest XOR. [R1]

Each benchmark process owns one explicit C context and frees it at exit. It performs
64 warmup hashes before timing. The P-core series uses 2,048 timed hashes per trial;
the E-core series uses 1,024. Five trials per variant run in deterministic randomized
order, pinned to the chosen logical CPU. This is an initial comparative study,
not a long thermal/power characterization or proof of all-input equivalence.

Common C flags are `-O3 -std=gnu99 -funroll-loops -fomit-frame-pointer`; native
variants additionally use `-march=native`. Instrumented variants place clocks around
the six major stages. They are used to identify where time goes, not to rank tiny
throughput differences against uninstrumented binaries. [R1] [R2]

### 5.2 Where the time goes

The following are medians of each stage's percentage of full timed execution, not
ratios formed from unrelated median durations:

| Stage | P-core, GCC native | E-core, GCC native |
| --- | ---: | ---: |
| Initial SHA-256 | 0.045% | 0.031% |
| PBKDF2, c=1 | 0.219% | 0.150% |
| S initialization | 2.268% | 1.579% |
| Main SMix1 | 73.056% | 73.806% |
| Main SMix2 | 24.286% | 24.345% |
| Final HMAC | 0.103% | 0.070% |

Rounding, wrapper overhead and taking medians independently mean the columns need
not sum to exactly 100%. The generic SSE2 P-core build gave the same broad result:
72.89% main SMix1, 24.41% main SMix2 and 2.32% S initialization. [R1] [R2]

Typical native P-core instrumented median stage durations were approximately
0.35 µs for initial SHA, 1.72 µs for PBKDF2, 17.67 µs for S initialization,
569.27 µs for main SMix1, 189.20 µs for SMix2, and 0.79 µs for the final HMAC.
These timings include the effects of the instrumentation and host variability.

### 5.3 Compiler and kernel variations

P-core first series, H/s; ranges are the minimum and maximum of five short trials:

| Variant | Median H/s | Trial range | Interpretation |
| --- | ---: | ---: | --- |
| GCC generic SSE2 | 1,419.5 | 1,202.3–1,445.3 | Strong baseline; no need to assume native wins |
| GCC native | 1,377.7 | 1,154.9–1,409.0 | Reference for native modifications |
| Native, forced-register constraints removed | 1,315.7 | 1,177.7–1,360.9 | No demonstrated gain |
| SSE2, V prefetch disabled | 1,285.5 | 1,164.5–1,440.0 | No demonstrated gain; wide variation |
| Native, V prefetch disabled | 1,372.1 | 1,054.4–1,457.2 | Too close/noisy to establish a winner |
| Native, cached first SHA block | 1,408.9 | 1,270.9–1,451.0 | Apparent difference exceeds the stage's plausible upside; not evidence of a 2% SHA win |
| Clang native | 1,336.1 | 1,166.7–1,358.8 | No gain in this series |
| Native, GPU-inspired conflict fast path | 1,222.2 | 1,089.2–1,283.3 | About 11.3% lower median than native baseline |

The broad P-core spread prevents confident claims about small changes. In
particular, dividing two medians does not remove thermal/frequency/background-load
effects. Larger controlled paired trials are necessary before choosing a release
compiler or accepting a few-percent optimization. [R1]

The E-core series was steadier for the native baseline:

| Variant | Median H/s | Trial range |
| --- | ---: | ---: |
| GCC generic SSE2 | 803.2 | 755.0–821.2 |
| GCC native | 804.9 | 793.0–818.6 |
| Native, forced-register constraints removed | 800.9 | 767.7–804.6 |
| Clang native | 771.7 | 742.9–815.5 |
| Native, GPU-inspired conflict fast path | 744.9 | 720.8–758.4 |

Here the tested conflict fast path is about 7.5% below the native median, with
non-overlapping observed ranges. The exact percentage is still machine-specific.
The P/E difference also reinforces the need for per-core-class tuning. [R2]

### 5.4 Amdahl's law changes the research priority

For fraction f of the original runtime and component acceleration a, full speedup
is `1 / ((1-f) + f/a)`. Applying the measured P-core fractions:

| Hypothetical change | Best possible whole-hash gain from the measured share |
| --- | --- |
| Eliminate all initial SHA-256 cost | About 0.045% |
| Eliminate SHA, PBKDF2 and final HMAC entirely | About 0.37% |
| Eliminate all S initialization | About 2.3% |
| Halve time in both main SMix passes | About 1.95× total speed, if achievable |
| Reduce time in both main SMix passes by 10% | About 10.8% total throughput gain |

Caching the first SHA block saves only part of the first line, not all of it. The
previous miner plan's concrete midstate suggestion remains correct, but it should
be a small housekeeping optimization after the main-loop work. It is not a route
to a large speedup on this measured CPU.

### 5.5 Comparison with cpuminer and current upstream

cpuminer already computes the SHA context for the first 64 bytes before scanning
nonces. It also uses its own SHA/HMAC implementation and a different SIMD abstraction,
with the forced-register assembly macros commented out in its PWX code. Transplanting
those changes one by one does not establish equivalence to cpuminer's complete
performance profile. This report does not claim tideminer beats cpuminer. [C5] [C6]

Comparing the actual upstream files, rather than just the changelog, shows that
the local SHA code **already contains** the majority-function optimization mentioned
upstream. The notable missing kernel feature is AVX512VL packed rotates for Salsa
on compatible CPUs. This i9 does not expose AVX-512, so that path was not benchmarked.
The upstream kernel remains very close to the local one otherwise. [R5]

The upstream performance document's broad historical statement about AVX2-and-higher
being unused must be read alongside its newer AVX512VL rotate change. Faster
128-bit rotates are different from widening PWX to 512 bits.
[Openwall CHANGES](https://raw.githubusercontent.com/openwall/yespower/main/CHANGES)

## 6. What the CUDA experiments have achieved

### 6.1 An implementation inventory

| Family | Actual idea in the code | Assessment |
| --- | --- | --- |
| Baseline, one thread/hash | Scalar CUDA hash with per-hash V/S and private temporary arrays | Reference starting point; poor hardware mapping expected |
| Warp-8 sequential PWX | Eight lanes share one hash; two lanes per 128-bit pair; groups still processed sequentially | Coalesces state work, but leaves many lanes waiting within PWX |
| Warp-8 full PWX | Detect intra-round S hazards, execute a parallel fast path, fall back for conflicts | Real algorithm-preserving idea; CPU translation validated, GPU execution not validated here |
| Register/in-place state | Keep sixteen 64-bit state pieces per lane and reduce global state reloads | Potential reduction in traffic; creates register/stack pressure |
| Vector versus scalar V transfers | Pair 64-bit lane values into 128-bit accesses, with shuffle redistribution | Fewer/wider memory operations versus more shuffle/coordination work |
| Split PBKDF2/SMix | Run initial SHA/PBKDF2 in a separate kernel | Demonstrably lowers compiled SMix register count |
| Persistent repeated-hash loop | Repeat the same inputs inside one kernel | Removes launch overhead, but does not represent fresh mining work |
| Persistent “queue” | Assign changing nonces to fixed groups with strided loops | Reuses scratch and bounds allocation; not a dynamic queue |
| Chunked “queue” | Statically assign chunks to blocks and groups | Changes traversal/scheduling granularity; not work stealing |
| Shared S | One hash/block with all 96 KiB S in dynamic shared memory | Trades locality against extremely low thread occupancy |
| Collision statistics | Device atomics count conflicting rounds | Diagnostic only; contended atomics alter performance |
| Warp-16 in `yes2.cu` | Split each 64-bit value across two lanes; sequential group processing | More lanes do not imply more useful arithmetic parallelism |

The older named baseline is 3,766 lines. `yes.cu` has 4,080, `yes1.cu` 4,058 and
`yes2.cu` 4,653. The important `yes.cu`→`yes1.cu` change makes lane-zero Salsa the
default while leaving the shuffle-heavy parallel version behind
`YESPOWER_PARALLEL_SALSA`. `yes2.cu` adds warp-16 support and further queue variants.
Those changes show real exploration, but file sequence alone does not establish
that later versions are faster. [C7] [C8] [R3]

### 6.2 The collision fast path is substantive

The full warp-8 path extracts the four pairs' S addresses before computation and
checks whether a later group would consume a write from an earlier group. On no
conflict, the groups can use the round's original table values concurrently. On a
conflict, the code follows the original group order. [C7]

For the first round there are six relevant address-equality checks, and for each
later round there are five. Under a simple uniform-address approximation, these
predict roughly 0.293% and 0.244% conflict probabilities per round, respectively.
They are a probabilistic model, not a guarantee of uniformity or independence.

The CPU translation measured 131,178 rounds and 341.4 conflicting rounds per hash
on average, giving **0.2603%** overall. That is close to the elementary model and
provides evidence that the idea is implemented as intended on the CPU. It does not
verify CUDA subgroup masks, memory ordering, or device-specific behavior. [R1]

Rarity per round can be misleading: hundreds of fallbacks occur in an average hash.
Simply executing the no-conflict formula unconditionally would normally corrupt
the result. A single wrong S value propagates through later lookups and the final
digest.

### 6.3 Why the CPU translation did not win

The translation eagerly calculates eight indices, checks hazards, branches, stages
loads/results, and writes them back. That removes a class of software-visible
dependencies, but adds instructions and live values to every round. It passes
275 scalar-reference headers, so its negative performance result is meaningful for
this concrete implementation. [R1] [R2]

Modern out-of-order CPUs already have mechanisms to execute independent loads and
arithmetic ahead and to forward values from earlier stores. Software speculation
must save more than it costs on top of those mechanisms. Additional checks and
register pressure are plausible explanations for the loss; exact stall attribution
requires counters and assembly analysis beyond the measurements available here.
[Intel optimization manual index](https://www.intel.com/content/www/us/en/developer/articles/technical/intel64-and-ia32-architectures-optimization.html)

This leaves room for a different implementation: explicit register forwarding for
known store slots, a branchless correction path, or a wider-vector layout could
have different costs. They should remain experiments until they beat the existing
code while retaining exact semantics.

### 6.4 What is not established by the GPU harness

Several defects or limitations undermine performance interpretation:

- Ordinary runs copy and display only the first digest. Sweep mode often prints
  no digest at all. There is no all-output differential correctness gate.
- Queue kernels write a digest only when `job == 0`. They do not return other
  candidates or perform target filtering. The final HMAC for other jobs may also
  be susceptible to dead-code elimination; the generated code needs checking
  before claiming every counted job performs the full same workload.
- Non-queue iterations repeat the same headers. They execute hashes, but do not
  provide that many distinct proof attempts.
- The queue is implemented with fixed arithmetic strides, not an atomic job
  allocator. Describing the chunk mode as eliminating atomic queue contention
  would misidentify the current code.
- Many allocation, copy, launch, synchronization and timing calls have unchecked
  return values. A failed run can still proceed toward a throughput printout.
- Timing is primarily CUDA-event time around device execution. It excludes relevant
  allocation/transfers/verification and has no job-change cancellation metric.
- The CPU comparison harness always prints a “ref” label even if linked against
  the optimized implementation; the linked source and build flags must be recorded.
- In `yes2.cu`, the warp-16 choice is handled after the generic sweep branch. The
  sweep path does not call the dedicated warp-16 runner, so that combination needs
  correction before interpreting its output as a warp-16 sweep.

These are source observations, not observed device failures. [C7] [C9]

There is also a concrete large-work-count arithmetic issue. Queue loops use 32-bit
`job += total_workers` or `base += gridDim.x * chunk_jobs`. The host permits totals
up to `0xffffffff`. Near that limit, addition can wrap and resume at a small job
number, creating repeated work or a nonterminating loop. Use 64-bit scheduling
counters or an overflow-safe terminal condition while serializing only the 32-bit
nonce. [C7]

GPU synchronization still needs an independent audit. Warp masks, partial groups,
collision branches and global/shared scratch handoffs must follow CUDA's memory
model. In particular, shuffle intrinsics and historical lockstep assumptions must
not be used as a substitute for required memory ordering. NVIDIA documents
`__syncwarp` as the memory-ordering primitive for participating lanes.
[CUDA language extensions](https://docs.nvidia.com/cuda/cuda-programming-guide/05-appendices/cpp-language-extensions.html)

No archived Hopper/Ada H/s measurements were present in the inspected source folder.
The defensible achievement is a collection of substantive kernels and useful ideas,
not a demonstrated speedup magnitude.

## 7. Why these kernels are a poor default fit for Hopper

### 7.1 Architecture features relevant to this workload

NVIDIA documents 64 resident warps and 65,536 32-bit registers per H100 SM, with
up to 228 KiB shared memory per SM and 227 KiB per block. These are capacity limits,
not a promise that a kernel reaches them.
[Hopper tuning guide](https://docs.nvidia.com/cuda/hopper-tuning-guide/index.html)

The key mismatch is that yespower wants rapid dependent integer computation and
low-latency mutable lookups per independent hash. Hopper's Tensor Cores and high
floating-point throughput are not direct execution units for its 32×32→64 product,
64-bit modular add, XOR, and data-dependent S addresses. The correct comparison is
useful hash work per SM under the actual dependency graph, not advertised FP8/FP16
TFLOPS. NVIDIA's architecture description separates these facilities.
[Hopper architecture overview](https://developer.nvidia.com/blog/nvidia-hopper-architecture-in-depth/)

### 7.2 Compiled register and stack requirements

This command successfully generated a Hopper cubin without a GPU:

```sh
nvcc -ccbin /usr/bin/g++-12 -O3 -arch=sm_90 -lineinfo -Xptxas=-v \
  -cubin "$YESPOWER_CUDA_SOURCE" \
  -o /tmp/yespower-yes2-sm90.cubin
```

The following are compiler-reported properties of that exact build. [R3]

| Kernel | Registers/thread | Stack frame/thread | Reported spill loads/stores |
| --- | ---: | ---: | --- |
| Scalar baseline | 128 | 2,848 B | 0 / 0 |
| Warp-8 sequential | 168 | 640 B | 0 / 0 |
| Warp-8 full | 168 | 640 B | 0 / 0 |
| Warp-8 full register/scalar-V | 168 | 640 B | 0 / 0 |
| Split PBKDF2 | 30 | 736 B | 0 / 0 |
| Split full SMix | 88 | 544 B | 0 / 0 |
| Full persistent | 136 | 672 B | 0 / 0 |
| Full queue | 134 | 672 B | 0 / 0 |
| Full chunk queue | 147 | 672 B | 0 / 0 |
| Full queue/register-state | 130 | 672 B | 0 / 0 |
| Warp-16 queue | 141 | 672 B | 0 / 0 |
| Shared-S | 125 | 672 B | 0 / 0 |

“Zero spills” does **not** mean no local-memory use: the nonzero stack frames are
reported separately. Local arrays and device-call frames can exist without ptxas
classifying their storage as spilled registers. Runtime local traffic remains to
be measured. Likewise, a function called “register” in the source does not prove
all its dynamically indexed state stays in registers.

A register-only upper-bound calculation illustrates the pressure. At 168 registers
per thread, 65,536 registers can accommodate at most roughly 390 threads before
allocation rounding and block constraints. With 64-thread blocks, six blocks/384
threads/12 warps are plausible as a register-limited ceiling: **18.75% of 64 warps**.
With 256-thread blocks, only one such block fits, or 12.5% occupancy. These are
derived limits, not measured occupancy.

At 88 registers, the same arithmetic allows roughly 744 threads before rounding.
A 64-thread-block layout can potentially fit eleven blocks/22 warps, about 34.4%.
Other resource limits and compiler allocation granularity still apply. This explains
why splitting an inexpensive arithmetic stage can help the **GPU**: it changes the
resource requirements of every SMix thread, not merely the time spent in SHA.

### 7.3 The default persistent launch is even more restrictive

The queue runner defaults to `blocks = multiProcessorCount`, with 64 threads/block.
That launches only one two-warp block per SM on average. Even if registers would
allow more blocks, they do not exist in the grid. The launch therefore provides
only **2/64 = 3.125% nominal warp occupancy** on a uniformly distributed full chip.
Increasing the number of jobs only extends the loops inside the same sparse grid.
It does not create more concurrent warps. [C7]

This is a much stronger source-level explanation than “Hopper is bad at yespower.”
The code's default limits its ability to hide dependent-load latency. The
occupancy-printing helper reports a resource-based maximum, which must not be
confused with the occupancy of the actually launched grid.

The nonpersistent defaults can also undersupply a large GPU. At 1,024 hashes,
eight lanes/hash and 64 threads/block, there are only 128 blocks. That was a very
different launch on a small Ada GPU than on a large Hopper device. Benchmark batch
sizes must be expressed relative to device SM count and the desired resident
blocks, not inherited unchanged from the RTX 4060 example. [C7] [C8]

### 7.4 S-box locality and occupancy pull in opposite directions

One warp-8 block with 64 threads carries eight hashes:

```text
S footprint = 8 * 96 KiB = 768 KiB
V footprint = 8 * 2 MiB = 16 MiB
```

The S footprint alone exceeds an SM's combined 256 KiB L1/shared-memory capacity.
Using a 132-SM full-device example, eight hashes/SM need 99 MiB of S state and
2,112 MiB of V. H100's documented 50 MB chip-wide L2 cannot hold all those S tables,
let alone V. This is a capacity estimate; actual replacement and hit rates require
profiling. [C7]
[NVIDIA memory-system description](https://docs.nvidia.com/cuda/hopper-tuning-guide/index.html#memory-system)

Reducing concurrent hashes helps cache residence but leaves fewer independent
dependency chains to keep GPU schedulers occupied. Increasing concurrent hashes
improves nominal occupancy but can destroy S locality. The target is the best
balance of useful latency hiding and cache residence, not maximum occupancy or
maximum VRAM allocation in isolation.

### 7.5 The shared-S experiment has an especially sharp tradeoff

`warp8fullshared` launches a 32-thread block for one hash, and immediately returns
lanes 8–31. It requests 98,304 bytes of dynamic shared memory. On Hopper, two such
blocks can fit within the shared-memory capacity, but three cannot. [C7] [R3]

Thus the design can have only two resident warps per SM due to S storage, with
just sixteen useful lanes across those warps. It improves where S is stored while
offering very little latency hiding or lane utilization. The limit follows from
the layout even before considering instruction counts.

A better shared-memory experiment should investigate two hashes in a deliberately
designed block, a larger cooperative group per hash, or a smaller hot-state cache
with correct backing-store handling. None removes the dependency problem for free.
Using 16 or 32 lanes merely to perform the same serialized work can add shuffles
without increasing useful work per SM.

### 7.6 Why the warp-16 extension is not automatically an improvement

The new path splits each 64-bit x across two 32-bit lanes. In PWX, the even lane
reconstructs the value, performs the multiply/add/XOR, then sends the high half
back. The code still loops over the four groups sequentially. Consequently,
doubling lanes/hash halves independent hashes at a fixed thread count without
necessarily doubling arithmetic throughput. The compiled queue still uses 141
registers/thread. [C7] [R3]

Its Salsa mapping also uses many shuffles and conditional updates. The earlier
`yes1.cu` switch back to lane-zero Salsa is a useful warning: fewer serial-looking
source operations can be faster than a wider mapping that communicates constantly.
Only execution measurements can choose between them on Hopper.

### 7.7 Hopper features worth testing, and features with weak fit

| Feature or approach | Relevance to this code |
| --- | --- |
| More resident blocks, tuned to register usage | High priority; directly addresses the sparse default queue launch |
| Split SHA/PBKDF2 from SMix | High priority; static register reduction is already observed |
| S locality experiments and cache-policy tuning | High priority; target the repeatedly accessed 96 KiB state |
| Warp/group remapping | Worth testing with actual instruction and latency data; not just lane count |
| Async copies/TMA for a selected 1 KiB V entry | Possible, but address becomes available late; barriers/setup may exceed useful overlap |
| Distributed shared memory | Research option for a redesigned layout; adds cross-SM coordination and remote latency |
| Tensor Cores | No direct mapping for the exact PWX recurrence; packing products does not solve dependent lookups/XOR |
| DPX | Its min/max/add patterns do not implement PWX's operation sequence |
| Lossless hardware memory compression | Hash state has no demonstrated cheaply compressible pattern; do not assume a benefit |
| Multi-GPU execution | Parallelize independent nonce ranges; do not split one tiny dependent hash across GPUs |

NVIDIA describes TMA as asynchronous bulk movement between global/shared memory,
and DPX as specialized dynamic-programming operations. Their capabilities support
these distinctions; whether a particular transfer can be profitably overlapped is
an algorithm-specific experiment.
[Hopper tuning features](https://docs.nvidia.com/cuda/hopper-tuning-guide/index.html#tensor-memory-accelerator)

## 8. CPU optimization opportunities, ranked by evidence

### 8.1 Highest priority: the core loop and its working set

**Modernize and measure PWX instruction scheduling.** Keep a trusted 128-bit baseline.
Compare native C intrinsics, precise inline assembly, and the existing assembly
path on named CPUs. Look for unnecessary XMM→GPR extraction, redundant shifts,
reloads, spills, address-generation pressure, and avoidable serialization between
independent pairs. This is where a small reduction repeated half a million times
per hash can matter.

The important constraint is preserving S access order where addresses conflict.
The existing CPU handles that through sequential semantics and hardware execution;
the GPU-inspired speculative implementation adds software checks. A more promising
new design might forward the few known recently stored values explicitly, or make
the compiler's aliasing model more precise without adding a branch per round.
Those are specific hypotheses, not established improvements.

**Tune the memory layout and worker placement together.** Test one physical thread
per P-core, SMT pairs, E-core-cluster density, and combined layouts with steady
clock/temperature observation. Keep S and scratch local to the worker's NUMA node.
The current P/E measurements are single-worker tests; they do not determine the
optimal whole-chip thread count.

**Measure scratch-layout variants.** Candidates include a separately aligned V,
controlled cache-line offsets between S tables, and a huge-page V with ordinary-page
S/XY. The tables' current separation is a multiple of 32 KiB; changing relative
offsets can alter cache-set/alias interactions. Any padded layout must preserve the
exact initialized S contents and rotation semantics. The cost of repacking S each
hash counts against any benefit.

### 8.2 Compiler specialization and unrolling: tested, no clear win

A follow-up P-core series fixed the version, N, r and personalization inside the
experimental entry point, and separately tested removal of the crate's explicit
`-funroll-loops` flag. These remain Tidecoin-only experiments; the fixed-parameter
entry point is not a generic yespower API. All variants passed the same 275 headers.
[R6]

| Follow-up variant | Median H/s | Trial range |
| --- | ---: | ---: |
| Generic SSE2 baseline | 1,359.1 | 1,271.1–1,440.8 |
| Native baseline | 1,310.4 | 993.6–1,434.1 |
| Native, fixed Tidecoin parameters | 1,319.6 | 1,198.5–1,440.1 |
| SSE2, fixed Tidecoin parameters | 1,312.0 | 1,243.0–1,435.7 |
| Native, explicit unrolling disabled | 1,324.5 | 1,024.0–1,430.6 |
| SSE2, explicit unrolling disabled | 1,331.5 | 1,288.4–1,460.5 |

The substantial spread overwhelms these small median differences. This does not
justify a production flag change. It does suggest that simple constant substitution
or toggling one unrolling flag is not an obvious large win on this host. Further
tests should isolate a hot function's unrolling and code footprint, rather than
assuming more whole-file unrolling is better.

PGO and compiler-version comparisons are reasonable follow-ups once a controlled
benchmark is available. Cross-language Rust LTO should not be assumed to optimize
a separately compiled C archive automatically. Inspect the final binary and exact
C flags rather than judging the optimization level from Cargo's profile name.

### 8.3 SIMD widening and independent-hash interleaving

AVX2/AVX-512 can perform more products per instruction, but PWX's two independent
table addresses per pair still require irregular loads. A gather-based wider design
may replace efficient 16-byte pair loads with more expensive gathers and require
lane rearrangement. It also has to preserve the table-write dependencies.

There are two distinct experiments:

1. Pack more pairs from **one hash** into a vector. This saves arithmetic instructions
   but needs dependency handling and efficient indexed loads/stores.
2. Pack or interleave **independent hashes**. This avoids cross-hash dependencies
   and may hide latency, but doubles or quadruples scratch pressure and live state.

The second has cleaner correctness semantics but can exceed private-cache capacity
very quickly. On the measured P-core, two contexts require about 4.19 MiB against a
2 MiB L2. Benchmark interleaving against both one hash/core and SMT; do not assume
that manually adding independent work is better than the CPU's existing execution
machinery.

On AVX512VL-capable CPUs, porting upstream's 128-bit rotate improvement is a low-risk
starting experiment. It benefits Salsa instructions without requiring a new PWX
layout. Its impact is not the same as the 2.3% S-initialization fraction: Salsa also
appears at the end of main BlockMix calls. The total benefit must be measured.

### 8.4 Smaller, valid improvements

An explicit prepared-header API can cache the first 64-byte SHA block, as cpuminer
already does. A production implementation should prepare once per template rather
than `memcmp` the prefix on every nonce, as the defensive experimental helper does.
The measured upside remains tiny. [C5] [R1]

An internal-state API could avoid serialization/shuffling of B between SMix phases
and fuse the final state extraction. The existing code only performs those conversions
at phase boundaries; this is not a per-PWX cost, so the ceiling is limited. Keep the
reference byte representation at the API boundary and prove internal equivalence.

Allocator ownership, persistent workers, batched counters and a low-overhead FFI
boundary are worthwhile miner engineering. They prevent avoidable losses and resource
leaks. They should not be marketed as a large improvement to the underlying math
when the baseline already reuses scratch and spends hundreds of microseconds in
each hash.

### 8.5 GPU ideas that transfer, and those that do not

| GPU idea | CPU counterpart | Current conclusion |
| --- | --- | --- |
| Conflict detection plus parallel PWX | Software dependency checks and fast/fallback path | Correct tested prototype, slower on both tested core classes |
| Keep state in registers | Retain hot X words, fuse state loads/stores | Already substantially present; inspect remaining spills before redesign |
| Persistent worker scratch | Long-lived OS threads with owned C contexts | Strong engineering choice; baseline already reuses TLS storage |
| Chunked nonce scheduling | Coarse CPU work leases | Useful for miner scheduling, outside the hash math |
| Split SHA/PBKDF2 to reduce registers | Separate CPU phases or calls | GPU benefit does not imply CPU benefit; CPU measured stage is tiny and compiler model differs |
| Wider/coalesced memory transfers | SIMD pair loads, contiguous state operations | Already core features of the optimized CPU code |
| Shared-memory S placement | Cache-local S/NUMA/layout policy | Transfer the locality objective, not CUDA shared-memory mechanics |
| Warp shuffles and barriers | SIMD permutations and dependency control | Not a direct port; many GPU coordination instructions disappear on a CPU |

## 9. Can the mathematics be changed?

### 9.1 Three different kinds of change

**Equivalent implementation:** produce exactly the same 32 bytes for every valid
80-byte header. This includes different instruction sequences, representations,
safe scheduling, equivalent arithmetic identities and precomputing invariant input
work. This is the appropriate optimization space for the miner.

**An exact algorithmic shortcut:** also preserve every digest, but reduce the
necessary work by exploiting mathematical structure, a time/memory tradeoff, or an
unexpected weakness. Such research is legitimate and potentially valuable. None
is demonstrated by the code or measurements in this report, and an implementation
speed test cannot prove that no such shortcut exists.

**A different PoW function:** reduce N/r, alter the S width or number of rounds,
drop writes, replace HMAC, change Salsa, or otherwise compute different digests.
That can make a function faster, but existing Tidecoin validators will reject its
proofs. It requires a protocol decision and separate security/economic analysis;
it is not a faster implementation of existing Tidecoin mining.

### 9.2 Arithmetic identities worth considering

| Candidate | Exactness and likely usefulness |
| --- | --- |
| Replace shifts/XOR with a native rotate | Exact with the same word width; useful when the ISA has a cheaper rotate |
| Reexpress SHA Ch/Maj boolean functions | Exact if proven; the relevant majority optimization already exists locally |
| Unsigned 32×32→64 instead of a general multiply | Exact and already used by the optimized CPU path |
| Decompose a product into 16-bit partial products | Can be exact with correct carries; usually adds work on CPUs with native widening multiplication |
| Reorder products/loads across independent groups | Exact only if the mutable-table dependencies are handled |
| Precompute constants or first SHA block | Exact when the prepared state matches the current header prefix |
| Replace scalar/array layout with SIMD layout | Exact if all shuffles, endian conversions and addresses are mapped correctly |
| Use approximate floating-point products | Not generally exact for arbitrary 64-bit results; unsuitable as a replacement |
| Move XOR through modular addition | Not a valid general identity; carry propagation prevents it |

For example, `(a*b + c) XOR d` is not generally equal to `a*b + (c XOR d)`.
Unsigned wraparound is part of the operation. A rewrite that changes integer width,
uses signed overflow, or drops a carry may pass selected inputs but fail ordinary
headers. The nonlinear mix of multiply/add/XOR and data-dependent addressing is
what makes broad symbolic simplification difficult.

### 9.3 Precompute more across nonces?

The first 64 header bytes usually remain fixed while scanning a nonce range, so
their SHA compression state is reusable. The nonce changes the remaining SHA input,
which changes PBKDF2 output, S initialization, all subsequent S evolution and V.
There is no observed reusable S/V state across distinct nonces. [C2]

Storing all possible first-hash outputs for a job would itself require doing that
work, would expire with the job, and would not eliminate each nonce's SMix. Memoizing
complete headers only helps repeated verification or accidental duplicate work;
it does not increase distinct mining attempts per second.

The CUDA `sha256_header80_nonce` helper patches the nonce without copying the whole
header, but still recomputes the first compression block. Its name should not be
mistaken for an existing SHA-midstate cache. [C7]

### 9.4 Reduce memory without changing the hash?

Time/memory tradeoffs are worth studying carefully. The original scrypt literature
provides formal models and examples of the relationship between sequential work
and memory; those theorems must not be imported wholesale as proofs about this
modified yespower construction.
[Percival, Stronger Key Derivation via Sequential Memory-Hard Functions](https://www.tarsnap.com/scrypt/scrypt.pdf)

An exact reduced-memory implementation could retain some V entries and recompute
others. The difficulty is reconstructing the correct historical entry together
with the mutable S state that produced it. SMix2 also updates V in place. Storing
checkpoints or logging writes consumes space and work of its own. A design needs a
precise replay model before it can be benchmarked meaningfully.

It would be wrong to call V information-theoretically incompressible: it is
deterministically generated from a short input. The engineering question is whether
there is a **cheap useful representation** smaller than V that avoids expensive
recomputation. No such representation has been shown here. Ordinary compression
of hash-like state and deduplicating entries are weak starting bets without evidence
of exploitable structure.

This question may have a different answer on a GPU than on a CPU. Reducing memory
per concurrent GPU hash could improve cache residence or allow a better mapping,
even if it adds some operations. The cost model must include both time and memory
and be tested on the actual device; “less memory” alone is not a speed result.

### 9.5 Early rejection, partial hashes, and choosing special nonces

The target comparison comes after the final digest. The available implementation
does not expose a proven inexpensive intermediate predicate that rules out losing
nonces. High-word-first comparison saves work **after** hashing, but cannot skip
SMix. Any proposal to prune based on intermediate state needs a proof that it never
discards a valid solution, or an explicitly analyzed statistical tradeoff.

Likewise, finding inputs with conveniently repeated table indices does not by
itself help mining: finding those inputs has a cost, and they still need valid final
digests. An apparent shortcut that simply biases sampling must be judged by accepted
work per unit time, not by a higher rate for a specially selected synthetic case.

### 9.6 A useful mathematical research agenda

The most practical sequence is to prove small transformations around the actual
hot recurrence. First specify one PWX round with explicit old/new table versions.
Then prove a no-hazard condition, a correct forwarding correction, and the exact
rotation/write-pointer update. A reduced-state model can exhaustively enumerate
address collisions and compare schedules; full-width randomized testing follows.

This is more actionable than assuming the entire hash can be algebraically collapsed.
The GPU fast path already demonstrates one such local scheduling equivalence. Its
CPU translation establishes that correctness is achievable; a cheaper realization
remains a performance question.

## 10. Recommended next experiments

### 10.1 CPU sequence

| Order | Experiment | Success condition |
| --- | --- | --- |
| 1 | Establish controlled baseline on representative P-core, E-core, AMD and server CPUs | Stable paired measurements; recorded frequency, topology, power domain and compiler |
| 2 | Inspect hot PWX assembly with usable counters | Identify dominant instruction/dependency/cache costs instead of inferring them from H/s alone |
| 3 | Compare exact PWX scheduling/forwarding variants | Full correctness plus repeatable whole-hash improvement beyond noise |
| 4 | Test S/V layout, huge-page V and topology/SMT density | Higher total H/s or H/J with all allocation/repacking costs included |
| 5 | Test upstream rotates and carefully scoped ISA variants | Named-CPU gain without reducing the portable baseline's coverage |
| 6 | Test two independent interleaved hashes per core | Net gain after extra scratch, cache pressure and cancellation cost |
| 7 | PGO and function-specific unrolling/specialization | Consistent gains on both benchmark and real changing-job workload |
| 8 | Prepared SHA and internal-format cleanup | Small measurable reduction without complicating correctness/lifecycle |

The experiment harness is deliberately outside the production backend. None of
the tested experimental changes has been installed into `rust-yespower` or selected
as the miner's default. A candidate should beat both the corresponding unmodified
C build and a properly configured cpuminer baseline before a superiority claim.

### 10.2 Hopper sequence when hardware is available

1. Record exact GPU model, SM count, MIG mode, driver/toolkit, clocks, power limit,
   ECC state and thermal conditions. Compile for its actual architecture and retain
   both compiler resource reports and the final binary identity.
2. Add checked CUDA calls and a mode that returns every digest. Validate at least
   a large deterministic varied corpus, nonce boundaries, partial group sizes,
   repeated launches and collision-heavy PWX cases against scalar C. Run memory,
   race and synchronization checking as appropriate.
3. Correct the queue overflow, warp-16 sweep dispatch, and work-count accounting.
   Use unique changing nonces and distinguish attempted, completed, verified and
   target-meeting results. Queue mode needs output filtering or full output for
   validation, rather than only job zero.
4. Sweep resident blocks/SM and block size explicitly. Compare the 168-register
   monolithic kernel with the 88-register split SMix kernel, including the extra
   launches and transfers. Inspect achieved occupancy and eligible warps, not just
   occupancy limits printed before launch.
5. Profile global/local/shared traffic, L1/L2 hit rates, memory sectors, dependency
   stalls, shuffle/barrier instructions, register allocation and active lanes.
   Determine whether S locality, insufficient concurrency, integer dependencies,
   or coordination dominates each variant.
6. Test S storage and layouts at equal useful work. Try one/two/four/eight concurrent
   hashes per SM with appropriate group widths and enough grid work. Do not tune
   solely to 90% of free VRAM.
7. Add bounded job epochs/cancellation and CPU verification of returned candidates.
   Judge kernel changes by end-to-end accepted work and H/J, including the CPU cost
   of feeding and checking the device.

### 10.3 Correctness gates for aggressive changes

The current 275-header suite is a useful experiment gate, not a proof. Before a
production kernel change, extend it with thousands of varied full headers,
same-prefix nonce sweeps, all nonce byte-carry boundaries, changing prepared
prefixes, concurrency/lifecycle tests, and long differential runs.

For PWX specifically, compare intermediate S0/S1/S2 contents, X state and write
pointer at each round. Construct cases where each possible earlier-write/later-read
collision occurs, including both write positions in round zero. For new SIMD
representations, exercise all lane permutations and wrap points.

Any mismatch disqualifies the candidate regardless of speed. Tiny target probabilities
make “the pool accepted a few shares” a particularly weak substitute for full-output
testing. Passing a local pool soak is an additional integration gate, not the kernel
oracle.

## 11. Conclusions and limits

The report identifies a concrete direction for serious CPU work: reduce the cost of
the million multiply/add/XOR operations and the dependent mutable-table accesses
inside main SMix, while keeping the working set well placed. The existing C baseline
already addresses many obvious optimizations. The straightforward GPU-inspired
speculation port is correct on the tested corpus but slower, and compiler/native/
prefetch/specialization tests do not establish a new robust speedup.

The GPU code has useful engineering and a real dependency-preserving idea, but its
validation and launch defaults are inadequate for a Hopper performance conclusion.
The successfully compiled 168→88 register reduction and the one-block-per-SM queue
default are concrete targets for the next GPU investigation. They are more actionable
than trying to apply Tensor Core throughput to a workload that does not directly
map to those units.

No mathematical shortcut preserving Tidecoin's digest has been discovered. Equivalent
local transformations, exact forwarding, memory/time tradeoffs and better machine
mapping remain legitimate research avenues. No GPU throughput, whole-chip optimum,
energy-efficiency gain, cache-miss rate, or cpuminer speedup is asserted by the
measurements available here.

## Sources and reproducibility

The code references below identify experiment inputs by filename, not bundled
source files or publicly verified deployments. Revision and file-checksum records
accompany the measurements. External references were checked during the investigation on
September 13–14, 2026; older design discussions are historical context.

### Source implementations

- **C1.** Rust yespower API (`rust-yespower/src/lib.rs`): fixed Tidecoin contract and FFI.
- **C2.** Optimized C yespower (`rust-yespower/depends/yespower/yespower-opt.c:410`): PWX macros,
  recursive preprocessing, BlockMix, SMix and allocation size calculation.
- **C3.** Scalar reference (`yespower/opt/yespower-ref.c:183`): readable algorithm and
  independent digest oracle.
- **C4.** CPU allocator (`rust-yespower/depends/yespower/yespower-platform.c`): mapping, alignment, huge-page
  threshold and cleanup.
- **C5.** cpuminer yespower gate (`cpuminer-opt/algo/yespower/yespower-gate.c:57`): prepared SHA state and nonce scan.
- **C6.** cpuminer yespower kernel (`cpuminer-opt/algo/yespower/yespower-opt.c:570`): SIMD abstraction, PWX scheduling and
  SHA/HMAC integration.
- **C7.** Latest local CUDA variant (`yespower/yes2.cu:1357`): full PWX, warp-16, queues,
  launch sizing, timing and output handling.
- **C8.** CUDA experiment README (`yespower/README_yespowertide_cuda.md`): original mode inventory and baseline intent;
  its single-thread performance paragraph does not describe all later variants.
- **C9.** CPU comparison harness (`yespower/yespower_cpu_test.c`): input generation, threading and printed label.

### Measurements and experiment source

- **R1.** [P-core raw results][R1]: 11 variants, five trials each, 2,048 timed hashes
  per trial; stage data and collision counts. [Experiment runner](../research/yespower/run.py),
  [C driver](../research/yespower/driver.c), and
  [CPU conflict-path prototype](../research/yespower/parallel_round.h).
- **R2.** [E-core raw results][R2]: six variants, five trials each, 1,024 timed hashes.
- **R3.** [Hopper compiler resource log][R3] and
  [GPU source/build provenance](../research/yespower/results/gpu-provenance.json).
- **R4.** [Local CPU/cache topology][R4]: sysfs values for allowed CPUs.
- **R5.** [Retrieved upstream source identities][R5]: hashes of current Openwall
  `yespower-opt.c` and `sha256.c` used in the source comparison.
- **R6.** [Specialization/unrolling follow-up][R6]: six variants, five trials each.
- [Reproduction instructions](../research/yespower/README.md) explain the commands,
  limits, temporary builds and separation from production code.

### Primary external references

1. Openwall/Solar Designer, [yespower README](https://raw.githubusercontent.com/openwall/yespower/main/README),
   current repository version: algorithm purpose, versions and reference/optimized roles.
2. Openwall, [yespower PERFORMANCE](https://raw.githubusercontent.com/openwall/yespower/main/PERFORMANCE),
   historical measurements and compiler/SMT discussion; not a benchmark of this machine.
3. Openwall, [yespower CHANGES](https://raw.githubusercontent.com/openwall/yespower/main/CHANGES),
   current file: AVX512VL rotates and SHA majority optimization history.
4. NVIDIA, [Hopper Tuning Guide](https://docs.nvidia.com/cuda/hopper-tuning-guide/index.html),
   sections on occupancy, memory, TMA and DPX.
5. Michael Andersch et al., NVIDIA, [NVIDIA Hopper Architecture In-Depth](https://developer.nvidia.com/blog/nvidia-hopper-architecture-in-depth/),
   March 22, 2022: architecture and execution facilities; marketing performance
   figures are not used as yespower predictions.
6. NVIDIA, [CUDA C/C++ Language Extensions](https://docs.nvidia.com/cuda/cuda-programming-guide/05-appendices/cpp-language-extensions.html),
   warp synchronization and memory-ordering requirements.
7. Intel, [Intel 64 and IA-32 Architectures Optimization Reference Manuals](https://www.intel.com/content/www/us/en/developer/articles/technical/intel64-and-ia32-architectures-optimization.html),
   official index: execution, memory access and store-forwarding background.
8. Colin Percival, [Stronger Key Derivation via Sequential Memory-Hard Functions](https://www.tarsnap.com/scrypt/scrypt.pdf),
   BSDCan 2009: formal background; not a proof covering this exact yespower variant.
9. Agnieszka Bielec and Solar Designer, [PHC: yescrypt on GPU discussion](https://www.openwall.com/lists/john-dev/2015/07/25/7),
   July 25, 2015: historical exploration of table placement. Its older 8 KiB
   yescrypt S-box discussion must not be confused with Tidecoin's 96 KiB state.

[R1]: ../research/yespower/results/cpu.json
[R2]: ../research/yespower/results/cpu-ecore.json
[R3]: ../research/yespower/results/cuda-sm90-build.log
[R4]: ../research/yespower/results/topology.json
[R5]: ../research/yespower/results/upstream-provenance.json
[R6]: ../research/yespower/results/cpu-specialization.json
