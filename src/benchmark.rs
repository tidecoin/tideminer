use crate::engine::{Engine, Placement, Sink, Work, WorkerEvent};
use crate::pow::{self, Hasher, Header};
use crate::target::Target;
use crate::topology::CoreKind;
use crate::work::Job;
use anyhow::{Context, Result, anyhow, ensure};
use serde::Serialize;
use std::sync::{Arc, Mutex, atomic::AtomicU64};
use std::{
    hint::black_box,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

pub const BACKEND: &str = pow::KERNEL;

#[derive(Serialize)]
pub struct Report {
    pub backend: &'static str,
    pub threads: usize,
    pub hashes: u64,
    pub seconds: f64,
    pub hashes_per_second: f64,
    pub scratch_bytes_per_worker: usize,
    pub workers: Vec<WorkerReport>,
}

#[derive(Serialize)]
pub struct WorkerReport {
    pub worker: usize,
    pub hashes: u64,
    pub seconds: f64,
    pub digest_xor: String,
}

/// Fixed amount of unique nonce work (reproducible corpus; digest XORs reduce across
/// worker counts). Thread startup and warmup are outside timing.
pub fn run(threads: usize, hashes: u64, warmup: u32) -> Result<Report> {
    ensure!((1..=4096).contains(&threads), "threads must be in 1..=4096");
    ensure!(
        hashes >= threads as u64 && hashes <= pow::NONCE_SPACE,
        "hashes must be between the thread count and 2^32"
    );
    thread::scope(|scope| -> Result<Report> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let mut starters = Vec::with_capacity(threads);
        let mut handles = Vec::with_capacity(threads);
        for worker in 0..threads {
            let ready_tx = ready_tx.clone();
            let (start_tx, start_rx) = mpsc::channel();
            let first = hashes * worker as u64 / threads as u64;
            let count = hashes * (worker as u64 + 1) / threads as u64
                - hashes * worker as u64 / threads as u64;
            handles.push(
                thread::Builder::new()
                    .name(format!("yespower-{worker}"))
                    .spawn_scoped(scope, move || -> Result<(WorkerReport, Instant)> {
                        let mut header = Header([0; pow::HEADER_LEN]);
                        let mut hasher = None;
                        let prepared = (|| -> Result<()> {
                            let mut h = Hasher::new()?;
                            h.self_test()?;
                            for nonce in 0..warmup {
                                header.set_nonce(nonce);
                                black_box(h.hash(&header.0)?);
                            }
                            hasher = Some(h);
                            Ok(())
                        })();
                        ready_tx
                            .send(prepared)
                            .context("benchmark coordinator stopped")?;
                        start_rx.recv().context("benchmark start cancelled")?;
                        let mut hasher = hasher.context("worker not prepared")?;
                        let started = Instant::now();
                        let mut digest_xor = [0u8; 32];
                        for nonce in first..first + count {
                            header.set_nonce(nonce as u32);
                            let digest = black_box(hasher.hash(&header.0)?);
                            for (sum, byte) in digest_xor.iter_mut().zip(digest) {
                                *sum ^= byte;
                            }
                        }
                        let finished = Instant::now();
                        Ok((
                            WorkerReport {
                                worker,
                                hashes: count,
                                seconds: finished.duration_since(started).as_secs_f64(),
                                digest_xor: hex::encode(digest_xor),
                            },
                            finished,
                        ))
                    })
                    .context("spawn hashing worker")?,
            );
            starters.push(start_tx);
        }
        drop(ready_tx);
        for _ in 0..threads {
            ready_rx.recv().context("worker stopped before warmup")??;
        }
        let started = Instant::now();
        for start in starters {
            start.send(()).context("worker stopped before benchmark")?;
        }
        let mut finished = started;
        let mut workers = Vec::with_capacity(threads);
        for handle in handles {
            let (report, end) = handle
                .join()
                .map_err(|_| anyhow!("hash worker panicked"))??;
            finished = finished.max(end);
            workers.push(report);
        }
        let seconds = finished.duration_since(started).as_secs_f64();
        Ok(Report {
            backend: BACKEND,
            threads,
            hashes,
            seconds,
            hashes_per_second: hashes as f64 / seconds,
            scratch_bytes_per_worker: pow::SCRATCH_BYTES,
            workers,
        })
    })
}

#[derive(Serialize)]
pub struct SustainedReport {
    pub backend: &'static str,
    pub threads: usize,
    /// Hashes in flight (sum of worker lanes).
    pub hashes_in_flight: usize,
    pub seconds: f64,
    pub hashes: u64,
    pub hashes_per_second: f64,
    pub by_kind: Vec<KindRate>,
    pub placements: Vec<Placement>,
    pub per_worker_hps: Vec<f64>,
}

#[derive(Serialize)]
pub struct KindRate {
    pub kind: CoreKind,
    pub workers: usize,
    pub hashes_per_second: f64,
}

/// Offline work for the engine: a realistic header shape and a target that is
/// never met, so workers hash continuously and report nothing.
pub fn synthetic_work() -> Result<Arc<Work>> {
    let params = serde_json::json!([
        "bench",
        "00".repeat(32),
        "01000000",
        "ffffffff",
        [],
        "20000000",
        "1d00ffff",
        "6553f123",
        true
    ]);
    let mut job = Job::from_notify(&params, 1.0)?;
    job.network_target = None;
    Ok(Arc::new(Work {
        session: 0,
        clean_generation: 0,
        job: Arc::new(job),
        extranonce1: Arc::from(&[0u8, 0, 0, 0][..]),
        extranonce2_len: 4,
        counter: Arc::new(AtomicU64::new(0)),
        extranonce2_space: 1 << 31,
        submit_target: Target::ZERO,
    }))
}

/// Fixed-duration throughput on the mining engine itself: pinned workers, dynamic
/// nonce leases, per-hash epoch checks. Measures what `mine` would sustain.
pub fn sustained(
    placements: Vec<Placement>,
    seconds: f64,
    warmup: f64,
    nice: Option<i32>,
) -> Result<SustainedReport> {
    ensure!(
        seconds > 0.0 && seconds <= 86_400.0,
        "seconds must be in (0, 86400]"
    );
    let failure = Arc::new(Mutex::new(None::<String>));
    let sink: Sink = {
        let failure = failure.clone();
        Arc::new(move |event| {
            if let WorkerEvent::Failed { worker, error } = event {
                failure
                    .lock()
                    .unwrap()
                    .get_or_insert(format!("worker {worker}: {error}"));
            }
        })
    };
    let engine = Engine::start(placements, nice, sink)?;
    engine.wait_ready(Duration::from_secs(60))?;
    engine.publish(Some(synthetic_work()?));
    thread::sleep(Duration::from_secs_f64(warmup));
    let before = engine.hashes();
    let started = Instant::now();
    thread::sleep(Duration::from_secs_f64(seconds));
    let after = engine.hashes();
    let elapsed = started.elapsed().as_secs_f64();
    let placements = engine.placements.clone();
    engine.stop();
    if let Some(error) = failure.lock().unwrap().take() {
        return Err(anyhow!(error));
    }
    let per_worker: Vec<u64> = after.iter().zip(&before).map(|(a, b)| a - b).collect();
    let hashes: u64 = per_worker.iter().sum();
    let mut by_kind: Vec<KindRate> = Vec::new();
    for (placement, count) in placements.iter().zip(&per_worker) {
        let rate = *count as f64 / elapsed;
        match by_kind.iter_mut().find(|k| k.kind == placement.kind) {
            Some(k) => {
                k.workers += 1;
                k.hashes_per_second += rate;
            }
            None => by_kind.push(KindRate {
                kind: placement.kind,
                workers: 1,
                hashes_per_second: rate,
            }),
        }
    }
    Ok(SustainedReport {
        backend: BACKEND,
        threads: placements.len(),
        hashes_in_flight: placements.iter().map(|p| p.lanes).sum(),
        seconds: elapsed,
        hashes,
        hashes_per_second: hashes as f64 / elapsed,
        by_kind,
        per_worker_hps: per_worker.iter().map(|&c| c as f64 / elapsed).collect(),
        placements,
    })
}
