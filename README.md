# tideminer

Tidecoin-only CPU miner for Stratum V1 pools, written in Rust.
Fixed PoW: **yespower 1.0, N=2048, r=8, no personalization, 80-byte header**.
No algorithm selector, configurable N/r, multi-coin machinery or developer fee.

## Install

macOS (Apple Silicon and Intel) and Linux (x86-64, arm64):

```sh
curl -fsSL https://github.com/tidecoin/tideminer/releases/latest/download/install.sh | sh
```

Windows (x86-64), in PowerShell:

```powershell
irm https://github.com/tidecoin/tideminer/releases/latest/download/install.ps1 | iex
```

The scripts ([install.sh](install.sh), [install.ps1](install.ps1)) download the release
archive for the machine, verify the release's `SHA256SUMS` and run the downloaded
binary's `self-test` before replacing an existing installation. They install without
administrator rights (`~/.local/bin`, or `%LOCALAPPDATA%\Programs\tideminer` added to
the user PATH) and can be re-run to update. On Linux/macOS, a running miner keeps
using the old binary until you restart it with your usual settings. On Windows,
stop the miner before updating, then start it again. `TIDEMINER_VERSION=v0.2.1` pins a release;
`TIDEMINER_INSTALL_DIR` changes the target. The download URLs need the releases to be
public.

- macOS: one universal binary, signed ad hoc (no certificate). Apple Silicon requires a
  valid signature, and files fetched with curl are not quarantined, so Gatekeeper does
  not interfere. A binary downloaded in a browser is quarantined; clear it with
  `xattr -d com.apple.quarantine tideminer`.
- Linux: static musl binaries, independent of the distribution's C library.
- Windows: Microsoft Defender often flags coin miners as potentially unwanted and may
  remove `tideminer.exe`; allow its folder with
  `Add-MpPreference -ExclusionPath "$env:LOCALAPPDATA\Programs\tideminer"` (administrator).
  Windows uses one worker per logical CPU, scheduled by the OS (no P/E-core detection or
  pinning yet), keeps the PC awake while mining and saves tuning to `%LOCALAPPDATA%`.

Releases are built by [CI](.github/workflows/release.yml) when a `v*` tag is pushed:
Linux x86-64 and arm64 (musl, via cargo-zigbuild; arm64 smoke-tested under QEMU), the
macOS universal binary and Windows, each running `self-test`, then `SHA256SUMS` and a
GitHub Release with the install scripts. [CI](.github/workflows/ci.yml) tests every
push on Linux, macOS and Windows.

## Build

Requires Rust 1.94.1 or newer. Hashing uses this repository's pure-Rust kernel,
[tidecoin-yespower](crates/yespower/README.md): bit-exact with Openwall's reference,
at parity with the optimized C on one hash per thread, and able to interleave two
hashes per thread (`--lanes`). No C compiler is needed, so macOS, Windows and ARM
builds are plain `cargo build --target ...`.

The Openwall C kernel (crates.io `rust-yespower = "=0.3.0"`) stays available
with `--features c-kernel` (needs a C compiler)
and is the independent oracle in the tests: every share in the mock-pool tests is
re-hashed with it. The pool validating with a different implementation than the
miner hashes with is a deliberate cross-check.

```sh
cargo build --release --locked
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

### macOS (Apple Silicon)

The same `cargo build --release --locked`; no extra flags (target-cpu, fat LTO
and PGO all measured neutral or slower). On AArch64 the kernel uses its `Aarch64`
backend (see [simd.rs](crates/yespower/src/simd.rs)): 64-bit integer code with V_j
prefetch and register-offset S-box loads, which measured faster than NEON.

An M3 Max (14" MacBook Pro) with the default `--layout all` (10 P + 4 E workers)
bursts at about 20 kH/s and settles near 18 kH/s once heat-soaked; fewer threads
measured lower even heat-soaked. Auto lanes stay at 1 on Apple Silicon: one lane's
96 KiB of S-boxes fits a P core's 128 KiB L1D and a second lane evicts them
(-36 to -43%). macOS cannot pin threads, so workers ask for a core class through
QoS instead; `--cpus`/`--cpu-affinity` are Linux-only, and `--max-temp` has no
sensor to read (macOS throttles by itself). While mining, benchmarking or tuning,
tideminer holds a power assertion so the Mac does not idle-sleep (the display
still may; closing the lid still sleeps); see `pmset -g assertions`.

#### GPU (`--gpu`)

macOS builds include a Metal GPU worker, off unless `--gpu` is given. It runs
yespower's memory-hard core on the GPU ([src/gpu/yespower.metal](src/gpu/yespower.metal):
8 SIMD lanes per hash, BlockMix inputs loaded one sub-block ahead) next to the CPU
workers; the CPU does SHA-256 / PBKDF2 before and HMAC after, and re-hashes every
share before submitting it. The GPU checks the known header/hash pair at start and
every minute; a failed check or a GPU/CPU disagreement stops the GPU worker with a
warning while the CPU workers go on. A GPU batch has a 10-second completion
deadline; timeout disables that worker without reusing its in-flight buffers.
In GPU-only mode, a worker failure stops the miner instead of leaving it idle.

The GPU shares the chip's memory system and power with the CPU, and yespower is
memory-hard: each GPU hash streams 2 MiB through the system cache and evicts the CPU
miner's data. So only a light GPU load adds hashes. M3 Max (30-core GPU), heat-soaked,
all 14 CPU cores plus the GPU: 120-180 hashes in flight added 7-10% in total; 480 or
more lowered the total by 13-40%. Details: [docs/METAL_GPU.md](docs/METAL_GPU.md).

```sh
tideminer tune --gpu                      # measure this Mac's best GPU load; saved
tideminer -o POOL:PORT -u ADDRESS --gpu    # automatically uses matching saved tuning
tideminer -o POOL:PORT -u ADDRESS --gpu --gpu-hashes 150   # or set the load yourself
tideminer -o POOL:PORT -u ADDRESS -t 0 --gpu              # GPU only, CPU left free
```

GPU only (`-t 0 --gpu`) ran 2.8 kH/s at about 10 W on the M3 Max and leaves every CPU
core to other work. Under a sustained full CPU + GPU load a 14" MacBook Pro reaches
heavy thermal pressure: the GPU's ~10 W then comes out of the P cores' clock (measured
~2.1 GHz instead of up to 4.06), and the CPU yields about 3x more hashes per watt than
the GPU, so on a thermally limited laptop compare `--gpu` against CPU only over long
runs before keeping it.

Without tuning, `--gpu` uses 6 hashes in flight per GPU core. `tune --gpu` compares
that with the GPU off and at 2 to 10 per core (and CPU variants), so it can also
conclude that a Mac (say a fanless one) mines best without the GPU. Other flags:
`--gpu-threadgroup` (threads per threadgroup, 8 per hash; 32 measured best). The
GPU's rate is reported separately (`GPU` in the hashrate lines and `bench` output).

## Mine

cpuminer-style, no subcommand needed (`tideminer mine ...` works too):

```sh
./target/release/tideminer -o stratum+tcp://POOL_HOST:PORT -u TDC_ADDRESS.rig
./target/release/tideminer -a yespowertide -o na.rplant.xyz:17059 --tls -u TDC_ADDRESS
```

- Pools: `stratum+tcp://`, `stratum+tls://` (also `+ssl`, `+tcps`), or `HOST:PORT` with
  `--tls`. TLS verifies the certificate and hostname against the Mozilla roots; `--cert
  ca.pem` trusts a pool's private CA. There is no insecure mode. `-o` may be repeated
  for failover. Password defaults to `x` (`-p`, `-O USER:PASS`, `TIDEMINER_PASSWORD`).
  Pool-specific password options pass through unchanged: for rplant, use
  `-p 'webpassword=YOURPASS'`. No separate `--webpassword` flag is needed; the pool
  interprets this value, and protocol logging hides the entire password field.
- cpuminer flags accepted: `-a` (Tidecoin names only; `-a yespower` needs `-N 2048 -R 8`),
  `-N`/`-R`/`-K`, `-t`, `--cpu-affinity MASK`, `--cpu-priority 0-5`, `-q`, `--no-color`,
  `-D` (debug), `-P` (protocol dump, password hidden), `-r`, `--retry-pause`, `-T`,
  `--time-limit`, `--benchmark`.
- CPU placement: mining automatically uses a matching saved `tideminer tune` result.
  Without one, every allowed logical CPU gets one pinned worker hashing
  one nonce at a time; that measured best for sustained throughput on the hybrid
  i9-13980HX. `-t N` takes CPUs in priority order (P cores, their SMT siblings, then E
  cores spread over L2 clusters); `--layout physical|performance|efficiency` and
  `--cpus 0-15,16,20` restrict it; `--nice 10` keeps a desktop responsive.
  `--lanes auto` (default) makes a worker interleave two hashes where its core would
  otherwise idle between S-box loads: no busy SMT sibling, and on E cores no other
  worker in the L2 cluster, while the hashes in flight stay within the logical CPU
  count. So `-t 8 --layout performance` runs 8 x 2 hashes on the 8 P cores, and the
  all-CPU default stays at one hash per worker. `--lanes 1|2` forces a count.
- `--autotune` runs measurements only when no matching saved result exists.
  Without this flag, a cache miss uses the built-in rules immediately.
  Within your `-t`, `--layout`,
  `--cpus` and `--lanes`, it compares the configurations that differ (lanes per core,
  a P core's SMT sibling versus a second lane, E cores versus SMT siblings) for
  ~15-40 s before connecting, in mirrored rounds, and prints a ranked table. A
  challenger replaces the rule-based choice only if it wins by more than the measured
  noise and no other program loaded the measured CPUs.
- `tideminer tune [--minutes 5]` is the longer offline version: it also varies the
  thread count (P cores, SMT siblings, E cores per L2 cluster), screens every
  configuration briefly, then measures the finalists in longer mirrored rounds after
  a 30 s warm-up. It prints H/s, spread, temperature and, where readable, watts and
  hashes per joule (`sudo tideminer tune` on Linux, whose CPU energy counter is
  root-only; unplugged laptops fall back to battery discharge), the fastest and the
  most efficient configuration as mining flags, and saves the fastest to
  `~/.cache/tideminer/tune.json`. Mining with the same machine and CPU/GPU limits
  then uses it automatically, without measuring or needing `--autotune`.
  Profiles are miner-version-specific; after an update, run `tune` again or use
  `--autotune` for a quick comparison.
  Different limits do not reuse that profile; malformed or invalid cached results
  fall back to the rules (or quick tuning with `--autotune`).
  `bench` and `topology` continue to use their explicit settings and built-in rules.
  `tune --list` shows the candidates; `tune --quick` runs the
  `--autotune` comparison alone.
- `--max-temp C` (Linux, coretemp/k10temp): keeps the CPU package under C by parking
  workers (lowest priority first) and adding them back once 3 °C cooler, instead of
  cpuminer's all-or-nothing pause, which flaps on hardware that cools 15 °C within a
  second of stopping. Laptops that boost to a thermal target sit near it under any
  load (this i9: ~92 °C during turbo, ~82 °C sustained), so use it there as a safety
  net just above that point (e.g. 90-95); a lower limit mostly idles the miner.
- Pool recovery: `--submit-timeout` defaults to **5 seconds** from queueing a
  share until its response. An unanswered share ends the session even while jobs
  arrive. `--timeout` / `-T` is a separate **90-second total idle watchdog**, with
  a ping halfway through; pools that ignore pings can legitimately be quiet
  between jobs. Increase either limit for a pool that needs longer. The first
  recovery attempt after an established session fails is immediate. Repeated
  failures back off with jitter, capped by `--retry-pause`, and each failure
  rotates to the next configured pool. A session lasting a minute resets the
  backoff. Unconfirmed shares are discarded when the session ends; their outcome
  is unknown and they are never replayed into a different session.
- Socket teardown interrupts pending TCP/TLS I/O. Stalled writes have a 5-second
  watchdog; TCP keepalive is enabled, and Linux also bounds unacknowledged data
  with `TCP_USER_TIMEOUT`. DNS, TCP connect, and TLS handshake share a 10-second
  connection budget; subscribe/authorize have a separate 10-second budget.
  After authorization, the first valid job must arrive within 10 seconds;
  unrelated pool messages cannot keep an idle worker waiting indefinitely.
  OS DNS calls run in a fixed two-thread resolver with a bounded queue: callers
  time out even if the OS resolver stalls, without spawning unbounded threads.
  Pool frames and inbound/outbound queues are bounded; overload reconnects.
- Mining output uses a bounded background writer. A blocked terminal or log pipe
  cannot stop share handling or watchdogs. Under sustained output stalls log
  lines may be dropped; the logger reports the drop count when output resumes.
  Log lines are capped at 8 KiB, including protocol dumps. Shutdown makes a
  bounded best-effort flush rather than waiting indefinitely for output.
- Output: startup banner (kernel, CPU, workers, pool), colored event lines (connect,
  new block with height, difficulty changes, accepted/rejected/stale shares with share
  difficulty, hashrate and latency), a periodic report (`--stats-interval`, default 60 s:
  hashrate per core type, share rate, pool-credited hashrate, time to share and to a
  solo block, CPU temperature and clocks, thermal state) and a final summary. Colors
  turn off automatically without a terminal or with `NO_COLOR`/`--no-color`.
- Ctrl-C stops cleanly and prints the summary. Press twice to force.

Verified live on rplant (`na.rplant.xyz:17059`, TLS 1.3): 44/44 shares accepted in
5 minutes, through a pool difficulty change and seven new blocks.

The miner switches to every new job, binds difficulty per job, retries temporary
submission failures, and handles keepalive, reconnects and failover. Submission
retries preserve the original share; duplicate handling depends on the pool.

## Inspect and measure

```sh
./target/release/tideminer topology            # detected CPUs and the worker placement
./target/release/tideminer bench --seconds 60  # sustained H/s on the mining engine
./target/release/tideminer bench --hashes 4096 -t 4   # reproducible corpus + digest XORs
./target/release/tideminer self-test
./target/release/tideminer probe -o stratum+tcp://POOL_HOST:PORT -u TDC_ADDRESS.rig
cargo run --release --example mockpool -- --port 3399 --difficulty 0.002 --warmup 20 --seconds 60
```

`bench --seconds` runs the real engine (pinned workers, dynamic nonce leases, per-hash
job checks) on a synthetic job and reports total and per-core-type H/s as JSON.
`hash` prints raw digest bytes, not the reversed block-explorer display format.
`probe` only checks subscribe/authorize/notify and does not submit shares.

## Performance

On the development i9-13980HX, measured by valid shares at the same validating mock
pool: +2.5% over cpuminer-opt at 32 threads, +29% at 8 threads (topology-aware
placement). Details and method: [docs/BENCHMARKING.md](docs/BENCHMARKING.md).

## WebAssembly

[crates/yespower](crates/yespower/README.md) is a pure-Rust yespower with a WASM SIMD128
kernel and multi-hash lanes for the browser miner: 1.34x the emscripten-compiled C per
Web Worker (two lanes), 6-14% more whole-machine throughput, verified bit-exact
against the reference vectors and the C. It includes a browser benchmark page.

## Tests

- Golden values for cpuminer-compatible share targets (`src/target.rs`) and the
  notify-to-header fixture (`src/work.rs`).
- 19 independent scalar-C reference vectors on concurrent threads
  (`tests/pow_reference.rs`; regenerate with
  `python3 tools/generate_vectors.py --source "$YESPOWER_REFERENCE"`).
- `tests/mine_mock.rs`: full mining against an in-process Stratum V1 pool that
  rebuilds every header independently and re-hashes it, through a
  retarget, a clean job, a "submit queue full" retry and a dropped connection.

Set `YESPOWER_REFERENCE` to the directory containing the scalar C reference
(`yespower-ref.c`, `sha256.c` and headers) before regenerating vectors.

## Performance development

The [detailed yespower research report](docs/YESPOWER_RESEARCH.md) traces the CPU
algorithm, exact memory/operation counts, measured stage costs, CUDA variants and
Hopper resource limits. Its [experiment bundle](research/yespower/README.md) includes
reproducible CPU variants, reference validation and raw measurements.

The [mathematical backtrace](docs/YESPOWER_BACKTRACE.md) follows dependencies from
the final digest, measures dead scratch writes, validates the PWX word
decomposition, and tests exact terminal-pruning and lazy-V prototypes.

The [PWX latency study](docs/YESPOWER_CPU_LATENCY.md) and
[job/shortcut analysis](docs/YESPOWER_SHORTCUT_ANALYSIS.md) bound the dependency
and reuse ceilings; the [cryptanalysis agenda](docs/CRYPTANALYSIS_AGENDA.md)
records what has been tested and what remains. Platform plans: the
[adaptive tuning and mobile notes](docs/AUTOTUNE_AND_MOBILE.md) and the
[web/WASM miner proposal](docs/WEB_MINER.md).

Start with [the plan](docs/PLAN.md) and [benchmark protocol](docs/BENCHMARKING.md).
Each worker owns about 2.10 MiB of yespower scratch, allocated after pinning and freed
when the worker stops.

## License

Original tideminer code is licensed under [MIT](LICENSE). Portions derived from
Openwall yespower retain their [upstream notices](LICENSE-YESPOWER), including
BSD-2-Clause terms. Release archives include both license files.
