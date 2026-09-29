# Web/WASM miner for Tidecoin

Prepared 2026-09-15. This note records the reasoning and constraints for a
browser-based Tidecoin miner built from the same Rust core.

Status 2026-09-27: the hashing core exists as the pure-Rust
[tidecoin-yespower](../crates/yespower/README.md) crate (WASM SIMD128, 1-2 lanes per
worker, `scan` API, benchmark page). Measured results there supersede the estimates in
section 4. The WebSocket Stratum transport or a WebSocket-to-TCP bridge remains to be implemented.

2026-09-28: SIMD128 support does not mean SIMD128 is faster. On Apple Silicon the
scalar WASM build beats it (M3 Max, 14 workers: Chrome +12%, Safari +17%), so a
miner must pick its build by measuring, not feature detection; the benchmark page's
`auto` build does. Safari also gives only 8 WASM memories per process fast bounds
checks, so with one memory per worker, workers 9+ run 29% slower; sharing one memory
(which needs COOP/COEP headers, reversing the "avoid SharedArrayBuffer" constraint
below where the page can be isolated) lifts Safari on an M3 Max from 12.2k to 14.3k H/s.
Numbers and causes: [tidecoin-yespower](../crates/yespower/README.md).

## 1. Rationale

- **Zero-install onboarding**: open a page, press start. No APK, no Termux, no
  Play Store, works on iOS. This is the lowest-friction path for users who do
  not run dedicated miners.
- **Operator observation**: on a mobile device, a browser WASM miner produced
  better results than the compiled native app. This has not been reproduced
  under a controlled A/B and may reflect the native app using one thread, poor
  scheduling, or a scalar build. It is still the strongest argument for treating
  web as a first-class target rather than a demo.
- Native CPU remains the power-user and farm path; the web miner reuses the same
  work/stratum/PoW logic and is judged by accepted work, not synthetic H/s.

## 2. Constraints

| Area | Constraint |
| --- | --- |
| Transport | Browsers cannot open raw TCP. Stratum V1 needs a WebSocket endpoint or a WS-to-TCP bridge. |
| Parallelism | One Web Worker per wasm instance, each with its own ~2.10 MiB scratch, avoids SharedArrayBuffer, COOP/COEP headers, and wasm-thread TLS problems. |
| Threading | wasm threads (emscripten + SAB) are possible but hostile on mobile Safari; not required. |
| Throttling | Background tabs are throttled or frozen. Mining only works in a visible foreground tab. |
| Screen sleep | Phones suspend the tab when the screen locks; needs the Screen Wake Lock API. |
| Memory | Each worker 2-3 MiB; fine for wasm32. No huge pages, bounds checks cost. |
| Policy | Google Play bans mining apps; browser mining carries the Coinhive stigma. Must be explicit, opt-in, and visibly stoppable. |
| Economics | Browser rates are low; show estimated payout cadence against the pool minimum. |
| Security | Page XSS can leak whatever credential the browser holds; use a worker-scoped token, never the wallet password. |

## 3. Architecture

```text
page (coordinator)
  |  extranonce2 / nonce leases, share queue, stats
  +-- Worker 0: wasm instance, own scratch ---+
  +-- Worker 1: wasm instance, own scratch ---+-- single WebSocket -> pool
  +-- Worker N: wasm instance, own scratch ---+     (or WS/TCP bridge)
```

- Shared Rust core: work construction, target math, share packaging, stats.
  Transport is a trait with TCP (native) and WebSocket (wasm) implementations.
- `wasm-bindgen` plus `web-sys`; compile the module once, instantiate per worker.
- One WS connection owned by the page; workers post candidates and counter
  batches. Reconnect and job epochs live on the page.
- Pool side: either add a WS endpoint to the pool or run a small Rust
  WebSocket-to-TCP bridge. The bridge is also the enforcement point for origin
  allowlists and per-IP limits.

## 4. Performance plan

- Add a wasm SIMD128 path to the C backend: `i64x2.extmul_low_i32x4_s` gives the
  same two-products-per-pair shape as SSE2 `pmuludq`; Salsa maps onto `i32x4`
  add/xor/shift almost directly. This is new kernel work, not a port of the x86
  intrinsics.
- Build with clang `-O3 -msimd128`, then `wasm-opt -O3`; keep the hot loop in
  wasm with no per-hash JS calls.
- Expect a per-core penalty versus native on desktop; whole-device throughput
  can still win because every core gets a worker. Treat both statements as
  hypotheses until the A/B below is run.

## 5. User experience

- Explicit Start/Stop; no auto-mining, ever.
- Worker slider defaulting to 2-4 on mobile, capped at
  `navigator.hardwareConcurrency`; show thermal advice.
- Live H/s per worker, accepted/rejected shares, session time, estimated payout
  cadence, and a clear mining indicator.
- Screen Wake Lock: request `screen` while mining, re-request on
  `visibilitychange`, and stop hashing when the page is hidden. This preserves
  battery and keeps the miner honest.

## 6. Security and abuse

- Worker-scoped pool token with server-side rate limits; never accept wallet
  passwords.
- Origin allowlist and per-IP connection/share limits at the bridge or pool.
- Do not auto-start from a URL parameter; require a click.
- No stored credentials; treat the page as untrusted after any XSS.

## 7. Milestones and gates

Proposed after M2/M3 and the owned-context API:

1. Transport abstraction plus WS endpoint or bridge; handshake proven against
   the mock pool.
2. wasm build of the Rust core with the existing scalar C path; per-worker
   nonce leasing; sustained session test.
3. SIMD128 kernel with the full parity corpus on wasm and native.
4. Fair native-vs-web A/B on one device: same thread count, foreground, screen
   on, same duration and thermal state; report accepted shares and H/J.
5. Live-pool soak with real credentials and payout verification.

## 8. Open questions

- How much of the mobile browser-over-app result is scheduling versus kernel
  build quality?
- SIMD128 gain versus the scalar fallback.
- iOS Safari behavior under longer sessions and wake lock re-acquisition.
- Whether the pool wants a native WS endpoint or an external bridge.
