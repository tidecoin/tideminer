# Mathematical backtrace of Tidecoin yespower

## Result

The backward analysis found removable **storage**, but no removable PWX arithmetic
in the evaluated dependency graph. Every one of the **1,049,424 unsigned
32×32→64 multiplications per hash**, and every PWX S-table load, remains live.
This is a result about pruning this computation, not a proof that no different
algebraic algorithm can be faster. [1][2]

There is substantial dead history storage. Across 27 independently checked
headers, approximately **85% of SMix2 V writes** are never subsequently read.
An exact lazy representation can avoid those immediate writes. It has been
implemented and validated, but has not demonstrated a reliable speedup in the
exploratory CPU measurements. A second prototype removes provably unused final
output and history writes. It also remains experimental. [3][4]

The first-word/companion-word decomposition of PWX is valid. The independent
evaluator executes both the ordinary order and the decomposed order and checks
their complete digests. Their dependency slices are identical: decomposition
changes scheduling opportunities, not the number of required arithmetic results.
[1][2]

The analysis concerns exactly **yespower 1.0, N=2048, r=8, no personalization,
80-byte input**. SHA-256, PBKDF2 and HMAC are evaluated using the local C library;
the dependency graph covers SMix, including S initialization. No production source
or miner backend is changed.

## 1. Start with the bytes the verifier actually consumes

The final output is:

```text
digest = HMAC-SHA256(key = B[960:1024], message = saved_seed[0:32])
```

Only the last 64 bytes of the final 1,024-byte B enter this HMAC. The other
960 bytes, the final contents of V and S, and the final S write cursor do not
escape the hash. Consequently, final serialization of those 960 bytes and
unobserved terminal scratch updates can be omitted. [5][6]

This does not imply that only one sixteenth of the final BlockMix computation
is needed. Write its sixteen 64-byte input chunks as B₀,…,B₁₅. Let P be one
PWX invocation together with its S-state updates:

```text
C₋₁ = B₁₅
(Cᵢ, Sᵢ₊₁) = P(Cᵢ₋₁ XOR Bᵢ, Sᵢ),   i = 0,…,15
Yᵢ = Cᵢ,                               i < 15
Y₁₅ = Salsa20/2(C₁₅)
```

Although Y₀,…,Y₁₄ need not be stored after the last BlockMix, their values
C₀,…,C₁₄ are successive inputs to C₁₅. The state recurrence consumes them even
when the output array does not. Removing an output store is different from
removing the computation that produced it.

Working one SMix2 step farther backward requires its input X and selected V row:

```text
jₜ = Integerify(Xₜ) AND 2047
Uₜ = Xₜ XOR Vₜ[jₜ]
Vₜ₊₁[jₜ] = Uₜ
Xₜ₊₁ = BlockMix(Uₜ, Sₜ)
```

The full X input propagates into the next BlockMix through its chunk XORs and
initial last-chunk state. The final Salsa couples the two word classes discussed
below. Backward propagation therefore expands into the earlier core computation;
it does not remain confined to the final 64-byte output slot. [5][6]

## 2. What the executable backtrace does

`research/yespower/backtrace/trace.cpp` is a separate C++ evaluator of the
reference SMix algorithm. It carries both the actual numerical value and a
dependency-node identifier for each word. It reproduces S initialization, seed
expansion, 2,048 SMix1 iterations and 684 SMix2 iterations. The final key is
unshuffled and passed through the original C HMAC. [1]

Every arithmetic result gets a node. Every S/V store gets a new version node,
even when it overwrites the same address. Every table load depends on:

1. The version of the stored value actually read.
2. The state bits that selected that address.

The second dependency is essential. Treating a runtime address as a free constant
would incorrectly suggest that its producer can be removed. Pointer rotations
and fixed write-cursor movement are deterministic scheduling state; the evaluator
follows them explicitly.

Dynamic V stores also depend on the state bits that select their destination.
Pruned replay checks those live store addresses in addition to load addresses.

The graph has **8,325,432 nodes per input**. Backward propagation starts with
all 512 bits of the final HMAC key. It uses conservative bit masks rather than
only marking whole words:

| Operation | Bits required from parents |
| --- | --- |
| XOR | The requested result bits from both operands |
| Addition modulo 2ʷ | All bits through the highest requested result bit, accounting for carry |
| 32×32 multiplication | The low required prefix of each 32-bit operand; a requested output bit at position 31 or above conservatively requires both full operands |
| Rotate | The inverse-rotated requested mask |
| Pack/split | The corresponding low or high mask |
| S lookup | Requested stored-value bits plus index bits 4–14 or 36–46 of the address-producing word |
| V lookup | Requested stored-value bits plus the low index bits selected by wrap/modulo |
| Store/copy | Requested bits of the value stored |

These rules deliberately do not solve algebraic identities between multiple
parents or exploit particular zero operands. They overapproximate dependencies.
A live node means this analysis did not establish that it can be omitted; it is
not a universal lower bound on implementations.

There is a further check beyond comparing ordinary execution to the reference:
the program **replays only the live graph**, zeros unneeded result bits, checks
all **2,460,544 live lookup addresses**, and verifies the complete final key.
This confirms that the pruned graph retains the observed execution's required
values and addresses. The graph itself is constructed by doing a full hash first;
that construction is not a shortcut a miner can obtain for free. [1][2]

The corpus contains the existing 19 fixture headers and eight independently
generated varied headers. Both execution orders match the independent scalar C
digest for all 27, and their slice counts agree. Source hashes and per-header
counts are recorded in `results/backtrace.json`. The corpus supports reproducible
observations; it is not an exhaustive input proof.

An additional build with GCC/G++ `-O1 -g -fsanitize=address,undefined` checked
normal and split evaluation plus pruned replay on the first fixture with no
sanitizer findings. Python compilation checks and the unchanged miner's
`cargo check` also pass.

## 3. What is live, and what is dead

The following counts refer to the evaluator's scalar 64-bit operations and
storage versions, not CPU instructions, cache lines or physical memory traffic.
S initialization also uses 32-bit Salsa arithmetic. [2]

| Phase | PWX multiplications | Removable multiplications found |
| --- | ---: | ---: |
| Seed expansion | 336 | 0 |
| SMix1 | 786,432 | 0 |
| SMix2 | 262,656 | 0 |
| Total | **1,049,424** | **0** |

All 64 result bits of these multiplication nodes remain live. All associated
PWX additions, XORs and S loads remain live too. This does not mean every bit
must travel through every intermediate instruction in an optimal implementation;
it means ordinary dead-result and truncated-result pruning did not expose a
smaller PWX computation.

Storage has a different result:

| Phase and storage | 64-bit stores/hash | Mean dead stores | Mean dead fraction |
| --- | ---: | ---: | ---: |
| S initialization → S | 12,288 | 1,541.78 | 12.547% |
| Expansion → S | 224 | 19.04 | 8.499% |
| SMix1 → S | 524,288 | 40,256.59 | 7.678% |
| SMix2 → S | 175,104 | 16,553.78 | 9.454% |
| SMix1 → V | 262,144 | 80,824.89 | 30.832% |
| SMix2 → V | 87,552 | 74,382.22 | **84.958%** |
| All BlockMix → B intermediates | 349,808 | 120 | 0.0343% |

Across all phases, about 8.20% of S storage versions and 44.38% of V storage
versions are dead. The 120 dead B words are exactly the last BlockMix's first
960 output bytes. Some corresponding stores may already be absent in optimized
C because that implementation uses different buffer ownership and fusion than
the reference evaluator. These counts must not be presented as a promise of that
many removable machine stores in the existing optimized kernel.

Most importantly, a dead S store does not make its underlying multiplication
dead. That result also continues along the current PWX state chain.

### Why roughly 85% of SMix2 V writes are dead

This is largely a consequence of a short read/write pass over a larger table.
Within SMix2, a V update is read only if a later iteration chooses that same row.
For every distinct row selected, the final write to that row is dead because
there is no later V consumer after SMix2. Earlier writes to the row feed its next
selection. Thus, in this graph:

```text
dead SMix2 row writes = number of distinct V rows selected during SMix2
```

Under an illustrative independent uniform-index model, the expected fraction is:

```text
N × (1 − (1 − 1/N)^m) / m,       N=2048, m=684
```

This is approximately 85%. Independence/uniformity is a model, not a proven
property of the cryptographic trajectory. The recorded traces supply the actual
counts.

There is no equivalent free online test for “this is the last visit to row j.”
Later addresses depend on later states. Skipping the write on the assumption
that the row will not recur would produce wrong hashes when it does recur.

## 4. Exact shortcut prototype: lazy V updates

The recurrence above admits a useful algebraic representation. Let V₀ be V at
the start of SMix2. Since each update XORs in the current X, for every row j:

```text
Vₜ[j] = V₀[j] XOR ⨁ Xᵤ
                    u < t, jᵤ = j
```

This follows by induction: rows other than jₜ are unchanged, and the selected
row gains exactly one additional XOR operand Xₜ. XOR associativity permits the
representation; no statistical assumption is needed.

`backtrace_lazy_smix2` implements it by retaining X₀,…,X₆₈₄ and maintaining a
linked list of previous visits for each row. On a first visit, it uses V₀[j]
directly. On a repeat visit, it materializes the XOR expression into a temporary
row. It never writes a new 1 KiB value back to V during SMix2. [3]

This is an exact change in representation, rather than a digest-breaking
diagnostic or an oracle requiring future addresses. It makes the final never-read
updates cheap list operations. However, there are costs:

- It retains **685 KiB = 701,440 bytes** of X state per worker in the prototype,
  rather than reusing a small rolling state buffer.
- Repeat visits require extra XORs and temporary-row materialization.
- It adds per-row list heads and iteration links.
- It changes cache behavior, output-buffer locality and code generation.

The state storage is static thread-local research scratch. It is not an owned
production context API. The variant deliberately accepts only Tidecoin's fixed
parameters and also serializes only the final B chunk. A production integration
would require appropriate context ownership and broader operational validation.

The algebra removes immediate V updates, but none of the PWX multiplications.
This distinction explains why a large percentage of dead V writes need not
translate into a large fraction of total execution time saved.

## 5. Exact shortcut prototype: terminal output pruning

`backtrace_finish` replaces only the last SMix2 BlockMix with a terminal form:

- Read the selected V row, but omit its final update.
- Carry all sixteen PWX results through registers, but omit the first fifteen
  output-chunk stores.
- Execute the final Salsa and write the last 64-byte output chunk.
- Omit export of S pointers and the write cursor, which have no further reader.
- Serialize only the final 64 bytes back into B.

It preserves S writes **inside** the terminal BlockMix. Blindly dropping those
would be incorrect: a later PWX lookup, including another group in the same
round, can consume an earlier write. Terminal S state being unobserved does not
make every operation that updates it unobservable during execution.

The transformation is structurally small: only one of the 2,732 main BlockMix
calls is terminal. It does not support a dramatic speedup forecast. The extra
function, peeled loop and changed code layout can also outweigh saved work.
[3][6]

## 6. First-word decomposition: exact, but not dead work

Separate every 128-bit PWX pair into `(a,b)` and every S entry into corresponding
first/second words. Write F(x)=low32(x)×high32(x). Each pair step is:

```text
i = low32(a) AND 0x7ff0
j = high32(a) AND 0x7ff0
a′ = (F(a) + A0[i]) XOR A1[j]
b′ = (F(b) + B0[i]) XOR B1[j]
```

Arithmetic is modulo 2⁶⁴. Table writes store a′ into the A plane and b′ into
the B plane. Table rotation and write positions are deterministic and identical
for both planes. Therefore the A transition and address sequence are independent
of B during PWX. Given that address sequence, the B transition is determined.

The `split` evaluator computes all first-word steps of one three-round PWX call,
then all companion-word steps using the recorded addresses. It preserves group
order, write order within each plane, wrap and rotation. This handles collisions
exactly, without assuming they are absent. It passes the full-hash parity and
pruned-graph replay checks. [1][2]

This demonstrates a scheduling decomposition. It does **not** permit ignoring
the B plane: its values feed companion table reads and eventually Salsa, which
mixes the planes. Nor does it establish a CPU speedup from a 48 KiB A-plane layout.
The traced implementation retains the original table layout and is intentionally
not a performance kernel.

### Why the address mask does not immediately give a smaller persistent state

The next addresses use only bits 4–14 and 36–46 of a. That is fewer than all
64 bits, so the statement “the next address needs every output bit” is inaccurate.
Nevertheless, the next arithmetic step uses both complete 32-bit halves of a as
multiplication operands. Unused high bits of an immediate address can therefore
be needed in the following product and ultimately its address bits.

Computing product bits only through position 46 does not generally let us discard
high bits of either 32-bit input: both inputs contribute terms within that output
prefix. Deferred calculation may expose scheduling opportunities, but simply
retaining the 22 address bits is not a closed state representation.

Similarly, multiplication's symmetry F(u,v)=F(v,u) does not directly identify
equivalent PWX states. Exchanging the halves exchanges the two lookup indices,
which select different mutable tables and participate in different addition/XOR
positions. No quotient-state shortcut follows from that symmetry alone.

## 7. Validation and performance interpretation

Both optimized-C prototypes and their baseline passed **4,115 headers** against
the independent scalar reference before the P-core measurements: 19 fixtures and
4,096 deterministic varied inputs. Each binary processes the entire corpus in
one process, exercising scratch reuse across inputs. The E-core build repeats a
275-header differential check. Timed trials use changing nonces and compare
baseline/variant digest XORs. [4]

Measurements use GCC native optimized builds on the same i9-13980HX, pinned to
CPU 2 or CPU 16, with 2,048 hashes per timed run and five adjacent randomized
baseline/variant pairs for each prototype. Each process warms up for 64 hashes.
Raw times, execution order, compiler flags and source hashes are saved. These
are short single-worker trials on an unisolated host, not full-chip measurements.

The P-core median paired ratios were **0.965× for terminal pruning** and
**0.951× for lazy V**. Trial ranges were 0.928–1.026× and 0.539–1.012×
respectively. The especially poor lazy-V outlier and changing baseline rates
show why these should not be interpreted as precise kernel regression estimates.
They establish no convincing speedup. E-core results are recorded separately in
`results/backtrace-ecore.json`: median ratios were **1.023× for terminal pruning**
and **1.022× for lazy V**, with ranges of 0.930–1.143× and 0.938–1.074×.
Those small apparent gains relative to the spread do not establish a repeatable
improvement. The saved E-core timing pass ran after graph evaluation had stopped;
the host itself was not isolated. Larger controlled trials would be needed
before accepting a small speedup. [4]

Neither variant is selected for production. Differential success establishes
agreement on the tested inputs; the recurrence derivations explain why the
transformations preserve the algorithm. Neither alone substitutes for a complete
implementation review across supported execution environments.

## 8. What this rules out, and what it leaves open

This work directly tests the proposal to backtrace from the digest and discard
irrelevant intermediate computation. For the modeled execution, it finds no
removable PWX multiplication or S lookup. It finds dead stores, derives a way to
avoid a major class of them without foreknowledge, and validates two exact C
prototypes. That is a concrete result even without a mining-speed breakthrough.

It does not establish impossibility of:

- Algebraically combining several live operations into a cheaper computation.
- A smaller equivalent state representation not discovered by this slice.
- A faster schedule using the valid first-word decomposition.
- Input-dependent special cases whose detection costs less than the work saved.
- Cryptanalytic improvements to the SHA/HMAC boundary or to proof search itself.

Those are distinct research problems. A live dependency graph alone cannot
answer them. It would be inaccurate to claim either that a dramatic shortcut
has been found or that all exact shortcuts have been ruled out.

A follow-up note, [YESPOWER_SHORTCUT_ANALYSIS.md](YESPOWER_SHORTCUT_ANALYSIS.md),
examines the three forms (smaller state, conditional parallel execution,
composite rounds) with new measurements and states the concrete obstruction in
each case, including the 47-bit address-prefix dependency.

## Reproduction

Requirements are the external yespower sources described in the
[experiment bundle](../research/yespower/README.md), Python 3, GCC/G++, and
Linux `taskset`. The graph evaluator temporarily uses hundreds of MiB of RAM;
that is diagnostic storage, not the miner's scratch requirement.

```sh
python3 research/yespower/backtrace/run.py --reference "$YESPOWER_REFERENCE" --varied 8
python3 research/yespower/backtrace/bench.py --source "$YESPOWER_SOURCE" --reference "$YESPOWER_REFERENCE" --cpu 2 --hashes 2048 \
  --trials 5 --varied 4096 --output research/yespower/results/backtrace-cpu.json
python3 research/yespower/backtrace/bench.py --source "$YESPOWER_SOURCE" --reference "$YESPOWER_REFERENCE" --cpu 16 --hashes 2048 \
  --trials 5 --varied 256 --output research/yespower/results/backtrace-ecore.json
```

The CPU IDs are specific to the recorded machine. Builds and source rewrites
occur only in temporary directories. No internet source is needed for these
derivations or experiments.

## Sources and artifacts

1. [Independent evaluator and backward slice](../research/yespower/backtrace/trace.cpp),
   with [reference-checking runner](../research/yespower/backtrace/run.py).
2. [Per-header graph, replay and liveness results](../research/yespower/results/backtrace.json).
3. [Exact C rewrite definitions](../research/yespower/backtrace/variants.py), with
   [differential-validation and paired-timing runner](../research/yespower/backtrace/bench.py).
4. [P-core validation/timing results](../research/yespower/results/backtrace-cpu.json)
   and [E-core results](../research/yespower/results/backtrace-ecore.json).
5. Independent scalar specification (`yespower/opt/yespower-ref.c:183`),
   especially `pwxform`, `blockmix_pwxform`, `smix1`, `smix2` and final HMAC.
6. Optimized c implementation (`rust-yespower/depends/yespower/yespower-opt.c:410`),
   especially PWX macros, `blockmix_xor_save`, `smix2` and final HMAC.
