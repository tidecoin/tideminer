# Tidecoin job reuse and the three PWX shortcut candidates

This note answers two questions that follow from
[the CPU latency study](YESPOWER_CPU_LATENCY.md) and
[the backtrace study](YESPOWER_BACKTRACE.md):

1. Tidecoin blocks are mostly empty and a pool changes few header fields between
   jobs. Can recurring header structure make hashing cheaper?
2. The backtrace left one door open: a transformation that computes several PWX
   steps more cheaply than executing them. Which of the three concrete forms can
   actually work?

The short answer is that job reuse saves nothing measurable (the reusable SHA
work is 0.044% of a hash), and each of the three transformations is blocked by a
concrete structural obstruction. The obstructing property is worth stating
precisely because it sharpens future search: **the next PWX address needs a
47-bit prefix of the next value, and that prefix requires all bits of both
32-bit multiplication operands.** No partial-product or narrow-state schedule can
leapfrog a round.

All new experiments in this note are diagnostic or measurement only. No
production code was changed.

## Findings

1. **Empty blocks and near-identical jobs do not help.** Across a nonce sweep only
   the SHA-256 midstate over the first 64 bytes is reusable. The measured whole
   initial SHA stage is 0.044% of a hash; PBKDF2 is 0.214%; final HMAC 0.095%.
   Eliminating SHA, PBKDF2 and HMAC *entirely* would give about 0.35%. Measured
   avalanche confirms why: changing one nonce changes 128.0 of 256 SHA-256 digest
   bits and 511.8 of 1024 PBKDF2 seed bits on average, so B, S and V never repeat.
2. **No smaller exact state is available in this structure.** To advance one word
   you need its full 64 bits: the product loses addressing information, the
   address prefix needs the full operands, and the companion-word decomposition
   is scheduling-only. The backtrace's live-node counts agree.
3. **Conditional parallel execution has no software headroom on this CPU.**
   Explicitly hoisting a round's four loads before its stores, with no hazard
   check, is 2.6% *slower* than the compiler's sequential form; the exact
   conflict-checked version is 8-11% slower. Out-of-order memory disambiguation
   already performs the reorder, so added checks and live values are pure cost.
4. **Collapsing rounds is blocked by an address-prefix dependency.** Any composite
   that reproduces future computation must resolve the intermediate round's table
   indices. Those indices are bits 4-14 and 36-46 of the intermediate value, so a
   47-bit prefix of the product is required; producing that prefix requires both
   full 32-bit operands. Diffusion confirms the absence of a local predictor:
   every one-bit input change alters the output, and 97.1% of single-bit changes
   alter the next-round address bits.
5. **The practical Tidecoin levers remain elsewhere**: prepared-header midstate
   (tiny), job pipelining/stale-work policy (latency, not H/s), and power,
   thermal and topology control, which the earlier measurements show dominate
   whole-chip throughput.

## Part A: what "mostly empty blocks" allows

### A.1 Header fields across a job

For a standard 80-byte Tidecoin header:

| Bytes | Field | Typical behavior |
| --- | --- | --- |
| 0-3 | version | constant, or rolled if the pool allows version rolling |
| 4-35 | previous block hash | constant within a block/job |
| 36-67 | merkle root | constant for an empty-block template with fixed coinbase/extranonce |
| 68-71 | time | changed by the pool per job, sometimes fixed |
| 72-75 | nBits | constant for long periods |
| 76-79 | nonce | swept by the miner |

So within one job the first 64 bytes (and often the first 76) are constant, and
the question is real: what can be reused when only nonce or time changes?

### A.2 The dependency chain to the digest

The yespower pipeline is

```text
digest = SHA256(header)
B      = PBKDF2-HMAC-SHA256(key = digest, salt = "", c = 1, dkLen = 128)
seed   = B[0:32]                   # saved before SMix mutates B
S      = S_init(B)                 # 96 KiB, depends on every B byte
SMix1/SMix2(B, S, V)               # >99% of the work
out    = HMAC-SHA256(key = B[960:1024], message = seed)
```

- Only the first SHA-256 compression block (header bytes 0-63) is shared between
  nonces. Everything after that is a function of the full 32-byte digest.
- There is no partial SHA-256 increment: changing one word of the second block
  changes the whole message schedule and all 64 compression rounds.
- PBKDF2's password is the digest, so B changes completely.
- The final HMAC message is the pre-SMix seed `B[0:32]`, copied before `smix_1_0`
  calls mutate B. It is not the initial SHA-256 digest; the two values differ and
  must not be interchanged.
- S initialization and every V entry depend on B. Scratch memory can be reused;
  its **contents** cannot.

### A.3 Measurements

`research/yespower/reuse.c` hashes 4,096 consecutive nonces that share the first
76 header bytes and reports:

| Quantity | Value |
| --- | --- |
| Midstate after first 64 bytes identical | 4,096 / 4,096 |
| Mean SHA-256 digest bits changed per nonce step | 128.01 / 256 |
| Mean PBKDF2 seed (B) bits changed per nonce step | 511.81 / 1024 |

The same-session stage medians (2,048 hashes, P-core, GCC native) are:

| Stage | Share of hash |
| --- | ---: |
| Initial SHA-256 | 0.044% |
| PBKDF2, c=1 | 0.214% |
| S initialization | 2.262% |
| Main SMix1 | 73.133% |
| Main SMix2 | 24.253% |
| Final HMAC | 0.095% |

Therefore:

- A perfect prepared-header implementation (reuse the midstate and specialize the
  fixed second-block words) can save at most the 0.044% initial SHA, and in
  practice about 0.02%. The measured `gcc_native_midstate` variant appearing 3%
  faster in one session is contradicted by the stage budget and is host noise.
- Even deleting SHA, PBKDF2 and HMAC completely leaves 99.65% of the runtime.
- Empty blocks change none of this. No S or V state can be carried across
  distinct nonces.

### A.4 What is still worth doing for the miner

These are engineering wins, not hash-math wins:

1. **Prepared header API**: compute the first 64-byte SHA state once per job,
   compress only the last 16 bytes per nonce. Worth ~0.02%, but it is easy and
   removes a per-hash branch.
2. **Job pipelining**: build the next job's coinbase/merkle/header while the
   current job is hashing, so a `mining.notify` does not stall workers.
3. **Time/version rolling only as a fallback.** At about 45 kH/s total, the full
   2^32 nonce space takes roughly 26.5 hours; a pool sends new jobs every few
   tens of seconds, so the space is never close to exhausted. Rolling is useful
   only if the pool goes quiet, and must respect the pool's rules.
4. **Do not optimize S_init (2.26%) or SHA (0.35%) further** until the main SMix
   has been improved, which the latency study shows is dependency-bound.

## Part B: the three shortcut forms

### B.1 Form 1: a smaller mathematical state

**Claim: the per-word state cannot be compressed below 64 bits while remaining
exact.**

Write one word pair update for a single plane:

```text
u = low32(x)          v = high32(x)
i = u & 0x7ff0        j = v & 0x7ff0
x' = ((u * v) + S0[i]) XOR S1[j]
```

- The next address is a function of `x'`: `i' = low32(x') & 0x7ff0` and
  `j' = high32(x') & 0x7ff0`.
- `i'` needs `x' mod 2^15`; `j'` needs `x' mod 2^47` (bits 36-46 live at bit 46).
- `x' mod 2^47 = ((u*v mod 2^47) + S0[i] mod 2^47) XOR S1[j] mod 2^47`.
- Since 2^47 is larger than 2^32, `u*v mod 2^47` depends on **all 32 bits of u
  and all 32 bits of v**. No operand prefix can be dropped.
- The product alone (`u*v`) is insufficient because `S0[i]` and `S1[j]` are
  selected by `u` and `v` separately; two states with the same product but
  swapped halves read different entries, as the backtrace note already showed.
- `(product, i, j)` is a sufficient statistic but is 64 + 11 + 11 = 86 bits,
  larger than the 64-bit value it replaces.
- The first-word/companion-word decomposition is exact but scheduling-only: the
  companion plane is still required for its own table writes and for Salsa,
  which mixes both planes.

A tempting near-miss: since addresses use only bits 4-14, one might hope the low
15 bits of each word form a closed subsystem. They do not, because the entry
*selected* as `S1[j]` is controlled by the high-half address `j`, which needs the
47-bit prefix. The smallest closed projection that determines all future
addresses is therefore the full 64-bit word. This is exactly what the backtrace's
bit-mask slice found from the other direction.

### B.2 Form 2: conditional parallel execution

Model of one PWX round: pairs `g = 0..3` read `S0[lo_g]`, `S1[hi_g]` and write
`S0[w_g]`, `S1[w_g]`. A later pair can read a slot written by an earlier pair, so
the conservative schedule reads after writes. Three ways to exploit the rare
non-conflict case:

1. **Hardware speculation** (already active): modern x86 memory disambiguation
   predicts no-alias and executes loads ahead of stores, replaying on a detected
   alias. The conflict rate is about 0.26% of rounds, so replays are negligible.
2. **Software checks plus fallback** (previous work): correct, but 8-11% slower
   on P-core and 7.5% on E-core in earlier sessions. The checks, address
   materialization and extra live values cost more than they save.
3. **Explicit hoisting with no checks** (new, `h0` in `diagnose.py`): all four
   pairs' entries are loaded before any store, which is the best case a software
   schedule could achieve. Same-session median: baseline 1,581.7 H/s versus
   1,540.5 H/s hoisted, i.e. **2.6% slower**, not faster.

The hoist result is the decisive one: if there were recoverable scheduling
headroom, the hoisted form would show it. Instead it only removes opportunities
from the compiler and adds register pressure. On this CPU, the order of
loads and stores within a round is already handled better by the hardware than by
the source.

Genuine extra parallelism exists only **across independent hashes**, which the
pair kernel measures (1.36-1.39x on an idle core, no whole-chip gain; see
[the latency note](YESPOWER_CPU_LATENCY.md)). An in-order CPU or a GPU with
explicit memory transactions could still prefer the checked schedule, but there
is no evidence that an exact conflict-aware schedule beats a plain sequential one
on an out-of-order x86 core.

### B.3 Form 3: collapsing several rounds into a cheaper composite

We look for `G` with

```text
ordinary:   x1 = F(x0, S) ; x2 = F(x1, S')
shortcut:   x2 = G(x0, S)
```

while also reproducing every table write that future computation observes.

**Address-prefix obstruction.** The second call `F(x1, S')` selects table entries
using bits 4-14 and 36-46 of `x1`. Those bits are bits 4-46 of `x1`, so any `G`
must produce a 47-bit prefix of `x1` exactly. From B.1 that prefix is
`((u0*v0 mod 2^47) + S0[i] mod 2^47) XOR S1[j] mod 2^47`, which requires both
full 32-bit operands of `x0`. Thus:

- No composite can "skip" the intermediate multiplication while still resolving
  the intermediate addresses.
- A composite would need a cheaper predictor for the address bits (bits 4-46) of
  `F(x)` without computing `F`. No such predictor is known, and the round is
  designed to prevent one.
- Even if a cheaper predictor existed, `G` must also produce all table writes that
  later lookups observe. Within a two-round window the write cursor advances
  deterministically, but whether a written slot is read depends on the skipped
  intermediate addresses, feeding the same circularity back.

**Diffusion measurement** (`research/yespower/diffusion.c`, 20,000 random states,
random S entries, one round, each of 64 input bits flipped):

| Quantity | Value |
| --- | ---: |
| Fraction of bit flips that change the output word | 1.00000 |
| Mean output bits changed per flip | 22.12 / 64 |
| Fraction of flips that change the next-round address bits | 0.97091 |
| Mean next-address bits changed per flip | 3.23 / 22 |

The output dependence is total, and the next-round addresses are sensitive to
almost every input bit. There is no visible local structure (for example, a small
subset of input bits determining the address) that a composite could exploit.

**What a shortcut would have to look like.** It is not enough to be nonlinear;
the composition would need one of:

- an exact closed form for `addr(F(x))` cheaper than evaluating `F` (a
  cryptanalytic weakness of the PWX round), or
- an invariant that lets the table write/read collisions be resolved without the
  intermediate value, or
- a representation in which the 47-bit prefix is produced together with the next
  state at less cost than one multiply-add-xor with two loads.

None has been found. The backtrace's 8.3 M-node live graph is the strongest
available evidence that ordinary pruning cannot remove the intermediate steps,
and the prefix argument above shows why a composite cannot sidestep them by
partial computation.

### B.4 Why dead storage does not become dead work

The backtrace found roughly 85% of SMix2 V writes unread and about 44% of all V
versions dead, but neither lazy V nor terminal pruning produced a reliable gain.
The reason is structural, not an implementation accident:

- A row write is dead only if no later iteration selects that row. Detecting that
  online requires the future address stream, which depends on the values being
  computed.
- The only exact online representation (lazy V) retains X history and
  reconstructs rows on revisits. SMix2 has 684 selections over 2,048 rows, so
  only about 103 revisits occur; a bounded recent-state cache cannot capture them
  without retaining hundreds of KiB, and reconstruction then costs more than the
  skipped write.
- The same holds for S: a dead S store does not make its multiplication dead,
  because the result also continues along the PWX state chain.

Dead-store elimination is therefore a storage result, not an arithmetic result,
and it does not translate into a mining speedup on this host.

## Part C: relevance to Tidecoin efficiency

Combining this note with the earlier measurements:

| Lever | Realistic upside | Recommendation |
| --- | ---: | --- |
| Prepared header/midstate | ~0.02% | Implement for cleanliness, not speed |
| SHA/PBKDF2/HMAC replacement | <0.35% total | Not worth research priority |
| S_init (2.26%) | small | Only after SMix work |
| Conflict-aware PWX scheduling | negative on this CPU | Do not pursue on OoO x86 |
| Smaller state / composite rounds | none found | Documented obstruction; not a mining lead |
| Dead V/S write elimination | no reliable gain | Research curiosity |
| Two-hash interleaving | +36% per idle core; 0 whole-chip | Use only on underutilized machines |
| Power/thermal/topology | dominant at full load | Highest-value remaining work |
| Job pipelining/stale policy | effective shares | Miner engineering, not hashing |

Tidecoin's empty blocks do not change the economics of any row above. The
accepted-work rate is set by whole-chip hashing throughput, and the earlier
measurements show that throughput is bounded by power/frequency and the S-box
dependency stall, not by header structure or SHA.

## Reproduction

```sh
# Header reuse and avalanche (writes stdout JSON)
SRC="$YESPOWER_SOURCE"
gcc -O2 -std=gnu99 -I"$SRC" research/yespower/reuse.c "$SRC/sha256.c" -o /tmp/reuse
/tmp/reuse

# PWX diffusion probe
gcc -O2 -std=gnu99 research/yespower/diffusion.c -o /tmp/diffusion
/tmp/diffusion

# Hoisting headroom (digest-breaking diagnostic; never mine with h0)
python3 research/yespower/diagnose.py --source "$YESPOWER_SOURCE" --cpu 2 --hashes 2048 --trials 5 \
  --variants baseline,h0 --output research/yespower/results/form2-hoist.json

# Prepared-header and conflict-aware comparison, plus stage shares
python3 research/yespower/run.py --source "$YESPOWER_SOURCE" --reference "$YESPOWER_REFERENCE" --cpu 2 --hashes 1024 --trials 3 \
  --variants gcc_native,gcc_native_midstate,gcc_native_parallel,gcc_native_stages \
  --output research/yespower/results/form2-window.json
```

Saved outputs: `results/reuse.json`, `results/diffusion.json`,
`results/form2-hoist.json`, `results/form2-window.json`. Raw backtrace counts
remain in `results/backtrace.json`. As before, timing is not counter-based
(`perf_event_paranoid=4`) and the host is shared; all speed comparisons are
same-session A/B runs, and the staged SHA/PBKDF2 shares are medians over
instrumented runs.

## Limits

- The prefix argument is an obstruction for the *specific* PWX construction, not
  a proof that no algorithm whatsoever can be faster. A cryptanalytic weakness
  in the round function, if found, would invalidate it.
- The diffusion probe used random tables rather than a specific hash's S state;
  the dependency structure does not depend on the table values, but this is
  evidence, not a theorem.
- The `h0` hoisting variant is intentionally digest-breaking on alias rounds; its
  timing bounds the *best case* for software reordering, not a usable kernel.
