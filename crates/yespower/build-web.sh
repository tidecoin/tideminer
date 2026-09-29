#!/bin/sh
# Browser packages for web/:
#   pkg-simd, pkg-scalar: one WASM memory per worker; work on any page. Scalar is the
#     fallback without SIMD128 and the faster kernel on ARM (Apple Silicon).
#   pkg-shared-simd, pkg-shared-scalar: every worker uses one shared memory. Safari
#     gives only 8 memories per process guard-page bounds checks; the rest check every
#     access (-29% per worker on an M3 Max), so beyond 8 workers only a shared memory
#     keeps them all fast. Needs a cross-origin-isolated page (COOP/COEP headers) and a
#     nightly toolchain: rustup toolchain install nightly --component rust-src --target wasm32-unknown-unknown
set -e
cd "$(dirname "$0")"
wasm-pack build --release --target web --out-dir web/pkg-simd --no-typescript --features web
RUSTFLAGS="" wasm-pack build --release --target web --out-dir web/pkg-scalar --no-typescript --features web
if rustup run nightly rustc --version >/dev/null 2>&1; then
  # Shared memory: std rebuilt with atomics; the memory is imported (the page hands it
  # to every worker) and re-exported (the first worker hands it to the page).
  SHARED="-C target-feature=+atomics,+bulk-memory,+mutable-globals -C link-arg=--shared-memory \
    -C link-arg=--import-memory -C link-arg=--export-memory -C link-arg=--max-memory=1073741824 \
    -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__tls_size \
    -C link-arg=--export=__tls_align -C link-arg=--export=__tls_base"
  CARGO_UNSTABLE_BUILD_STD=panic_abort,std RUSTFLAGS="-C target-feature=+simd128 $SHARED" \
    rustup run nightly wasm-pack build --release --target web --out-dir web/pkg-shared-simd --no-typescript --features web
  CARGO_UNSTABLE_BUILD_STD=panic_abort,std RUSTFLAGS="$SHARED" \
    rustup run nightly wasm-pack build --release --target web --out-dir web/pkg-shared-scalar --no-typescript --features web
else
  echo "No nightly toolchain: skipping pkg-shared-* (see the comment at the top of this script)."
fi
rm -f web/pkg-*/.gitignore web/pkg-*/package.json web/pkg-*/README.md
ls -l web/pkg-*/*.wasm
