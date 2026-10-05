//! Metal GPU worker for macOS (`--gpu`): yespower's memory-hard core on the GPU,
//! eight SIMD lanes per hash (see `gpu/yespower.metal`), next to the CPU workers.
//!
//! The GPU shares the SoC's memory system and power budget with the CPU, and
//! yespower is memory-hard: every GPU hash streams 2 MiB of V through the system
//! cache, evicting the CPU miner's data. So the right GPU load is small and
//! machine-specific. Measured on an M3 Max (14 CPU cores, 30-core GPU), heat-soaked,
//! all CPU cores plus the GPU: 120-180 hashes in flight added 7-10% in total; 480
//! and more lowered the total by 13-40%. `tideminer tune --gpu` measures it.
//!
//! The CPU does SHA-256 / PBKDF2 before and HMAC after every hash
//! ([`tidecoin_yespower::prepare`] / [`tidecoin_yespower::finish`]), and the engine
//! re-hashes every share on the CPU before reporting it.
//!
//! Along with `os.rs`, the only module with `unsafe`: Metal buffer contents are raw
//! pointers into GPU-visible memory.
#![allow(unsafe_code)]

use anyhow::{Context, Result, anyhow, ensure};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBuffer, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder, MTLCommandQueue,
    MTLComputeCommandEncoder, MTLComputePipelineState, MTLCreateSystemDefaultDevice, MTLDevice,
    MTLLibrary, MTLResourceOptions, MTLSize,
};
use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SOURCE: &str = include_str!("gpu/yespower.metal");
const KERNEL: &str = "yespower_smix8";
/// SIMD lanes per hash in the kernel.
const LANES: usize = 8;
const BATCH_TIMEOUT: Duration = Duration::from_secs(10);
/// GPU scratch per hash: V (2 MiB) + S (96 KiB) + XY (1 KiB) + B (1 KiB).
pub const HASH_BYTES: usize = (2048 * 16 * 8 + 1536 * 8 + 16 * 8 + 128) * 8;
/// Threadgroup size in threads (4 hashes): measured best for CPU + GPU together.
pub const DEFAULT_THREADGROUP: usize = 32;

type Buffer = Retained<ProtocolObject<dyn MTLBuffer>>;
type CommandBuffer = Retained<ProtocolObject<dyn MTLCommandBuffer>>;

/// The Metal device, as far as choosing a load needs.
#[derive(Clone, Debug)]
pub struct Info {
    pub name: String,
    /// GPU cores from the IO registry, when readable.
    pub cores: Option<usize>,
    /// Hashes in flight that fit Metal's recommended working set.
    pub max_hashes: usize,
}

pub fn info() -> Option<Info> {
    autoreleasepool(|_| {
        let device = MTLCreateSystemDefaultDevice()?;
        Some(Info {
            name: device.name().to_string(),
            cores: crate::os::gpu_core_count(),
            max_hashes: (device.recommendedMaxWorkingSetSize() as usize / 2) / HASH_BYTES,
        })
    })
}

struct Slot {
    input: Buffer,
    output: Buffer,
    pending: Option<Pending>,
}

struct Pending {
    command: CommandBuffer,
    count: usize,
    completed: mpsc::Receiver<()>,
    deadline: Instant,
}

/// The kernel, its scratch and two batch slots: while the GPU runs one batch, the
/// CPU prepares the next, so the GPU never waits for SHA / HMAC work.
pub struct Gpu {
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    scratch: Buffer,
    slots: [Slot; 2],
    hashes: usize,
    threadgroup: usize,
}

impl Gpu {
    // Rust worker threads have no Cocoa event loop to drain autoreleased Metal
    // temporaries. Each Metal operation owns a short-lived pool; Retained fields
    // keep submitted commands and their resources alive beyond that pool.
    /// Compile the kernel and allocate scratch for `hashes` in flight.
    pub fn new(hashes: usize, threadgroup: usize) -> Result<Self> {
        autoreleasepool(|_| {
            ensure!(hashes >= 1, "--gpu-hashes must be at least 1");
            ensure!(
                threadgroup >= LANES && threadgroup.is_multiple_of(LANES),
                "--gpu-threadgroup must be a multiple of {LANES}"
            );
            let device = MTLCreateSystemDefaultDevice().context("no Metal GPU")?;
            let library = device
                .newLibraryWithSource_options_error(&NSString::from_str(SOURCE), None)
                .map_err(|e| anyhow!("Metal kernel did not compile: {e}"))?;
            let function = library
                .newFunctionWithName(&NSString::from_str(KERNEL))
                .context("Metal kernel entry point missing")?;
            let pipeline = device
                .newComputePipelineStateWithFunction_error(&function)
                .map_err(|e| anyhow!("Metal pipeline: {e}"))?;
            ensure!(
                threadgroup <= pipeline.maxTotalThreadsPerThreadgroup(),
                "--gpu-threadgroup {threadgroup} exceeds the device limit {}",
                pipeline.maxTotalThreadsPerThreadgroup()
            );
            let working_set = device.recommendedMaxWorkingSetSize() as usize;
            let scratch_bytes = hashes * HASH_BYTES;
            ensure!(
                scratch_bytes <= working_set / 2,
                "--gpu-hashes {hashes} needs {} MiB of GPU memory; this Mac allows about {} MiB",
                scratch_bytes >> 20,
                (working_set / 2) >> 20
            );
            let buffer = |bytes: usize, options: MTLResourceOptions| {
                device
                    .newBufferWithLength_options(bytes, options)
                    .context("Metal buffer allocation failed")
            };
            let scratch = buffer(scratch_bytes, MTLResourceOptions::StorageModePrivate)?;
            let slot = || -> Result<Slot> {
                Ok(Slot {
                    input: buffer(hashes * 32 * 4, MTLResourceOptions::StorageModeShared)?,
                    output: buffer(hashes * 16 * 4, MTLResourceOptions::StorageModeShared)?,
                    pending: None,
                })
            };
            Ok(Self {
                queue: device.newCommandQueue().context("Metal command queue")?,
                pipeline,
                scratch,
                slots: [slot()?, slot()?],
                hashes,
                threadgroup,
            })
        })
    }

    pub fn hashes(&self) -> usize {
        self.hashes
    }

    /// Queue a batch (`inputs`: B's first 32 words per hash) in `slot`. Batches
    /// share the scratch; Metal's hazard tracking runs them one after the other.
    pub fn submit(&mut self, slot: usize, inputs: &[[u32; 32]]) -> Result<()> {
        autoreleasepool(|_| {
            let count = inputs.len();
            ensure!(count >= 1 && count <= self.hashes, "GPU batch size {count}");
            let slot = &mut self.slots[slot];
            ensure!(slot.pending.is_none(), "GPU slot still busy");
            // SAFETY: `input` holds `hashes * 32` u32s (count <= hashes) in shared
            // storage, and no batch using this slot is in flight (checked above).
            unsafe {
                let words = slot.input.contents().cast::<[u32; 32]>().as_ptr();
                std::ptr::copy_nonoverlapping(inputs.as_ptr(), words, count);
            }
            let command = self.queue.commandBuffer().context("Metal command buffer")?;
            let encoder = command
                .computeCommandEncoder()
                .context("Metal compute encoder")?;
            encoder.setComputePipelineState(&self.pipeline);
            let n = count as u32;
            // SAFETY: the buffers outlive the command buffer (held by self and retained
            // by Metal); setBytes copies the 4-byte count immediately.
            unsafe {
                encoder.setBuffer_offset_atIndex(Some(&slot.input), 0, 0);
                encoder.setBuffer_offset_atIndex(Some(&slot.output), 0, 1);
                encoder.setBuffer_offset_atIndex(Some(&self.scratch), 0, 2);
                encoder.setBytes_length_atIndex(NonNull::from(&n).cast(), 4, 3);
            }
            encoder.dispatchThreads_threadsPerThreadgroup(
                MTLSize {
                    width: count * LANES,
                    height: 1,
                    depth: 1,
                },
                MTLSize {
                    width: self.threadgroup,
                    height: 1,
                    depth: 1,
                },
            );
            encoder.endEncoding();
            let (completed, completion) = mpsc::sync_channel(1);
            let handler =
                block2::RcBlock::new(move |_: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                    let _ = completed.try_send(());
                });
            // SAFETY: Metal copies the valid block. Its only capture is an owned,
            // thread-safe sender; it never accesses Gpu or a buffer's raw memory.
            unsafe {
                command.addCompletedHandler(block2::RcBlock::as_ptr(&handler));
            }
            let deadline = Instant::now() + BATCH_TIMEOUT;
            command.commit();
            slot.pending = Some(Pending {
                command,
                count,
                completed: completion,
                deadline,
            });
            Ok(())
        })
    }

    pub fn busy(&self, slot: usize) -> bool {
        self.slots[slot].pending.is_some()
    }

    /// Wait for the batch in `slot`: B's last 16 words per hash.
    pub fn wait(&mut self, slot: usize) -> Result<Vec<[u32; 16]>> {
        let mut out = Vec::new();
        self.wait_into(slot, &mut out)?;
        Ok(out)
    }

    /// Reuse the caller's output allocation across batches. On failure the
    /// caller must ignore its previous contents; an unfinished slot stays busy.
    pub fn wait_into(&mut self, slot: usize, out: &mut Vec<[u32; 16]>) -> Result<()> {
        autoreleasepool(|_| {
            let slot = &mut self.slots[slot];
            let pending = slot.pending.as_ref().context("GPU slot is idle")?;
            // Do not clear pending on timeout: a caller must never reuse input or
            // read output while the GPU could still access them. The worker exits;
            // Metal's retained command references keep its resources alive.
            pending
                .completed
                .recv_timeout(pending.deadline.saturating_duration_since(Instant::now()))
                .context("GPU batch did not complete within 10 seconds")?;
            let Pending { command, count, .. } = slot.pending.take().unwrap();
            if command.status() != MTLCommandBufferStatus::Completed {
                let reason = command
                    .error()
                    .map_or_else(|| "unknown".to_string(), |e| e.to_string());
                return Err(anyhow!("GPU batch failed: {reason}"));
            }
            out.resize(count, [0u32; 16]);
            // SAFETY: the batch completed, so the GPU no longer writes `output`, which
            // holds `hashes * 16` u32s (count <= hashes).
            unsafe {
                let words = slot.output.contents().cast::<[u32; 16]>().as_ptr();
                std::ptr::copy_nonoverlapping(words, out.as_mut_ptr(), count);
            }
            Ok(())
        })
    }

    /// Hash `headers` (one batch) on the GPU: full digests.
    pub fn hash(&mut self, headers: &[[u8; 80]]) -> Result<Vec<[u8; 32]>> {
        let prepared: Vec<_> = headers.iter().map(tidecoin_yespower::prepare).collect();
        let inputs: Vec<[u32; 32]> = prepared.iter().map(|p| p.b).collect();
        self.submit(0, &inputs)?;
        let tails = self.wait(0)?;
        Ok(tails
            .iter()
            .zip(&prepared)
            .map(|(tail, p)| tidecoin_yespower::finish(tail, &p.prehash))
            .collect())
    }

    /// The known Tidecoin header/hash pair plus nonces checked against the CPU.
    pub fn self_test(&mut self) -> Result<()> {
        let vector = crate::pow::Header::from_hex(crate::pow::VECTOR_HEADER)?;
        let count = self.hashes.min(8);
        let headers: Vec<[u8; 80]> = (0..count as u32)
            .map(|i| {
                let mut h = vector.clone();
                if i > 0 {
                    h.set_nonce(i);
                }
                h.0
            })
            .collect();
        let digests = self.hash(&headers)?;
        ensure!(
            hex::encode(digests[0]) == crate::pow::VECTOR_HASH,
            "GPU self-test: wrong hash for the known header"
        );
        for (header, digest) in headers.iter().zip(&digests).skip(1) {
            ensure!(
                *digest == tidecoin_yespower::hash(header),
                "GPU self-test: GPU and CPU hashes differ"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_timeout_keeps_the_slot_busy() {
        let Ok(mut gpu) = Gpu::new(1, DEFAULT_THREADGROUP) else {
            return;
        };
        let prepared = tidecoin_yespower::prepare(&[0; 80]);
        gpu.submit(0, &[prepared.b]).unwrap();
        // Withhold the completion signal, independently of GPU execution speed.
        let (_sender, receiver) = mpsc::channel();
        let pending = gpu.slots[0].pending.as_mut().unwrap();
        let actual = std::mem::replace(&mut pending.completed, receiver);
        pending.deadline = Instant::now() + Duration::from_millis(20);
        assert!(gpu.wait(0).is_err());
        assert!(gpu.busy(0));
        assert!(gpu.submit(0, &[prepared.b]).is_err());
        let pending = gpu.slots[0].pending.as_mut().unwrap();
        pending.completed = actual;
        pending.deadline = Instant::now() + BATCH_TIMEOUT;
        assert_eq!(gpu.wait(0).unwrap().len(), 1);
        assert!(!gpu.busy(0));
    }
}
