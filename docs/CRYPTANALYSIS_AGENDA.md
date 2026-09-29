# Tidecoin yespower cryptanalysis agenda

Prepared 2026-09-15. Research plan. No production code was changed and no new
experiments were run. This note defines what a mining-relevant break would be,
records what has already been tested, and lists the bounded experiments that
remain. Related: [YESPOWER_RESEARCH.md](YESPOWER_RESEARCH.md),
[YESPOWER_BACKTRACE.md](YESPOWER_BACKTRACE.md),
[YESPOWER_CPU_LATENCY.md](YESPOWER_CPU_LATENCY.md),
[YESPOWER_SHORTCUT_ANALYSIS.md](YESPOWER_SHORTCUT_ANALYSIS.md).

## 1. What "crack" can mean

1. **Evaluation shortcut**: same digest, less work. A mining acceleration; no
   consensus consequence.
2. **Distributional weakness**:
   - *global*: `P(H < T) = (1+eps) * T / 2^256`, worth `1+eps`;
   - *conditional/steerable*: a cheap predicate on miner-controlled input
     raises success probability; the only class with large upside;
   - *correlation*: related inputs produce related outputs; enables incremental
     computation.
   A distributional weakness is a coin security finding and may justify a
   protocol change.
3. **Full preimage break**: find below-target digests without the work.

Not cracks: partitioning trials across headers, blocks or heights (independent
Bernoulli trials; the expected work is invariant), and empty-block template
reuse (worth ~0.02-0.044%).

## 2. Evidence to date

| Item | Result | Source |
| --- | --- | --- |
| Stage shares | SMix1 73.1%, SMix2 24.3%, S init 2.3%, SHA/PBKDF2/HMAC ~0.35% | YESPOWER_RESEARCH |
| Header reuse | only first 64-byte SHA midstate; one nonce step flips 128.01/256 digest bits and 511.81/1024 B bits | reuse.json |
| Backtrace | 8.3M live nodes; 0 of 1,049,424 multiplications removable; 85.0% of SMix2 V stores dead but storage-only | backtrace.json |
| Lazy V, terminal pruning | exact prototypes; no reliable speedup | backtrace.json |
| Composite rounds | 47-bit address prefix needs all 32 bits of both operands | SHORTCUT B.1/B.3 |
| Speculation/hoisting | software reorder 2.6% slower; checked conflicts 8-11% slower on P, 7.5% on E | form2-hoist.json |
| Pair interleaving | exact 1.36-1.39x on an idle core; no whole-chip gain | YESPOWER_CPU_LATENCY |
| Diffusion, one PWX round | every flip changes the output; 97.091% change next address bits | diffusion.json |
| Bias suite | 1,048,576 hashes per mode; MSB chi-square 240.6/250.0 (df 255); interaction 3996.2/4115.0 (df 4095); adjacency 0.00391/0.00386 | bias.json, TIDECOIN_BLOCK_TIMING |

Documentation correction recorded here for the record: the final HMAC message
is the pre-SMix seed `B[0:32]`, saved at `yespower-opt.c:1108`, not the initial
SHA-256 digest. `YESPOWER_SHORTCUT_ANALYSIS.md` was corrected accordingly.

### Not tested

- Full-digest avalanche and uniformity over all 256 bits.
- Conditional/steerable bias beyond `(field & 15) x MSB` on one fixed prefix.
- SAT/SMT/ANF search for address predictors or composite rounds.
- MILP differential/linear trails.
- Lazy S initialization.
- ASIC/FPGA cost model.

## 3. The SHA-256 gate

The only miner-controlled input channel is `SHA256(header) -> PBKDF2 -> B`
(the code order is `SHA256_Buf`, then `PBKDF2_SHA256`, then `smix_1_0`). The
Salsa/PWX mixers are keyed by `B` and never see header bytes. A conditional bias
therefore requires a weakness at the SHA-256/HMAC boundary, and a bias
conditioned on special `B` values is not steerable because finding such nonces
means inverting SHA-256.

Blockchain history cannot detect success-rate bias: it contains only successes.
Under a multiplicative bias, `H/T` remains uniform conditional on `H < T`, and
the attempt denominator does not exist on chain. Mining-relevant bias testing
needs instrumented attempt ranges (miner logs, pool share logs with ranges) or
controlled synthetic scans.

## 4. Programme

### 4.1 Full-digest bias suite

- 256-bit avalanche for single-bit nonce/ntime flips over many random headers;
  compare Hamming distance to Binomial(256, 0.5).
- Byte-wise and bit-wise uniformity; condition on 8-16 input bits, with
  multiple-testing correction and a held-out sample.
- Several random header prefixes, not one fixed family.
- Target-relative shape test only; it cannot measure rate bias.
- Power: a 5-sigma single-bin relative deviation `eps` needs about
  `6.4e9 * (eps/0.001)^-2` hashes, i.e. days at measured whole-chip rates for
  0.1%. State the bound, not a claim of bias, when the budget is smaller.

### 4.2 Reduced-width formal search

- Encode one or two PWX rounds over 8/16-bit words with concrete or symbolic
  tables in SAT/SMT (CaDiCaL, CryptoMiniSat, Bitwuzla, z3), plus ANF tooling.
- Ask: does a cheaper composition reproduce the 47-bit address prefix exactly?
  Prove UNSAT for a bounded operation count, or extract a witness.
- Measure algebraic degree and linear correlation of the 22 address bits.
- Full-width (32-bit, 1024-byte state, 2,732 BlockMix calls) is out of reach for
  any solver today; feasibility comes from the abstraction, not hardware.

### 4.3 MILP trails

- Model add/xor/rotate difference propagation and the multiplicative PWX step;
  treat S lookups as random to obtain generic upper bounds.
- Goal: a reduced-round truncated trail that predicts an output prefix or keeps
  address bits constant. A distinguisher is not a speedup unless it predicts
  target-relevant bits cheaply.

### 4.4 Lazy S initialization

- S init is 2.3% and 12.547% of its stores are never read. Prototype an exact
  scheme that skips serialization of never-read entries while preserving the
  chain; expected ceiling is low but this transformation has not been tried.

### 4.5 ASIC/FPGA cost model

- Quantify SRAM per engine (2.1 MiB V plus 96 KiB S), serial load latency, and
  parallel engine count. Decides whether the GPU/ASIC-resistance assumption
  holds economically.

## 5. Expected gains

| Finding | Mechanism | Realistic gain |
| --- | --- | --- |
| Global bias eps | higher hit rate | `1+eps` |
| Conditional bias | skip/filter nonces | large in principle; no evidence |
| Exact composite round | fewer ops/round | only if it breaks the load dependency; otherwise bounded by arithmetic share |
| Address predictor | earlier loads, parallel rounds | ~0 on OoO x86; maybe 10-40% in-order/GPU |
| Locality | S tables in L1 | <=15-20% (d4 = 1.15-1.2x) |
| Trail/distinguisher | usually none | 0 unless target bits are predicted |
| Negative proof | none | retires the question |

Measured ceilings: removing the S dependency entirely is 2.0x P-core / 2.9x
E-core (digest-breaking diagnostic, not achievable exactly); removing V traffic
is 1.0-1.08x.

## 6. Hardware

- Measured whole-chip i9-13980HX rate is roughly 14-17 kH/s (16 P-threads about
  10.4 kH/s; E-cores about 3.7-6.9 kH/s throttled). Bias samples: 1e9 hashes
  about 16-20 hours; 6.4e9 about 4-5 days.
- SAT/SMT is RAM-bound and mostly single-threaded: the current 62 GB machine
  handles reduced-width instances; hard instances want a 256-512 GB node.
- Full-width search is not a hardware problem; no CPU makes it tractable.
- GPUs are not useful for yespower evaluation; they can help bitsliced Boolean
  analysis.

## 7. Deliverables

- Bias bounds at an explicitly stated sample size and corrected significance.
- A reduced-width result on the composite/address-predictor question, positive
  or negative.
- Trail bounds for reduced rounds.
- A documented decision on lazy S init and the ASIC cost model.

If a real evaluation shortcut appears it is fair game for every miner. If a
security weakness appears, the responsible path is coordinated analysis and a
protocol decision, not silent exploitation.
