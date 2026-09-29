# Why Tidecoin pools find blocks in bursts: block-timing math

This note investigates the observation that Tidecoin pools seem to find blocks or
shares much more often at some times than others. The short answer is that the
chain itself has a strong, measurable, mathematically explained modulation:
**Tidecoin mainnet difficulty is a five-day-lagged feedback loop, while network
hashrate changes much faster, so block rate swings by up to about 8x between
retarget windows and remains overdispersed even inside a window.** None of this
is a yespower weakness, and none of it changes per-hash work.

The analysis in this note uses the local mainnet chain snapshot in
a Tidecoin mainnet block snapshot (height 1,853,812, December 2024) parsed by
`research/tidecoin/chain_timing.py`. It measures timing only; proof-of-work
validity is not rechecked.

## Findings

1. **The chain under yespower retargets every 7,200 blocks (5 days) with the
   legacy Bitcoin rule.** Across 1,853,813 active-chain blocks there are 257
   `nBits` changes, at a median spacing of exactly 7,200 blocks. Tidecoin
   mainnet has AuxPoW disabled, so yespower is used for every height and the
   post-AuxPoW per-block retarget never applies.
2. **Five-day block production is wildly non-stationary.** The ratio of actual
   window duration to the 5-day target has median 0.97 but spans **0.123 to
   7.694**; 6.2% of windows are more than 2x fast and 7.8% more than 2x slow.
   Recent history is calmer but still significant: 2024 windows span 0.41 to
   2.53, and the last 40 windows span 0.57 to 2.36.
3. **The retarget oscillates.** Window-ratio lag-1 autocorrelation is **-0.188**.
   A fast window is followed by slower ones and vice versa: after a window
   faster than 2x (`r < 0.5`), the next window's median ratio is 1.70 (and
   3.25 after the fastest quartile); after a slow window (`r > 2`), the next
   median is 0.87-0.94. This is the classic delayed, saturating feedback loop
   overshoot.
4. **Block arrivals are overdispersed, not exponential.** The inter-block
   interval coefficient of variation is **1.73** where a constant-rate Poisson
   process gives 1.0. Hourly block-count variance/mean is **30.3** raw. Testing
   only short-timescale fluctuation at fixed difficulty (each hour against the
   containing window's own mean rate) still gives a chi-square per degree of
   freedom of **10.6** over the whole chain and **3.6** in 2024, versus 1.0 for a
   constant-rate process; the corresponding within-window relative rate standard
   deviation is about 40% (whole chain) and 21% (2024).
5. **There is a small diurnal component.** After removing daily totals, the
   block-share peak-to-trough amplitude is about **0.33 percentage points** of a
   day (about 8% relative), peaking near 15:00 UTC and dipping near 22:00 UTC.
6. **The digest has no gross per-nonce or per-time bias.** A one-million-hash
   uniformity probe of the most significant digest byte gives chi-square 240.6
   (nonce sweep) and 250.0 (time sweep) against 255 degrees of freedom, with the
   adjacent-hash equality rate equal to the 1/256 expectation. This rules out
   any simple nonce/time structure large enough to explain the burstiness
   (roughly few-percent per-bin at this sample size).
7. **No yespower shortcut follows.** The chain-level explanation is consensus
   math (difficulty lag), not a property of the hash function. The miner's
   per-hash cost is unchanged, and none of the header/job reuse results apply.
   The measurable opportunity is economic (difficulty-lag arbitrage), not
   computational.

## 1. The yespower chain's retarget rule

From the local consensus source (`src/pow.cpp`, `src/kernel/chainparams.cpp`):

| Parameter | Value |
| --- | --- |
| Mainnet AuxPoW activation | disabled |
| PoW for all mainnet heights | yespower |
| Target spacing | 60 s |
| Target timespan | 5 days (432,000 s) |
| Retarget interval | 7,200 blocks |
| Clamp on measured timespan | 1/4 to 4x |
| PoW limit | `0x01ffff...` compact |

The legacy adjustment is

```text
actual   = time_of_last_block_in_window - time_of_first_block_in_window
factor   = clamp(actual / target_timespan, 1/4, 4)
target  *= factor                 # target, not difficulty
```

and difficulty is the inverse of target. The post-AuxPoW per-block rule (17-block
average, -32%/+16%) exists in the code but applies only to test/regtest chains
whose PoW is scrypt, so it is irrelevant to yespower mining.

The parser confirms the rule is live: `nBits` is constant for exactly 7,200
blocks at a time over the whole 1.85M-block snapshot.

## 2. Why the block rate swings so much

Let `D_k` be difficulty during window `k`, `H_k` the average network hashrate in
that window, and `r_k` the observed window duration divided by 5 days. Since the
expected interval at difficulty `D` and hashrate `H` is `D * 2^32 / H`,

```text
r_k = D_k * 2^32 / (60 * H_k)
```

The retarget sets `D_{k+1} = D_k / clamp(r_k)`. Substituting gives the exact
one-step relation

```text
r_k = (r_{k-1} / clamp(r_{k-1})) * (H_{k-1} / H_k)
```

Inside the clamp the first factor is 1, so **the observed window ratio is the
inverse hashrate change across the window boundary**. Deviations of `r_k` from 1
are pure hashrate volatility that the five-day lag cannot track. Outside the
clamp, the residual `r_{k-1}/clamp(r_{k-1})` persists and compounds, which is
why the extremes reach 8x instead of 4x.

This is a first-order feedback loop with a one-window delay and saturation. Its
signature is mean-reverting oscillation, which the data show:

| Previous window ratio | n | Next ratio median | Next ratio mean |
| --- | ---: | ---: | ---: |
| < 0.25 | 4 | 3.25 | 3.42 |
| 0.25-0.50 | 12 | 1.70 | 1.90 |
| 0.50-0.80 | 51 | 1.02 | 1.29 |
| 0.80-1.25 | 141 | 0.98 | 1.03 |
| 1.25-2.00 | 28 | 0.91 | 0.98 |
| 2.00-4.00 | 15 | 0.87 | 0.78 |
| > 4.00 | 5 | 0.94 | 0.87 |

Lag-1 autocorrelation of `r_k` is -0.188; lag-2 is -0.002; lag-3 is +0.073.

### Window-ratio distribution by year

| Year | p05 | median | p95 | min | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| 2021 | 0.38 | 0.88 | 2.85 | 0.23 | 7.69 |
| 2022 | 0.28 | 1.07 | 5.20 | 0.12 | 6.61 |
| 2023 | 0.59 | 0.98 | 1.93 | 0.35 | 3.87 |
| 2024 | 0.59 | 0.98 | 1.74 | 0.41 | 2.53 |
| last 40 windows | 0.62 | 0.93 | 1.46 | 0.57 | 2.36 |

The phenomenon was extreme during 2021-2022 (likely multipool hashrate hopping on
a young chain) and has calmed, but a 2x swing over five days is still common and
the last window in the snapshot is 1.45 (slow).

## 3. Overdispersion: block arrivals are not simple Poisson

If hashrate and difficulty were constant, inter-block intervals would be
exponential with coefficient of variation 1.0 and the Fano factor of counts in
fixed windows would be 1.0. The measured whole-chain values are much larger:

| Statistic | Value |
| --- | ---: |
| Inter-block interval mean | 68.0 s |
| Median | 36 s |
| Standard deviation | 117.7 s |
| Coefficient of variation | **1.73** |
| p10 / p90 / p99 | 5 s / 154 s / 504 s |
| Max interval | 9,907 s |
| Fraction under 30 s | 44.5% |
| Fraction over 120 s | 14.8% |
| Hourly count variance/mean, raw | **30.3** |
| Within-window hourly chi-square per df, whole chain | **10.6** |
| Within-window hourly chi-square per df, 2024 | **3.6** |

The raw variance/mean of 30.3 mostly reflects the multi-day difficulty state.
The within-window chi-square divides each hour's count by the containing
window's own measured mean rate, so it tests only short-timescale fluctuation at
fixed difficulty; a constant-rate Poisson process gives 1.0. The measured 10.6
(whole chain) and 3.6 (2024) show that the local rate fluctuates substantially
even inside a difficulty window. Writing the count model as a Cox process
`count ~ Poisson(e)`, `e = 60/r_k`, the dispersion `1 + e * sigma^2` gives a
within-window relative rate standard deviation of about **40%** over the whole
chain and **21%** in 2024. For the interval distribution,

```text
CV_interval^2 = 1 + Var(lambda) / E[lambda]^2
```

so `CV = 1.73` implies the local hashrate/rate has standard deviation about
**1.4x its mean**. That is a very bursty process: long dry spells followed by
clusters of blocks.

A small diurnal component survives removing daily totals: peak-to-trough
amplitude 0.0033 of a day (about 8% relative), peak near 15:00 UTC, trough near
22:00 UTC, with a maximum per-hour z-score of about 5.2 across days.

### Digest uniformity probe

The burstiness above is chain-level. A separate probe checks whether the hash
output itself has nonce or time structure: `research/yespower/bias.c` hashes a
fixed header prefix while varying only the nonce field, then only the time field,
and histograms the most significant digest byte (byte 31, the first byte the
target comparison reads). One million hashes per mode, eight processes:

| Mode | Hashes | Top-byte chi-square (df 255) | (field & 15) x top-byte (df 4095) | Adjacent-hash equality (expected 1/256) |
| --- | ---: | ---: | ---: | ---: |
| nonce | 1,048,576 | 240.6 | 3996.2 | 0.00391 |
| time | 1,048,576 | 250.0 | 4115.0 | 0.00386 |

All statistics sit inside the noise of a uniform distribution. The test is not a
proof of cryptographic quality, but at this sample size it bounds per-bin
structure to a few percent, far below the 100%+ block-rate swings, so the chain
effect cannot be a hash bias.

## 4. What this does and does not imply for the miner

**Hash kernel:** nothing. The per-hash work of yespower is unchanged. The
shortcut analysis in [YESPOWER_SHORTCUT_ANALYSIS.md](YESPOWER_SHORTCUT_ANALYSIS.md)
still holds, and no header/job reuse follows from block timing.

**Pool share rate:** a worker at hashrate `h` and share target `T_s` submits
shares as a Poisson process with rate `h / T_s` (in hashes per share units). That
rate depends on the worker's own hashrate and the pool's share difficulty, not on
network difficulty. So a *worker's* share count per second should be stationary
apart from hardware behavior and pool difficulty changes. Observed bursts at the
pool level therefore come from one of:

1. The network block-finding bursts above (visible if the pool reports blocks or
   block candidates rather than shares).
2. Pool hashrate changes: other miners joining or leaving.
3. Variable-difficulty control: pools often adjust share difficulty to hit a
   target share period. Downward adjustments produce temporary bursts; an
   aggressive controller can oscillate.
4. The local machine: frequency, thermal and background-load changes. The CPU
   latency study measured whole-chip per-core throughput varying by 30% or more
   between sessions.
5. Rehashed identical headers: if a pool re-sends the same template bytes and the
   miner restarts its nonce scan, the same winning nonces are found again at the
   same scan offsets, producing periodic-looking bursts and duplicate
   submissions. Every job should change at least extranonce, time or version.
6. Overlapping work assignment: if two workers hash the same `(header, nonce)`
   tuples (overlapping ranges or reused extranonce), a win is submitted by both
   at once and the pool sees a burst plus duplicates.

A concrete diagnostic when pool data is available: log share submission times and
hashes done between shares, then compute the dispersion index (variance/mean of
counts per fixed interval) and the autocorrelation. Poisson gives an index near
1; hardware or vardiff effects give a larger index and a smooth autocorrelation;
network block bursts correlate with the current window ratio computed from recent
block timestamps.

**Economics (difficulty-lag arbitrage):** within a window, the expected Tidecoin
per hash for solo or PPLNS mining is proportional to `1 / D_k`, and `D_k` is
known publicly at the window start from the retarget rule. The retarget factor
`clamp(r_k)` sets the next window's difficulty relative to the current one.
Across 2024 the factor spans 0.41 to 2.53 (p05-p95: 0.59 to 1.74), so relative
income per hash can differ by roughly 3x between the 5th and 95th percentile
windows and about 6x at the extremes; historically it was far larger. Pointing
portable hashrate at Tidecoin during low-difficulty windows is an
expected-value strategy, but it is public information, requires fast hashrate
movement, and is independent of any kernel optimization. Pooled PPS payouts damp
or remove it.

## 5. Mathematical model summary

```text
Window k:
  r_k   = observed_duration_k / target_timespan
  D_{k+1} = D_k / clamp(r_k, 1/4, 4)
  r_k   = (r_{k-1}/clamp(r_{k-1})) * H_{k-1}/H_k

Block process:
  lambda(t) = H(t) / (D(t) * 2^32)
  E[interval] = 1 / lambda
  CV_interval^2 = 1 + Var(lambda)/E[lambda]^2
  Var(count over window W) / E[count] = 1 + W * Var(lambda)/E[lambda]

Retarget loop: one-sample delay + saturation -> oscillatory mean reversion
```

The key point is that the "certain times" are a deterministic consequence of a
lagged, saturated controller driven by a volatile hashrate. They are not a
yespower property, not a hidden nonce bias, and not usable to reduce hashing
work.

## Reproduction

```sh
python3 research/tidecoin/chain_timing.py --blocks "$TIDECOIN_BLOCKS" \
  --output research/tidecoin/results/chain-timing.json

python3 research/yespower/bias_test.py --source "$YESPOWER_SOURCE" \
  --output research/yespower/results/bias.json
```

The chain script reads `blk*.dat` records directly from
the directory supplied with `--blocks`, links headers by previous
hash, selects the maximum-work chain, and writes interval, retarget-window,
time-of-day, detrended-diurnal and per-year statistics. The bias runner builds
`research/yespower/bias.c` against the vendored optimized C, runs one process per
listed CPU and merges the histograms. Saved outputs are
`results/chain-timing.json` and `results/bias.json`.

## Limits

- The snapshot ends at height 1,853,812 in December 2024. Current network
  behavior may differ, especially after any hashrate regime change.
- Only timing was parsed; block validity and PoW were not rechecked.
- Block timestamps can be adjusted by miners within consensus limits, which adds
  a small amount of noise to all time-based statistics.
- The `r_k = H_{k-1}/H_k` relation assumes the clamp does not bind; outside the
  clamp the residual factor is stated explicitly.
- The parser's chain selection uses cumulative work but does not validate
  difficulty transitions.
