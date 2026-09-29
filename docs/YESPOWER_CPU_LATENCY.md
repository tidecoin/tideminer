# Follow-up: yespower CPU latency diagnosis and the two-hash pair kernel

> **Correction and follow-up:** the [mathematical backtrace](YESPOWER_BACKTRACE.md)
> supersedes categorical claims below about the absence of exact shortcuts.
> In the checked-in diagnostic, `d4` masks reads to **16 KiB per table**, not
> 4 KiB, while retaining table writes; it does not establish L1 residency.
> `d5` makes **S0**, not S1, independent. `d2` changes V indexing but retains V
> reads/writes. The recorded measurements remain historical observations;
> their stronger causal interpretations and universal worker-count conclusions
> require further evidence.

This note extends [the main research report](YESPOWER_RESEARCH.md). It answers one
question: **where does Tidecoin yespower CPU time actually go, and can exact
mathematics remove any of it?** All experiments are pinned single-worker or
whole-chip A/B runs on the same i9-13980HX used in the main report. The host was
not isolated, so absolute H/s drift; every comparison below is an A/B inside one
session. No production code or API was changed.

## Findings

1. **The kernel is dominated by the dependency chain through S-box loads, not by
   the multiply, not by SHA/PBKDF2, not by V.** Replacing the two data-dependent
   S indices with constants (a digest-breaking diagnostic) makes the same
   instruction stream run about **2.0x faster on a P-core and 2.9x faster on an
   E-core**. Merely confining the dependent S accesses to 4 KiB, so they stay in
   L1 but remain data-dependent, recovers only about 15-20%. Removing V history
   traffic entirely recovers about 0-8%.
2. **There is no exact mathematical shortcut in PWX.** The address of round
   `k+1` depends on the full 64-bit result of round `k`, which depends on both
   S loads of round `k`. No reordering, precomputation, representation change, or
   time/memory tradeoff found so far removes that cycle without changing the
   digest. Section "Why the mathematics resists" lists every candidate examined.
3. **Two independent hashes interleaved in one thread (`yespower_pair`) is an
   exact win on an otherwise idle core: about 1.36-1.39x** on both P-core and
   E-core. It is validated against the independent scalar reference on 275
   reference headers plus 4,096 varied headers. It reaches the same throughput
   as two hardware SMT threads on a P-core using one thread.
4. **Pairing does not raise whole-chip mining throughput.** With all E-cores
   busy, one hash per core beats pairing (about 6.3-6.9 kH/s total vs
   4.5-5.6 kH/s). With all P-cores busy, 8 pair threads (16 hashes) match
   16 SMT threads (about 10.4 kH/s both), but do not exceed them. Do **not**
   adopt the pair kernel in the miner; `research/yespower/pair/` is an
   experiment.
5. **The whole-chip regime is power and memory bounded, not math bounded.** With
   16 E-cores loaded, E-cores run at about 2.1 GHz instead of 4.0 GHz. The
   diagnostic that removes the S dependency still doubles all-core E throughput
   at that throttled clock, so the dependency is still the largest software
   loss, but no exact software change can exploit it without extra memory
   pressure that the loaded machine cannot absorb.
6. **The existing production configuration is right**: one worker per logical
   CPU (SMT on P-cores, one thread per E-core). The pair experiment's value is
   explanatory, not operational.

## Diagnostic method

`research/yespower/diagnose.py` builds digest-breaking variants that isolate one
cost at a time. They must never be used for mining; the point is the relative
rate, not the output.

| Variant | Change | Question answered |
| --- | --- | --- |
| `baseline` | none | reference |
| `d1` | force both masked S offsets to 0 (`lo = hi = 0`) | cost of the data-dependent S-load chain plus S cache traffic |
| `d2` | force the V history index to 0 after every BlockMix | cost of random V traffic |
| `d4` | keep dependent offsets but mask to 4 KiB | cost of the dependency with the tables L1-resident |
| `d5` | force only the S1 offset to 0 | cost of one of the two loads |

Run:

```sh
python3 research/yespower/diagnose.py --source "$YESPOWER_SOURCE" --cpu 2  --hashes 2048 --trials 3
python3 research/yespower/diagnose.py --source "$YESPOWER_SOURCE" --cpu 16 --hashes 2048 --trials 3
```

Representative medians (H/s):

| Variant | P-core (CPU 2) | E-core (CPU 16) |
| --- | ---: | ---: |
| baseline | 1,329-1,410 | 755-808 |
| d1 (S chain and traffic removed) | 2,540-2,738 | 2,262-2,329 |
| d2 (V traffic removed) | 1,295-1,507 | not measured |
| d4 (dependent, 4 KiB tables) | 1,578-1,674 | 904-914 |
| d5 (S1 load independent) | 1,309-1,606 | not measured |

The robust reading:

- `d1 / baseline` is about **1.9-2.0x** on P and **2.9x** on E. That whole gap
  is the S dependency chain plus its cache footprint.
- `d4 / baseline` is only **1.15-1.2x**, and `d4` has essentially the same
  instruction stream as `d1` but keeps the dependency. **The dependency, not
  cache residency, is the cost.**
- `d2 / baseline` is about **1.0-1.08x**. V is not the problem, matching the
  earlier observation that SMix1+SMix2 are PWX-dominated.

All-16-E-core A/B (short runs, frequency sampled at about 2.1 GHz):

| Variant | Total E H/s | Per core |
| --- | ---: | ---: |
| baseline | 3,743-6,924 | 234-433 |
| d1 | 8,430-13,188 | 527-824 |
| d4 | 4,093 | 256 |
| d5 | 4,374 | 273 |

The gap survives full load; the spread is host noise and thermal state.

## The two-hash pair kernel

`research/yespower/pair/` contains a research copy of the pass-2 kernel that
runs two independent hashes in lockstep:

- `pair_kernel.h` appends pair variants of `blockmix`, `blockmix_xor`,
  `blockmix_xor_save`, `smix1`, `smix2`, and `yespower_pair` to a copied
  `yespower-opt.c`. Both contexts keep their own V, XY, S0/S1/S2 and write
  cursor. The two instruction streams are interleaved at PWX round granularity
  so the out-of-order window always contains both dependent chains.
- `pair_driver.c` verifies pair output against single output for every timed
  nonce and times both modes in alternating order.

Build, validate and time:

```sh
python3 research/yespower/pair/run.py --source "$YESPOWER_SOURCE" --reference "$YESPOWER_REFERENCE" --cpu 2  --hashes 4096
python3 research/yespower/pair/run.py --source "$YESPOWER_SOURCE" --reference "$YESPOWER_REFERENCE" --cpu 16 --hashes 2048
# larger differential corpus (4,115 headers) before timing
python3 research/yespower/pair/run.py --source "$YESPOWER_SOURCE" --reference "$YESPOWER_REFERENCE" --cpu 2 --hashes 1024 --varied 4096
```

Single-thread results:

| CPU | single median H/s | pair median H/s | speedup |
| --- | ---: | ---: | ---: |
| P-core CPU 2 | 1,228-1,383 | 1,834-1,921 | 1.39-1.49 |
| E-core CPU 16 | 799 | 1,090 | 1.36 |

Two hardware SMT threads on one P-core gave about 981 + 981 = 1,961 H/s. One
pair thread gives about 1,900 H/s, so **software interleaving nearly exactly
reproduces SMT** on one hardware thread. On E-cores, where no SMT exists, it is
a genuine 1.36x single-thread improvement.

Whole-chip A/B (short runs, same session; hashes in parentheses):

| Configuration | Total H/s |
| --- | ---: |
| 16 E-cores, 1 single hash each (16) | 6,342-6,924 |
| 8 E-cores, pair (16) | 5,173-5,575 |
| 16 E-cores, pair (32) | 4,502-5,097 |
| 8 P-cores, 1 single hash each (8) | 8,174 |
| 8 P-cores, SMT, 1 single hash each (16) | 10,351 |
| 8 P-cores, pair (16) | 10,463 |
| 8 P-cores, pair on both SMT siblings (32) | 8,989 |

Interpretation: the pair kernel doubles each worker's live working set (two V
arrays and two 96 KiB S states). On an idle core that buys latency hiding. Under
full load the memory system and the 2.1 GHz power-limited clock dominate, and the
extra per-core footprint costs more than the overlap gains. E-cores saturate at
roughly 5-7 kH/s total regardless of configuration.

## Why the mathematics resists

A PWX round acts on four 128-bit pairs. For one pair with words `(x0, x1)`:

```text
lo = low32(x0) & 0x7ff0          hi = high32(x0) & 0x7ff0
y0 = (low32(x0) * high32(x0) + S0[lo]) ^ S1[hi]
y1 = (low32(x1) * high32(x1) + S0[lo+8]) ^ S1[hi+8]
```

The same two S entries serve both words. The next round's `lo/hi` come from
`y0` (and `y1` supplies only the multiply input). Every candidate that follows
was checked against this recurrence:

| Candidate | Result |
| --- | --- |
| Reorder the 4 pairs / issue loads earlier | Already the source of the measured stall; the GPU-inspired speculative version in the main report is correct but slower on CPU. |
| Forward recently written S slots in registers | The write cursor is known, but the read addresses are not; only about 0.26% of rounds conflict. Hardware store-to-load forwarding already handles the exact-address case. |
| Replace the S load latency with computation | `S0`/`S1` values are 64-bit and data-dependent; no cheaper representation exists. |
| Algebraic collapse of two rounds | `(p + s) ^ t` then a second masked lookup; the table index depends on the carry-propagating sum, so no closed form. |
| Reduce S below 96 KiB / change Swidth or N/r | Changes consensus. |
| Precompute S or V across nonces | S and V depend on the full PBKDF2 output, which changes with every nonce. |
| Store all candidate first-block SHA midstates | SHA/PBKDF2 plus S-init together are under 3% of runtime; the ceiling is tiny. |
| Compress or checkpoint V | V is deterministic but not cheaply compressible; reconstruction needs the correct historical S state. |
| Early-reject on an intermediate | The only exact predicate is the final digest; the target comparison is after all work. |
| Pick nonces with friendly table indices | Biased sampling; accepted work per second is what counts. |
| Drop the rare conflicting writes | Breaks the digest, measured earlier at hundreds of fallbacks per hash. |
| Use `u*v` low-32-only or floating point | Not exact for arbitrary 64-bit results. |

The one exact transformation that does exist is **more independent work in the
same execution stream**, because the recurrence cannot be shortened but its
latency can be hidden. SMT does that on P-cores for free; the pair kernel does it
on E-cores and single P-threads, but it cannot beat the whole-chip memory and
power ceiling.

## Practical conclusions for the miner

- Keep one hashing worker per logical CPU. On the measured chip that is two
  workers per P-core (SMT) and one per E-core. This is what the current
  `--threads` default already does.
- Do not switch E-cores to the pair kernel. Under full load it is a 20-30% loss.
  On P-cores the 8-pair configuration ties the 16-thread SMT configuration
  (10.5 kH/s vs 10.4 kH/s) at half the thread count, so there is no throughput
  reason to change it either.
- The pair kernel is still useful as a tool: it isolates the dependency stall and
  can validate scheduling ideas without a second core.
- Remaining small levers are outside the kernel math: power/thermal headroom
  (the measured E-core clock halves under all-core load), and possibly layout/TLB
  work, which the `d4` result suggests is second order.
- A 128-bit SIMD kernel cannot host three interleaved contexts without spilling
  (12 XMM state registers plus temporaries). Three-way pairing was not attempted
  for that reason.

## Caveats and reproducibility

The P/E absolute rates vary with package power, ambient temperature and host
background load; several whole-chip runs differed by 30% or more between
sessions. All speedup claims above come from A/B measurements inside one session
where possible. The diagnostic variants intentionally produce wrong digests. The
pair kernel passed the 275-header scalar oracle and a further 4,096 deterministic
varied headers (`--varied 4096`). Timing uses one explicit C context per worker,
64 warmup hashes, and unique nonces. `perf_event_paranoid=4` still blocks hardware
counters, so the stall attribution is behavioural rather than counter-based.
