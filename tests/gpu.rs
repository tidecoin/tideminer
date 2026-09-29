//! The Metal GPU worker's kernel against the CPU kernel (macOS with a Metal GPU).
#![cfg(all(target_os = "macos", feature = "gpu"))]

use tideminer::gpu::{self, Gpu};

#[test]
fn gpu_hashes_match_the_cpu() {
    if gpu::info().is_none() {
        eprintln!("no Metal GPU: skipped");
        return;
    }
    // A GPU that cannot run the kernel at all (e.g. a CI virtual machine's) skips;
    // any wrong hash below fails.
    let mut gpu = match Gpu::new(64, gpu::DEFAULT_THREADGROUP) {
        Ok(gpu) => gpu,
        Err(error) => {
            eprintln!("GPU unusable here, skipped: {error:#}");
            return;
        }
    };
    gpu.self_test().unwrap();
    // 64 pseudo-random headers per batch, two batches: enough pwxform rounds that
    // the kernel's rare conflict path (a read of an entry written earlier in the same
    // round) runs many times.
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..2 {
        let headers: Vec<[u8; 80]> = (0..64)
            .map(|_| {
                std::array::from_fn(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
            })
            .collect();
        let digests = gpu.hash(&headers).unwrap();
        for (header, digest) in headers.iter().zip(&digests) {
            assert_eq!(*digest, tidecoin_yespower::hash(header));
        }
    }
}
