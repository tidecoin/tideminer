# Metal GPU worker: measurements (Apple M3 Max, 2026-09-28/29)

The GPU worker (`--gpu`, macOS) runs yespower's smix core in Metal
([src/gpu/yespower.metal](../src/gpu/yespower.metal)); SHA-256 / PBKDF2 / HMAC stay on
the CPU (`tidecoin_yespower::prepare` / `finish`). Every kernel version below was
bit-exact: the known Tidecoin header/hash pair plus up to 128 random headers against
the Rust kernel.

Machine: 14" MacBook Pro, M3 Max (10 P + 4 E CPU cores, 30-core GPU, 36 GB).

## What counts

yespower is memory-hard and the Apple GPU shares the SoC's memory system and power
budget with the CPU. A GPU hash streams 2 MiB of V through the system-level cache, so
GPU hashing evicts the CPU miner's data. The only number that matters is **CPU + GPU
together, heat-soaked**, against the CPU alone: GPU-only speed misled twice.

## Kernel versions (GPU only, 120 hashes in flight)

| Version | H/s | Notes |
| --- | ---: | --- |
| v1: straight port, one thread per hash | ~1,100 | S-box read, compute, write per step: 4 serial memory trips per round |
| v2: round's 8 S-box reads issued together | 1,154 | exact conflict check (a read of an entry written earlier in the round, ~1.5% of rounds) falls back to the sequential order |
| v3: warp-8, 8 SIMD lanes per hash | 1,742 | each lane owns one 64-bit word; conflict rounds replayed by all 8 lanes |
| v3 + next sub-block's inputs loaded ahead | 2,150 | the random V_j line arrives during pwxform |

A diagnostic with every S-box read forced to hit (wrong hashes) ran 2.5x faster than
v2: about 60% of a v2 hash was S-box latency and 40% exposed instruction latency,
which is what warp-8 removed.

Rejected: V_j look-ahead in v2 (-3%: registers), the whole V_j block up front (-7%),
two sub-blocks ahead (within noise of one), double buffering (+0.4%; kept, harmless).

## CPU + GPU (heat-soaked, 2 mirrored rounds, 25 s each)

| GPU | CPU alone | CPU + GPU | Change |
| --- | ---: | ---: | ---: |
| v1, 1536 hashes | 17.7k / 16.8k | 11.5k / 10.2k | -35% / -40% |
| v1, 480 | | 15.4k / 16.4k | -13% / -2% |
| v2, 120 | 17.9k / 17.3k | 18.9k / 18.0k | +5.7% / +3.9% |
| v3, 120 | 18.3k / 17.6k | 19.6k / 19.0k | +7.1% / +7.9% |
| v3 + look-ahead, 180 | 19.1k / 18.3k | 21.1k / 20.0k | +10.3% / +9.2% |

Also measured: GPU instead of the E cores (10 P cores + GPU) never beat all 14 cores
alone; one hash per threadgroup made the GPU 16% faster alone but the total lower
(more active SIMD groups, more pressure on the CPU); fewer GPU cores (threadgroups of
128 or 256) did not reduce the CPU's loss (~1.5k H/s), which points at memory traffic,
not GPU power. Session-to-session variation of the totals is about ±2%.

A mock-pool run (1 CPU thread + GPU at 180) credited 4.87 kH/s from 2,229 valid
shares, 0 invalid, consistent with CPU ~2k + GPU ~2.8k.

## Why not WebGPU

WGSL has no 64-bit integers, and the browser engines add their own overheads; see
[the WASM notes](../crates/yespower/README.md) for how far browsers get on the CPU.
