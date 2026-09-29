# Adaptive CPU tuning and the AArch64/Android target

Prepared 2026-09-15. Proposal. No production code or configuration was changed and
no new measurements were taken. Companion to the autotuning paragraph in
[PLAN.md](PLAN.md) M3 and the AArch64 note in PLAN section 1.

## 1. Scope

- **Autotuning**: select and maintain a worker layout under warm load so that
  accepted work per second is maximized; an efficiency profile maximizes H/J.
  Synthetic H/s is a diagnostic, not the success metric ([PLAN.md](PLAN.md)).
- **Mobile/AArch64**: current porting state, order of work, and clearly labelled
  estimates. These estimates are not measurements.

## 2. Autotuning

### 2.1 Objective and metric

Score candidates by accepted work per second where a pool is connected, otherwise
by a bounded warm synthetic run. H/J is the secondary profile. Retuning must not
run during live job churn; a miner with pending shares keeps its layout until a
quiet point.

### 2.2 Signals available without privileges (Linux)

| Signal | Source | Use | Caveat |
| --- | --- | --- | --- |
| Cores/SMT | `/sys/devices/system/cpu/cpu*/topology/thread_siblings_list`, `core_cpus_list`, `core_id` | Distinguish physical cores from SMT siblings | None for normal users |
| Caches | `cache/index*/{level,type,shared_cpu_list,size}` | Group workers by shared cache | L3 often one instance |
| NUMA | `node*/cpumap`, `distance` | Local scratch and workers | Single-node laptops |
| Hybrid class | `cpu_capacity` (or frequency table) | Separate P and E core sets | Fallback: cpufreq max |
| Frequency | `cpufreq/scaling_{cur,min,max}_freq`, `scaling_governor` | Detect throttling and governor limits | `cpuinfo_cur_freq` may need root |
| Thermal | `/sys/class/thermal/thermal_zone*/{type,temp}`, hwmon `coretemp`/`k10temp` | Step down before skin/OS limits | Zone names are machine-specific |
| Throttle counters | `cpu*/thermal_throttle/{core,package}_throttle_count` | Confirm real throttling, not load noise | Not on all kernels |
| Power | `powercap/intel-rapl:*/energy_uj`, constraint files | H/J scoring | Permissions vary; often root/udev |
| Cpuset | `/proc/self/status` `Cpus_allowed_list`, cgroup v2 `cpuset.cpus.effective` | Never propose a forbidden CPU | Container/Android restrictions |
| Pressure | `/proc/pressure/cpu` | Correlate lost throughput with system load | Optional kernel feature |

Read failures are non-fatal: the tuner degrades to topology plus achieved H/s.

### 2.3 Knobs

- Worker count, capped by allowed CPUs.
- Affinity sets: P physical, P+SMT, E only, P+E physical, all logical. On ARM:
  big cluster, big+mid, all. IDs come from topology, never assumed ranges.
- Affinitize before scratch allocation and first touch.
- Optional pair kernel only on underutilized cores, never on full-load E-cores
  ([YESPOWER_CPU_LATENCY.md](YESPOWER_CPU_LATENCY.md)).

### 2.4 Calibration protocol

- Bounded candidate set (tens, not hundreds); each probe 1-3 seconds warm.
- Deterministic randomized order, paired repetitions; require repeated wins
  beyond the observed spread before promoting.
- Cache the chosen layout by fingerprint: CPU model and microcode, topology,
  cpuset, kernel, backend build and C flags, AC/battery and power limits.
- Always retain a conservative fallback layout.
- Today, each configuration must run in its own process: the upstream C TLS
  allocator has no destructor ([README.md](../README.md)).

### 2.5 Control loop

- Sample every 5-30 seconds: achieved H/s per worker against calibration,
  frequency ratio, throttle-counter deltas, thermal headroom.
- Step down on sustained degradation (hysteresis, minimum dwell time), re-probe
  at the next quiet point. Log every decision with the triggering signal.
- Oscillation is treated as a bug: one state change per dwell window.

### 2.6 Android notes

- SELinux may hide governor and some frequency nodes; RAPL does not exist.
  Fall back to topology, thermal zones, and achieved H/s.
- big.LITTLE/DynamIQ: pin to the performance cluster; never count little cores
  as equivalent capacity.
- No SMT on ARM. Worker count equals chosen physical cores.
- Sustained skin-temperature limits usually dominate short-burst tuning.

### 2.7 Dependencies

- Owned yespower contexts (M2) so worker sets can change without leaking
  ~2.10 MiB per thread.
- `stats` module (M3) for accepted-work scoring.
- `src/benchmark.rs` already provides the warm, disjoint-range measurement shape.

## 3. AArch64/Android target

### 3.1 Current state

- `yespower-opt.c` has SSE2/AVX paths and a generic scalar fallback
  (`#else` branch, around line 603). AArch64 compiles the scalar fallback:
  correct, but without the two-products-per-pair 128-bit mapping.
- [PLAN.md](PLAN.md) section 1 defers AArch64/NEON until correctness and CPU
  performance gates.

### 3.2 Estimated hashrate (estimates, roughly +/-2x)

Derived by scaling the measured i9 values (P-core ~1,378 H/s, E-core ~805 H/s;
about 260 and 190 H/s/GHz with SSE2) to ARM scalar code (30-50% per-GHz loss),
40-70% sustained clocks. Verify on device before believing any of it.

| Class | Example SoCs | Burst, whole device | Sustained |
| --- | --- | ---: | ---: |
| 4x A53 | Snapdragon 439, Helio P22 | 100-300 H/s | 50-150 H/s |
| 4x A73/A76 | Snapdragon 665, 720G | 0.3-0.6 kH/s | 0.15-0.3 kH/s |
| 2020 flagship | Snapdragon 865, Exynos 990 | 0.8-1.5 kH/s | 0.4-0.8 kH/s |
| 2022-23 flagship | Snapdragon 8 Gen 2, Dimensity 9000 | 1.5-2.5 kH/s | 0.7-1.3 kH/s |
| 2024-25 flagship | Snapdragon 8 Gen 3/Elite, Dimensity 9300 | 2-3.5 kH/s | 1-1.8 kH/s |

### 3.3 Optimization order

1. **NEON PWX**: `vmull.u32` reproduces the SSE2 two-products-per-pair shape;
   a NEON Salsa20/2 path follows the same lane mapping. This is the main
   available per-core win.
2. **Affinity and cluster pinning** as in 2.3/2.6.
3. **Re-test the pair kernel on mobile**: it is an exact 1.36-1.39x on an idle
   core, and shared cluster caches may change the whole-chip result.
4. **Do not port SHA extensions first**: SHA, PBKDF2 and final HMAC are under
   0.35% of runtime.
5. **Thermal autotune** per section 2.

### 3.4 Build and distribution

- Cross-compile for `aarch64-linux-android` with the NDK; the `cc` crate picks
  up NDK clang. Termux can build on device but slowly.
- Portable releases need runtime feature dispatch; never ship
  `target-cpu=native` assumptions.
- Application route: foreground service plus wake lock; Google Play policy
  blocks on-device mining apps, so sideload/F-Droid/Termux/web are the channels.
- Expect heavy battery drain, heat and battery wear; check pool payout minimums
  before recommending phone mining as profitable.

### 3.5 Verification

- The same 275-header parity corpus and the benchmark protocol used on x86.
- On-device thermal soak with throttle counters and sustained H/s reporting.
