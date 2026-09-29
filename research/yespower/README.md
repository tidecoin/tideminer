# yespower CPU/GPU research bundle

Read [the detailed report](../../docs/YESPOWER_RESEARCH.md), the
[CPU latency follow-up](../../docs/YESPOWER_CPU_LATENCY.md), the
[backtrace study](../../docs/YESPOWER_BACKTRACE.md) and the
[shortcut analysis](../../docs/YESPOWER_SHORTCUT_ANALYSIS.md). This directory
contains isolated experiments, not a replacement production backend. The originals
in rust-yespower, cpuminer-opt and the CUDA experiment checkout are read only inputs.

`run.py` copies C sources into a temporary directory, modifies only those copies,
compiles with GCC/Clang, validates all 275 headers against independent scalar C,
and runs pinned randomized-order trials. It removes its temporary builds on exit.
The fixed-parameter variant intentionally supports only Tidecoin's parameter set;
it must not be exported as a general yespower API.

Source paths are explicit inputs. Set `YESPOWER_SOURCE` to the optimized C
source directory (containing `yespower-opt.c`, headers and `sha256.c`),
`YESPOWER_REFERENCE` to the scalar directory (containing `yespower-ref.c`,
headers and `sha256.c`), and `YESPOWER_CUDA_SOURCE` to the CUDA experiment file.
These external sources are not bundled; use the recorded revisions and checksums
when reproducing the archived results. Requirements:
Python 3, GCC, Clang, taskset, Linux clock/affinity support and those local checkouts.
No extra Python packages are required. Native builds execute only on a compatible
host. Choose a CPU ID in your allowed affinity set; IDs 2 and 16 refer to the
documented i9-13980HX topology, not a universal convention.

## Mathematical backtrace

Read [the derivation and results](../../docs/YESPOWER_BACKTRACE.md).
`backtrace/run.py` builds an independent C++ SMix evaluator, checks normal and
decomposed PWX execution against scalar C, and replays a backward-pruned graph
including lookup-address dependencies. It requires G++ as well as GCC.
`backtrace/bench.py` validates and times exact terminal-pruning and lazy-V C
prototypes. Neither is wired into the miner.

```sh
python3 research/yespower/backtrace/run.py --reference "$YESPOWER_REFERENCE" --varied 8
python3 research/yespower/backtrace/bench.py --source "$YESPOWER_SOURCE" --reference "$YESPOWER_REFERENCE" --cpu 2 --hashes 2048 --trials 5 \
  --varied 4096 --output research/yespower/results/backtrace-cpu.json
python3 research/yespower/backtrace/bench.py --source "$YESPOWER_SOURCE" --reference "$YESPOWER_REFERENCE" --cpu 16 --hashes 2048 --trials 5 \
  --output research/yespower/results/backtrace-ecore.json
```

Graph nodes and dead-store percentages are not CPU instruction counts or memory
bandwidth measurements. The graph uses substantial temporary diagnostic memory.

Recorded first P-core series:

```sh
python3 research/yespower/run.py --source "$YESPOWER_SOURCE" --reference "$YESPOWER_REFERENCE" --cpu 2 --hashes 2048 --trials 5 \
  --variants gcc_sse2,gcc_native,gcc_native_noforce,gcc_sse2_noprefetch,gcc_native_noprefetch,gcc_native_midstate,clang_native,gcc_native_parallel,gcc_sse2_stages,gcc_native_stages,gcc_native_parallel_stats \
  --output /tmp/yespower-cpu-rerun.json
```

E-core series:

```sh
python3 research/yespower/run.py --source "$YESPOWER_SOURCE" --reference "$YESPOWER_REFERENCE" --cpu 16 --hashes 1024 --trials 5 \
  --variants gcc_sse2,gcc_native,gcc_native_parallel,gcc_native_noforce,clang_native,gcc_native_stages \
  --output /tmp/yespower-ecore-rerun.json
```

Specialization/unrolling follow-up:

```sh
python3 research/yespower/run.py --source "$YESPOWER_SOURCE" --reference "$YESPOWER_REFERENCE" --cpu 2 --hashes 2048 --trials 5 \
  --variants gcc_sse2,gcc_native,gcc_native_fixed,gcc_sse2_fixed,gcc_native_nounroll,gcc_sse2_nounroll \
  --output /tmp/yespower-specialization-rerun.json
```

## Latency diagnosis and pair kernel (follow-up)

`diagnose.py` builds digest-breaking variants that isolate the S-box dependency
chain from S cache traffic and from V traffic. The variants are diagnostic only;
they must never be used for mining. It prints median H/s per variant:

```sh
python3 research/yespower/diagnose.py --source "$YESPOWER_SOURCE" --cpu 2  --hashes 2048 --trials 3
python3 research/yespower/diagnose.py --source "$YESPOWER_SOURCE" --cpu 16 --hashes 2048 --trials 3
```

`pair/run.py` builds the research-only two-hash interleaved kernel, validates
both the single and pair paths against the scalar reference on the 275-header
corpus, then times `pair` against `single` in alternating order:

```sh
python3 research/yespower/pair/run.py --source "$YESPOWER_SOURCE" --reference "$YESPOWER_REFERENCE" --cpu 2  --hashes 4096
python3 research/yespower/pair/run.py --source "$YESPOWER_SOURCE" --reference "$YESPOWER_REFERENCE" --cpu 16 --hashes 2048
```

`pair/pair_kernel.h` is appended to a temporary copy of `yespower-opt.c`; it is
not a generic API and is not wired into the miner. It exists to measure how much
of the single-thread stall two independent chains can hide. See the follow-up
document for the whole-chip results and why pairing is not a mining win.

## Job reuse and shortcut probes

`reuse.c` measures how much of a header is reusable across nonces and how far the
SHA-256 digest and PBKDF2 seed move. `diffusion.c` measures one-round sensitivity
of the output and of the next-round address bits. Both print JSON and take no
arguments:

```sh
SRC="$YESPOWER_SOURCE"
gcc -O2 -std=gnu99 -I"$SRC" research/yespower/reuse.c "$SRC/sha256.c" -o /tmp/reuse
gcc -O2 -std=gnu99 research/yespower/diffusion.c -o /tmp/diffusion
/tmp/reuse
/tmp/diffusion
```

`diagnose.py` also accepts `h0`, which hoists a PWX round's loads above its stores
with no hazard check. `h0` is digest-breaking on conflict rounds and exists only
to bound the software-reordering headroom.

```sh
python3 research/yespower/diagnose.py --source "$YESPOWER_SOURCE" --cpu 2 --hashes 2048 --trials 5 \
  --variants baseline,h0 --output research/yespower/results/form2-hoist.json
```

`bias.c` with `bias_test.py` probes the digest for nonce/time structure: one
process per CPU sweeps a field and histograms the most significant digest byte.
See [the block-timing report](../../docs/TIDECOIN_BLOCK_TIMING.md) for the
chain-level analysis it supports.

```sh
python3 research/yespower/bias_test.py --source "$YESPOWER_SOURCE" --output research/yespower/results/bias.json
```


The source checksums, compilers, flags and raw observations are in `results/*.json`.
Archived source identities use filenames; checksums describe the experiment inputs
at measurement time, before the publication cleanup.
`stages_ns` fields are initial SHA, PBKDF2, S-init, main SMix1, main SMix2 and final
HMAC, in that order. Non-instrumented variants report zeroes. `counts[0]` and
`counts[1]` in the parallel-stats variant are total PWX rounds and conflicting
rounds; the other two entries are reserved. Timed work uses unique zero-header
nonces `0..hashes`; varied-header parity runs happen before timing.

The 275-case parity check and digest XOR are necessary experiment checks, not
production cryptographic assurance. The report explicitly discusses measurement
noise, incomplete hardware access, the absence of energy/counter measurements, and
the need for targeted collision tests before shipping a speculative kernel.

Hopper resource compilation (does not require a working GPU):

```sh
nvcc -ccbin /usr/bin/g++-12 -O3 -arch=sm_90 -lineinfo -Xptxas=-v \
  -cubin "$YESPOWER_CUDA_SOURCE" \
  -o /tmp/yespower-yes2-sm90.cubin
```

The saved log used CUDA 12.2. Compiler versions can change register/stack results.
No cubin is committed, and no GPU runtime result is claimed. The original CUDA
harness needs the correctness/accounting fixes in the report before its H/s output
can be treated as validated mining performance.
