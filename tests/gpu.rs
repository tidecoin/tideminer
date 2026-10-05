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

    // Both retained commands must survive the submit pools draining. Reuse one
    // output allocation while alternating slots and changing the batch length.
    let mut tails = Vec::with_capacity(64);
    let output_storage = tails.as_ptr();
    for count in [64, 1, 17, 63] {
        let headers: [Vec<[u8; 80]>; 2] = std::array::from_fn(|slot| {
            (0..count)
                .map(|n| [((n + slot * 64) & 255) as u8; 80])
                .collect()
        });
        let prepared = headers.each_ref().map(|headers| {
            headers
                .iter()
                .map(tidecoin_yespower::prepare)
                .collect::<Vec<_>>()
        });
        for (slot, batch) in prepared.iter().enumerate() {
            let inputs: Vec<_> = batch.iter().map(|item| item.b).collect();
            gpu.submit(slot, &inputs).unwrap();
        }
        for slot in 0..2 {
            gpu.wait_into(slot, &mut tails).unwrap();
            assert_eq!(tails.len(), count);
            assert_eq!(tails.as_ptr(), output_storage);
            for ((tail, prepared), header) in tails.iter().zip(&prepared[slot]).zip(&headers[slot])
            {
                assert_eq!(
                    tidecoin_yespower::finish(tail, &prepared.prehash),
                    tidecoin_yespower::hash(header)
                );
            }
            assert!(!gpu.busy(slot));
        }
    }
}
