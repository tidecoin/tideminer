//! Long-lived hashing workers.
//!
//! Each worker is placed (pinned on Linux, QoS-hinted on macOS) before its
//! yespower context first touches scratch, then loops: take a nonce lease from
//! the current work's shared counter, hash it, report qualifying digests, and
//! check the work epoch after every hash (~0.6-2 ms) so job switches are prompt.
//! Leases are claimed dynamically, so faster cores simply take more of them.
use crate::os;
use crate::pow::{Hasher, NONCE_OFFSET};
use crate::target::Target;
use crate::topology::{CoreKind, Topology};
use crate::work::{Job, extranonce2_bytes};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Nonces per lease. Divides 2^32, so a lease never crosses an extranonce2 boundary,
/// and is a multiple of every supported lane count.
pub const LEASE: u64 = 64;
/// Supported hashes per worker step.
pub const MAX_LANES: usize = 2;

/// Work as the workers see it: one job, one extranonce binding, one search counter.
pub struct Work {
    pub session: u64,
    /// Bumped by `clean_jobs=true`, reconnects and extranonce changes.
    pub clean_generation: u64,
    pub job: Arc<Job>,
    pub extranonce1: Arc<[u8]>,
    pub extranonce2_len: usize,
    /// High 32 bits: extranonce2 index; low 32 bits: nonce. Shared by jobs with an
    /// identical template so a retarget continues instead of re-searching headers.
    pub counter: Arc<AtomicU64>,
    pub extranonce2_space: u64,
    pub submit_target: Target,
}

impl Work {
    pub fn header(&self, extranonce2: u64) -> [u8; 80] {
        self.job.header(
            &self.extranonce1,
            &extranonce2_bytes(extranonce2, self.extranonce2_len),
        )
    }
}

pub struct Found {
    pub work: Arc<Work>,
    pub extranonce2: u64,
    pub nonce: u32,
    pub digest: [u8; 32],
    pub worker: usize,
}

pub enum WorkerEvent {
    Found(Found),
    Failed {
        worker: usize,
        error: String,
    },
    /// A worker stopped without stopping mining (the GPU worker: no device, a failed
    /// self-test or a wrong hash); the other workers go on.
    Warning {
        worker: usize,
        message: String,
    },
}

pub type Sink = Arc<dyn Fn(WorkerEvent) + Send + Sync>;

#[derive(Clone, Debug, Serialize)]
pub struct Placement {
    pub cpu: Option<usize>,
    pub kind: CoreKind,
    /// Physical core index, shared by SMT siblings.
    pub core: usize,
    pub smt_rank: usize,
    pub l2_group: Option<usize>,
    /// Hashes interleaved per step (1 or 2). 2 pays off on cores without an
    /// SMT sibling at work; see [`auto_lanes`].
    pub lanes: usize,
    /// Set for the GPU worker (`--gpu`, macOS), which is placed after the CPUs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuSpec>,
}

/// The GPU worker's load (`--gpu`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuSpec {
    /// Hashes in flight on the GPU (`--gpu-hashes`).
    pub hashes: usize,
    /// Threads per threadgroup, 8 per hash (`--gpu-threadgroup`).
    pub threadgroup: usize,
}

impl Placement {
    /// The GPU worker's placement.
    pub fn gpu(spec: GpuSpec) -> Self {
        Self {
            cpu: None,
            kind: CoreKind::Gpu,
            // Never equal to a CPU core, so per-core rules ignore it.
            core: usize::MAX,
            smt_rank: 0,
            l2_group: None,
            lanes: 1,
            gpu: Some(spec),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
pub enum Layout {
    /// Every allowed logical CPU (measured best sustained throughput on hybrid i9).
    All,
    /// One thread per physical core, no SMT siblings.
    Physical,
    /// Performance cores only (with SMT).
    Performance,
    /// Efficiency cores only.
    Efficiency,
}

/// Choose worker placements. `threads` takes CPUs in priority order measured on a
/// hybrid i9-13980HX under sustained load: first thread of each P core (~1.08 kH/s
/// each), then P-core SMT siblings (~+360 H/s each), then E cores spread across
/// L2 groups (~+185-300 H/s each once P cores are busy).
pub fn plan(
    topology: &Topology,
    layout: Layout,
    threads: Option<usize>,
    cpu_list: Option<&[usize]>,
) -> Result<Vec<Placement>> {
    let place = |c: &crate::topology::Cpu| Placement {
        cpu: c.id,
        kind: c.kind,
        core: c.core,
        smt_rank: c.smt_rank,
        l2_group: c.l2_group,
        lanes: 1,
        gpu: None,
    };
    if let Some(list) = cpu_list {
        ensure!(topology.pinning, "--cpus needs an OS with thread pinning");
        let mut out = Vec::new();
        for &id in list {
            let Some(cpu) = topology.cpus.iter().find(|c| c.id == Some(id)) else {
                bail!("CPU {id} is not available to this process");
            };
            out.push(place(cpu));
        }
        ensure!(!out.is_empty(), "--cpus selected no CPUs");
        return Ok(out);
    }
    let mut candidates: Vec<_> = topology
        .cpus
        .iter()
        .filter(|c| match layout {
            Layout::All => true,
            Layout::Physical => c.smt_rank == 0,
            Layout::Performance => c.kind != CoreKind::Efficiency,
            Layout::Efficiency => c.kind == CoreKind::Efficiency,
        })
        .collect();
    ensure!(
        !candidates.is_empty(),
        "layout {layout:?} matches no CPUs on this machine"
    );
    // Priority: P (or uniform) before E, then SMT rank, then spread over L2 groups
    // (round-robin by position inside each group), then CPU id for stability.
    let mut position = std::collections::HashMap::new();
    let mut rank = Vec::with_capacity(candidates.len());
    for c in &candidates {
        let slot = position
            .entry((c.l2_group, c.kind, c.smt_rank))
            .or_insert(0usize);
        rank.push(*slot);
        *slot += 1;
    }
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by_key(|&i| {
        let c = candidates[i];
        let class = match c.kind {
            CoreKind::Performance | CoreKind::Unknown => 0,
            CoreKind::Efficiency => 1,
            CoreKind::Gpu => 2,
        };
        (class, c.smt_rank, rank[i], c.id, i)
    });
    candidates = order.into_iter().map(|i| candidates[i]).collect();
    let count = threads.unwrap_or(candidates.len());
    ensure!(count >= 1, "thread count must be at least 1");
    if topology.pinning {
        ensure!(
            count <= candidates.len(),
            "{count} threads requested but layout {layout:?} has {} CPUs",
            candidates.len()
        );
    }
    // Without pinning (macOS) extra threads beyond the CPU count are allowed but unhelpful.
    Ok((0..count)
        .map(|i| place(candidates[i % candidates.len()]))
        .collect())
}

/// Give a second interleaved hash to the workers whose core would otherwise wait
/// idle on S-box loads. Measured on the i9-13980HX: 2 lanes add 25-36% on such a
/// core and match what a busy SMT sibling adds, but lose where a shared cache is
/// already the limit. A worker gets 2 lanes, in placement (priority) order, when
/// - no other worker runs on its physical core (a busy SMT sibling already fills
///   those stalls; a second lane then only adds cache pressure);
/// - on an efficiency core, no other worker shares its L2 cluster (a loaded E
///   cluster is bound by its shared L2: pairing there lost 20-30%; an unknown
///   cluster counts as one per core kind);
/// - the hashes in flight stay within the logical CPU count, so the scratch
///   footprint never exceeds the all-threads default (32 workers x 2 lanes lost 19%);
/// - one lane's S-boxes do not already fit the L1 data cache. Where they fit (Apple
///   Silicon P cores, 128 KiB), a second lane evicts them: on an M3 Max 2 lanes
///   lost 36-43% on any worker count. macOS cannot pin, so a worker may land on
///   any core there and the largest L1D decides.
pub fn auto_lanes(placements: &mut [Placement], topology: &Topology) {
    let logical_cpus = topology.cpus.len();
    let one_lane_fits_l1 = topology
        .max_l1d_bytes
        .is_some_and(|l1d| l1d >= tidecoin_yespower::SBOX_BYTES as u64);
    use std::collections::HashMap;
    let mut per_core = HashMap::new();
    let mut per_cluster = HashMap::new();
    for p in placements.iter() {
        *per_core.entry(p.core).or_insert(0usize) += 1;
        *per_cluster.entry((p.kind, p.l2_group)).or_insert(0usize) += 1;
    }
    let mut spare = logical_cpus.saturating_sub(placements.len());
    for p in placements.iter_mut() {
        let core_alone = per_core[&p.core] == 1;
        let cluster_alone =
            p.kind != CoreKind::Efficiency || per_cluster[&(p.kind, p.l2_group)] == 1;
        p.lanes = if spare > 0 && core_alone && cluster_alone && !one_lane_fits_l1 {
            spare -= 1;
            2
        } else {
            1
        };
    }
}

#[repr(align(128))]
struct PaddedCounter(AtomicU64);

struct Shared {
    slot: Mutex<Option<Arc<Work>>>,
    changed: Condvar,
    epoch: AtomicU64,
    stop: AtomicBool,
    ready: Vec<AtomicBool>,
    failure: Mutex<Option<String>>,
    exhausted: AtomicU64,
    /// Workers with index >= this are parked (thermal control). Placement order
    /// is priority order, so the least valuable workers park first.
    active: AtomicUsize,
    counters: Box<[PaddedCounter]>,
}

impl Shared {
    /// Block until there is work or we are stopping.
    fn wait_for_work(&self, index: usize) -> Option<(Arc<Work>, u64)> {
        let mut slot = self.slot.lock().unwrap();
        loop {
            if self.stop.load(Relaxed) {
                return None;
            }
            if !self.parked(index)
                && let Some(work) = slot.as_ref()
            {
                return Some((work.clone(), self.epoch.load(Relaxed)));
            }
            slot = self.changed.wait(slot).unwrap();
        }
    }

    fn parked(&self, index: usize) -> bool {
        index >= self.active.load(Relaxed)
    }

    /// Sleep while this worker is parked; true if work changed meanwhile.
    fn wait_while_parked(&self, index: usize, epoch: u64) -> bool {
        let mut slot = self.slot.lock().unwrap();
        while !self.stop.load(Relaxed) && self.parked(index) {
            slot = self.changed.wait(slot).unwrap();
        }
        self.epoch.load(Relaxed) != epoch
    }

    fn wait_for_new_epoch(&self, epoch: u64) {
        let mut slot = self.slot.lock().unwrap();
        while !self.stop.load(Relaxed) && self.epoch.load(Relaxed) == epoch {
            slot = self.changed.wait(slot).unwrap();
        }
    }
}

pub struct Engine {
    shared: Arc<Shared>,
    handles: Vec<JoinHandle<()>>,
    pub placements: Vec<Placement>,
}

impl Engine {
    pub fn start(placements: Vec<Placement>, nice: Option<i32>, sink: Sink) -> Result<Self> {
        ensure!(!placements.is_empty(), "no worker placements");
        let shared = Arc::new(Shared {
            slot: Mutex::new(None),
            changed: Condvar::new(),
            epoch: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            ready: (0..placements.len())
                .map(|_| AtomicBool::new(false))
                .collect(),
            failure: Mutex::new(None),
            active: AtomicUsize::new(placements.len()),
            exhausted: AtomicU64::new(0),
            counters: (0..placements.len())
                .map(|_| PaddedCounter(AtomicU64::new(0)))
                .collect(),
        });
        // Own the handles before spawning: a later spawn failure must stop and
        // join the workers already started, including those waiting for work.
        let mut engine = Self {
            shared: shared.clone(),
            handles: Vec::with_capacity(placements.len()),
            placements,
        };
        let has_cpu_workers = engine.placements.iter().any(|p| p.gpu.is_none());
        for (index, placement) in engine.placements.iter().cloned().enumerate() {
            let shared = shared.clone();
            let sink = sink.clone();
            let name = match placement.cpu {
                Some(cpu) => format!("tm-{}{cpu}", placement.kind.label()),
                None => format!("tm-{index}"),
            };
            engine.handles.push(
                std::thread::Builder::new()
                    .name(name)
                    .stack_size(256 * 1024)
                    .spawn(move || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            worker(index, &placement, nice, &shared, &sink)
                        }))
                        .unwrap_or_else(|panic| {
                            let message = panic
                                .downcast_ref::<String>()
                                .map(String::as_str)
                                .or_else(|| panic.downcast_ref::<&str>().copied())
                                .unwrap_or("unknown panic");
                            Err(anyhow::anyhow!("worker panicked: {message}"))
                        });
                        if let Err(error) = result {
                            let error = format!("{error:#}");
                            let optional = placement.gpu.is_some() && has_cpu_workers;
                            if !optional {
                                shared
                                    .failure
                                    .lock()
                                    .unwrap()
                                    .get_or_insert_with(|| format!("worker {index}: {error}"));
                            }
                            // Failed optional GPU startup must not delay CPUs.
                            shared.ready[index].store(true, Relaxed);
                            sink(if optional {
                                WorkerEvent::Warning {
                                    worker: index,
                                    message: format!("GPU worker stopped: {error}"),
                                }
                            } else {
                                WorkerEvent::Failed {
                                    worker: index,
                                    error,
                                }
                            });
                        }
                    })?,
            );
        }
        Ok(engine)
    }

    /// Wait until every worker has placed itself and passed its self-test.
    pub fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(error) = self.shared.failure.lock().unwrap().as_ref() {
                bail!("{error}");
            }
            if self.shared.ready.iter().all(|ready| ready.load(Relaxed)) {
                // A failing worker publishes its error before marking ready.
                if let Some(error) = self.shared.failure.lock().unwrap().as_ref() {
                    bail!("{error}");
                }
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "workers did not start in time");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Replace the current work; every worker switches after its current hash.
    /// Let only the first `active` workers hash (placement = priority order);
    /// the rest sleep, keeping their scratch, until allowed again.
    pub fn set_active(&self, active: usize) {
        let _slot = self.shared.slot.lock().unwrap();
        self.shared
            .active
            .store(active.min(self.placements.len()), Relaxed);
        self.shared.changed.notify_all();
    }

    pub fn active(&self) -> usize {
        self.shared.active.load(Relaxed)
    }

    pub fn publish(&self, work: Option<Arc<Work>>) {
        let mut slot = self.shared.slot.lock().unwrap();
        *slot = work;
        self.shared.epoch.fetch_add(1, Relaxed);
        self.shared.changed.notify_all();
    }

    /// Cumulative hashes per worker.
    pub fn hashes(&self) -> Vec<u64> {
        self.shared
            .counters
            .iter()
            .map(|c| c.0.load(Relaxed))
            .collect()
    }

    pub fn total_hashes(&self) -> u64 {
        self.shared.counters.iter().map(|c| c.0.load(Relaxed)).sum()
    }

    pub fn exhausted_events(&self) -> u64 {
        self.shared.exhausted.load(Relaxed)
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        {
            let _slot = self.shared.slot.lock().unwrap();
            self.shared.stop.store(true, Relaxed);
            self.shared.epoch.fetch_add(1, Relaxed);
            self.shared.changed.notify_all();
        }
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker(
    index: usize,
    placement: &Placement,
    nice: Option<i32>,
    shared: &Shared,
    sink: &Sink,
) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Some(cpu) = placement.cpu {
        ensure!(
            os::pin_current_thread(cpu),
            "could not pin worker to CPU {cpu}"
        );
    }
    #[cfg(target_os = "macos")]
    os::prefer_core_class(placement.kind != CoreKind::Efficiency);
    #[cfg(any(unix, windows))]
    if let Some(nice) = nice {
        os::set_current_thread_nice(nice);
    }
    #[cfg(not(any(unix, windows)))]
    let _ = nice;
    if let Some(spec) = placement.gpu {
        #[cfg(all(target_os = "macos", feature = "gpu"))]
        // Inner pools drain every Metal batch; this outer pool also covers
        // framework temporaries created while dropping the worker's resources.
        return objc2::rc::autoreleasepool(|_| scan_gpu(index, spec, shared, sink));
        #[cfg(not(all(target_os = "macos", feature = "gpu")))]
        {
            let _ = (spec, sink);
            bail!("GPU mining needs macOS (Metal) and the `gpu` feature");
        }
    }
    // Allocate and first-touch scratch only after placement.
    match placement.lanes {
        1 => scan::<1>(index, shared, sink),
        2 => scan::<2>(index, shared, sink),
        lanes => bail!("unsupported lane count {lanes} (1..={MAX_LANES})"),
    }
}

/// The mining loop with `K` hashes per step: K consecutive nonces of one lease.
fn scan<const K: usize>(index: usize, shared: &Shared, sink: &Sink) -> Result<()> {
    const { assert!(K >= 1 && LEASE.is_multiple_of(K as u64)) };
    let mut hasher = Hasher::<K>::new()?;
    hasher.self_test()?;
    shared.ready[index].store(true, Relaxed);
    let counter = &shared.counters[index].0;
    let mut done = 0u64;
    while let Some((work, epoch)) = shared.wait_for_work(index) {
        let mut headers = [[0u8; 80]; K];
        let mut header_extranonce2 = u64::MAX;
        'work: loop {
            let start = work.counter.fetch_add(LEASE, Relaxed);
            let extranonce2 = start >> 32;
            if extranonce2 >= work.extranonce2_space {
                shared.exhausted.fetch_add(1, Relaxed);
                shared.wait_for_new_epoch(epoch);
                break 'work;
            }
            if extranonce2 != header_extranonce2 {
                headers = [work.header(extranonce2); K];
                header_extranonce2 = extranonce2;
            }
            let first = start as u32;
            for step in (0..LEASE as u32).step_by(K) {
                let nonces: [u32; K] =
                    core::array::from_fn(|k| first.wrapping_add(step + k as u32));
                for (header, nonce) in headers.iter_mut().zip(nonces) {
                    header[NONCE_OFFSET..].copy_from_slice(&nonce.to_le_bytes());
                }
                let digests = hasher.hash_lanes(&headers)?;
                done += K as u64;
                counter.store(done, Relaxed);
                for (nonce, digest) in nonces.into_iter().zip(digests) {
                    if work.submit_target.is_met_by(&digest) {
                        sink(WorkerEvent::Found(Found {
                            work: work.clone(),
                            extranonce2,
                            nonce,
                            digest,
                            worker: index,
                        }));
                    }
                }
                if shared.epoch.load(Relaxed) != epoch {
                    break 'work;
                }
                if shared.parked(index) && shared.wait_while_parked(index, epoch) {
                    break 'work;
                }
            }
        }
    }
    Ok(())
}

/// The GPU checks the known header/hash pair this often while mining.
#[cfg(all(target_os = "macos", feature = "gpu"))]
const GPU_CHECK_EVERY: Duration = Duration::from_secs(60);

/// One GPU hash in a batch. The enclosing work loop owns the job until both
/// slots drain, so individual nonces do not need their own reference count.
#[cfg(any(test, all(target_os = "macos", feature = "gpu")))]
struct GpuItem {
    check: bool,
    extranonce2: u64,
    nonce: u32,
    prepared: tidecoin_yespower::Prepared,
}

/// Nonces for GPU batches from the work's lease counter, a lease at a time.
#[cfg(any(test, all(target_os = "macos", feature = "gpu")))]
struct GpuFeed {
    work: Arc<Work>,
    header: [u8; 80],
    extranonce2: u64,
    next: u64,
    end: u64,
    exhausted: bool,
}

#[cfg(any(test, all(target_os = "macos", feature = "gpu")))]
impl GpuFeed {
    fn new(work: Arc<Work>) -> Self {
        Self {
            work,
            header: [0; 80],
            extranonce2: u64::MAX,
            next: 0,
            end: 0,
            exhausted: false,
        }
    }

    /// Fill the existing batch to `count`, preserving any known-answer item.
    /// May return fewer once the extranonce2 space is exhausted.
    fn fill(&mut self, items: &mut Vec<GpuItem>, count: usize, shared: &Shared) {
        while items.len() < count && !self.exhausted {
            if self.next == self.end {
                let start = self.work.counter.fetch_add(LEASE, Relaxed);
                let extranonce2 = start >> 32;
                if extranonce2 >= self.work.extranonce2_space {
                    shared.exhausted.fetch_add(1, Relaxed);
                    self.exhausted = true;
                    break;
                }
                if extranonce2 != self.extranonce2 {
                    self.header = self.work.header(extranonce2);
                    self.extranonce2 = extranonce2;
                }
                (self.next, self.end) = (start, start + LEASE);
            }
            let nonce = self.next as u32;
            self.next += 1;
            self.header[NONCE_OFFSET..].copy_from_slice(&nonce.to_le_bytes());
            items.push(GpuItem {
                check: false,
                extranonce2: self.extranonce2,
                nonce,
                prepared: tidecoin_yespower::prepare(&self.header),
            });
        }
    }
}

/// The GPU worker: batches of `spec.hashes` nonces, two in flight so the GPU never
/// waits for the CPU. Digests meeting the share target are re-hashed on the CPU
/// before they are reported; any disagreement stops the GPU worker (as a warning).
#[cfg(all(target_os = "macos", feature = "gpu"))]
fn scan_gpu(index: usize, spec: GpuSpec, shared: &Shared, sink: &Sink) -> Result<()> {
    let started = (|| -> Result<_> {
        let mut gpu = crate::gpu::Gpu::new(spec.hashes, spec.threadgroup)?;
        gpu.self_test()?;
        Ok((gpu, Hasher::<1>::new()?))
    })();
    let (mut gpu, mut verifier) = started?;
    shared.ready[index].store(true, Relaxed);
    let vector = crate::pow::Header::from_hex(crate::pow::VECTOR_HEADER)?;
    let counter = &shared.counters[index].0;
    let mut done = 0u64;
    let mut next_check = Instant::now() + GPU_CHECK_EVERY;
    let mut batches: [Vec<GpuItem>; 2] = std::array::from_fn(|_| Vec::with_capacity(gpu.hashes()));
    let mut inputs = Vec::with_capacity(gpu.hashes());
    let mut tails = Vec::with_capacity(gpu.hashes());
    while let Some((work, epoch)) = shared.wait_for_work(index) {
        let mut feed = GpuFeed::new(work.clone());
        // Queue the next batch in `slot`; false once the work has no nonces left.
        let mut submit = |slot: usize, gpu: &mut crate::gpu::Gpu, batch: &mut Vec<GpuItem>| {
            batch.clear();
            if Instant::now() >= next_check {
                next_check = Instant::now() + GPU_CHECK_EVERY;
                batch.push(GpuItem {
                    check: true,
                    extranonce2: 0,
                    nonce: 0,
                    prepared: tidecoin_yespower::prepare(&vector.0),
                });
            }
            feed.fill(batch, gpu.hashes(), shared);
            if batch.iter().all(|item| item.check) {
                return Ok(false);
            }
            inputs.clear();
            inputs.extend(batch.iter().map(|item| item.prepared.b));
            gpu.submit(slot, &inputs)?;
            Ok::<bool, anyhow::Error>(true)
        };
        let mut live = [false; 2];
        for slot in 0..2 {
            live[slot] = submit(slot, &mut gpu, &mut batches[slot])?;
        }
        let mut slot = 0;
        while live[0] || live[1] {
            if !live[slot] {
                slot ^= 1;
                continue;
            }
            gpu.wait_into(slot, &mut tails)?;
            live[slot] = false;
            let batch = &batches[slot];
            for (item, tail) in batch.iter().zip(&tails) {
                let digest = tidecoin_yespower::finish(tail, &item.prepared.prehash);
                if item.check {
                    ensure!(
                        hex::encode(digest) == crate::pow::VECTOR_HASH,
                        "GPU check: wrong hash for the known header"
                    );
                    continue;
                }
                done += 1;
                if work.submit_target.is_met_by(&digest) {
                    let mut header = work.header(item.extranonce2);
                    header[NONCE_OFFSET..].copy_from_slice(&item.nonce.to_le_bytes());
                    let cpu = verifier.hash_lanes(&[header])?[0];
                    ensure!(
                        cpu == digest,
                        "GPU hash differs from the CPU's (nonce {:08x})",
                        item.nonce
                    );
                    sink(WorkerEvent::Found(Found {
                        work: work.clone(),
                        extranonce2: item.extranonce2,
                        nonce: item.nonce,
                        digest,
                        worker: index,
                    }));
                }
            }
            counter.store(done, Relaxed);
            let current = shared.epoch.load(Relaxed) == epoch && !shared.parked(index);
            if current {
                live[slot] = submit(slot, &mut gpu, &mut batches[slot])?;
            }
            slot ^= 1;
        }
        if feed.exhausted {
            shared.wait_for_new_epoch(epoch);
        } else if shared.parked(index) {
            shared.wait_while_parked(index, epoch);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::Cpu;

    #[test]
    fn gpu_feed_reuses_storage_across_nonce_rollover_and_exhaustion() {
        let placements = plan(&Topology::uniform(1), Layout::All, None, None).unwrap();
        let engine = Engine::start(placements, None, Arc::new(|_| {})).unwrap();
        let work = crate::benchmark::synthetic_work().unwrap();
        let mut expected = (1u64 << 32) - LEASE;
        work.counter.store(expected, Relaxed);
        let mut feed = GpuFeed::new(work.clone());
        let mut batch = Vec::with_capacity(96);
        let storage = batch.as_ptr();
        for round in 0..4 {
            batch.clear();
            if round == 0 {
                batch.push(GpuItem {
                    check: true,
                    extranonce2: 0,
                    nonce: 0,
                    prepared: tidecoin_yespower::prepare(&[0; 80]),
                });
            }
            feed.fill(&mut batch, 96, &engine.shared);
            assert_eq!(batch.len(), 96);
            assert_eq!(batch.as_ptr(), storage);
            assert_eq!(Arc::strong_count(&work), 2, "no reference per nonce");
            for item in batch.iter().filter(|item| !item.check) {
                assert_eq!(
                    (item.extranonce2, item.nonce),
                    (expected >> 32, expected as u32)
                );
                let mut header = work.header(item.extranonce2);
                header[NONCE_OFFSET..].copy_from_slice(&item.nonce.to_le_bytes());
                let prepared = tidecoin_yespower::prepare(&header);
                assert_eq!(item.prepared.b, prepared.b);
                assert_eq!(item.prepared.prehash, prepared.prehash);
                expected += 1;
            }
        }

        let mut last_work = crate::benchmark::synthetic_work().unwrap();
        Arc::get_mut(&mut last_work).unwrap().extranonce2_space = 1;
        last_work.counter.store((1u64 << 32) - LEASE, Relaxed);
        let mut feed = GpuFeed::new(last_work);
        batch.clear();
        feed.fill(&mut batch, 96, &engine.shared);
        assert_eq!(batch.len(), LEASE as usize);
        assert!(feed.exhausted);
        assert_eq!(batch.as_ptr(), storage);
        assert_eq!(batch.last().unwrap().nonce, u32::MAX);
        batch.clear();
        feed.fill(&mut batch, 96, &engine.shared);
        assert!(batch.is_empty());
        engine.stop();
    }

    #[test]
    fn startup_failure_is_reported_without_waiting_for_timeout() {
        let mut placements = plan(&Topology::uniform(2), Layout::All, None, None).unwrap();
        placements[1].lanes = 0;
        let engine = Engine::start(placements, None, Arc::new(|_| {})).unwrap();
        let start = Instant::now();
        let error = engine.wait_ready(Duration::from_secs(5)).unwrap_err();
        assert!(
            error.to_string().contains("unsupported lane count"),
            "{error}"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        // Also joins the healthy worker sleeping for work.
        engine.stop();
    }

    #[test]
    fn parked_workers_stop_and_hash_totals_agree() {
        let placements = plan(&Topology::uniform(2), Layout::All, None, None).unwrap();
        let engine = Engine::start(placements, None, Arc::new(|_| {})).unwrap();
        engine.wait_ready(Duration::from_secs(5)).unwrap();
        engine.set_active(0);
        engine.publish(Some(crate::benchmark::synthetic_work().unwrap()));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            engine.total_hashes(),
            0,
            "parked workers must not start new work"
        );
        assert_eq!(engine.total_hashes(), engine.hashes().iter().sum::<u64>());
        engine.stop();
    }

    #[test]
    fn work_switches_and_parking_preserve_unique_nonces_and_shutdown() {
        let placements = plan(&Topology::uniform(4), Layout::All, None, None).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let engine = Engine::start(
            placements,
            None,
            Arc::new(move |event| {
                tx.send(event).unwrap();
            }),
        )
        .unwrap();
        engine.wait_ready(Duration::from_secs(5)).unwrap();
        let mut work = crate::benchmark::synthetic_work().unwrap();
        Arc::get_mut(&mut work).unwrap().submit_target = Target::MAX;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for step in 0..80 {
                    engine.publish((step % 3 != 0).then(|| work.clone()));
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
            scope.spawn(|| {
                for step in 0..85 {
                    engine.set_active(step % 5);
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
        });
        let mut resumed = crate::benchmark::synthetic_work().unwrap();
        let new_work = Arc::get_mut(&mut resumed).unwrap();
        new_work.submit_target = Target::MAX;
        new_work.session = 1;
        new_work.counter = work.counter.clone();
        engine.set_active(4);
        engine.publish(Some(resumed));
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut collected = Vec::new();
        loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("workers must resume on the new work");
            let resumed = matches!(&event, WorkerEvent::Found(found) if found.work.session == 1);
            collected.push(event);
            if resumed {
                break;
            }
        }
        engine.set_active(0);
        engine.publish(None);
        let (stopped, done) = std::sync::mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            engine.stop();
            stopped.send(()).unwrap();
        });
        done.recv_timeout(Duration::from_secs(2))
            .expect("shutdown lost a wakeup");
        shutdown.join().unwrap();
        let mut seen = std::collections::HashSet::new();
        for event in collected.into_iter().chain(rx.try_iter()) {
            match event {
                WorkerEvent::Found(found) => {
                    assert!(
                        seen.insert((found.extranonce2, found.nonce)),
                        "duplicate nonce lease"
                    );
                }
                WorkerEvent::Failed { error, .. } => panic!("{error}"),
                WorkerEvent::Warning { message, .. } => panic!("{message}"),
            }
        }
        assert!(!seen.is_empty());
    }

    #[test]
    fn worker_panic_is_reported_to_the_coordinator() {
        let placements = plan(&Topology::uniform(1), Layout::All, None, None).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let engine = Engine::start(
            placements,
            None,
            Arc::new(move |event| {
                if matches!(event, WorkerEvent::Found(_)) {
                    panic!("injected worker failure");
                }
                tx.send(event).unwrap();
            }),
        )
        .unwrap();
        engine.wait_ready(Duration::from_secs(5)).unwrap();
        let mut work = crate::benchmark::synthetic_work().unwrap();
        Arc::get_mut(&mut work).unwrap().submit_target = Target::MAX;
        engine.publish(Some(work));
        let WorkerEvent::Failed { error, .. } = rx.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("worker panic must be fatal");
        };
        assert!(error.contains("injected worker failure"), "{error}");
        engine.stop();
    }

    #[cfg(not(all(target_os = "macos", feature = "gpu")))]
    #[test]
    fn unavailable_gpu_requires_a_cpu_fallback() {
        let mut placements = plan(&Topology::uniform(2), Layout::All, None, None).unwrap();
        placements[1].gpu = Some(GpuSpec {
            hashes: 1,
            threadgroup: 32,
        });
        let only_gpu = vec![placements[1].clone()];
        let engine = Engine::start(placements, None, Arc::new(|_| {})).unwrap();
        engine.wait_ready(Duration::from_secs(2)).unwrap();
        engine.stop();
        let engine = Engine::start(only_gpu, None, Arc::new(|_| {})).unwrap();
        let error = engine.wait_ready(Duration::from_secs(2)).unwrap_err();
        assert!(error.to_string().contains("GPU mining needs"), "{error}");
        engine.stop();
    }

    fn hybrid() -> Topology {
        // 2 P cores with SMT (cpus 0-3), 4 E cores in two L2 groups (cpus 4-7).
        let mut t = Topology::uniform(0);
        t.pinning = true;
        for id in 0..8 {
            let (kind, core, smt_rank, l2) = if id < 4 {
                (CoreKind::Performance, id / 2, id % 2, id / 2)
            } else {
                (CoreKind::Efficiency, id - 2, 0, 2 + (id - 4) / 2)
            };
            t.cpus.push(Cpu {
                id: Some(id),
                kind,
                core,
                smt_rank,
                l2_group: Some(l2),
                max_mhz: None,
            });
        }
        t
    }

    fn ids(p: &[Placement]) -> Vec<usize> {
        p.iter().map(|p| p.cpu.unwrap()).collect()
    }

    #[test]
    fn thread_priority_is_p_then_smt_then_spread_e() {
        let t = hybrid();
        assert_eq!(
            ids(&plan(&t, Layout::All, None, None).unwrap()),
            [0, 2, 1, 3, 4, 6, 5, 7]
        );
        assert_eq!(
            ids(&plan(&t, Layout::All, Some(5), None).unwrap()),
            [0, 2, 1, 3, 4]
        );
        assert_eq!(
            ids(&plan(&t, Layout::Physical, None, None).unwrap()),
            [0, 2, 4, 6, 5, 7]
        );
        assert_eq!(
            ids(&plan(&t, Layout::Performance, None, None).unwrap()),
            [0, 2, 1, 3]
        );
        assert_eq!(
            ids(&plan(&t, Layout::Efficiency, None, None).unwrap()),
            [4, 6, 5, 7]
        );
        assert_eq!(
            ids(&plan(&t, Layout::All, None, Some(&[7, 1])).unwrap()),
            [7, 1]
        );
        assert!(plan(&t, Layout::All, None, Some(&[9])).is_err());
        assert!(plan(&t, Layout::All, Some(9), None).is_err());
        assert!(plan(&t, Layout::All, Some(0), None).is_err());
    }

    fn lanes(t: &Topology, layout: Layout, threads: Option<usize>) -> Vec<usize> {
        let mut p = plan(t, layout, threads, None).unwrap();
        auto_lanes(&mut p, t);
        p.iter().map(|p| p.lanes).collect()
    }

    #[test]
    fn auto_lanes_fill_idle_cores_only() {
        let t = hybrid();
        // Every logical CPU busy: one hash each.
        assert_eq!(lanes(&t, Layout::All, None), [1; 8]);
        // P cores with idle SMT siblings get 2; so does an E core alone in its cluster.
        assert_eq!(lanes(&t, Layout::All, Some(2)), [2, 2]);
        assert_eq!(lanes(&t, Layout::Physical, Some(4)), [2, 2, 2, 2]);
        // Busy siblings: 1 each.
        assert_eq!(lanes(&t, Layout::Performance, None), [1; 4]);
        // Full E clusters stay at 1 even though E cores have no SMT.
        assert_eq!(lanes(&t, Layout::Efficiency, None), [1; 4]);
        // Physical: P cores 2; E clusters of two busy cores 1.
        assert_eq!(lanes(&t, Layout::Physical, None), [2, 2, 1, 1, 1, 1]);
        // CPUs 0, 2, 1: core 0 has both siblings busy, core 1 is alone.
        assert_eq!(lanes(&t, Layout::All, Some(3)), [1, 2, 1]);
    }

    #[test]
    fn auto_lanes_respect_the_footprint_budget() {
        // No SMT and no cluster info (like macOS): 2 lanes only while the hashes
        // in flight fit the logical CPU count, highest priority first.
        let t = Topology::uniform(4);
        assert_eq!(lanes(&t, Layout::All, Some(2)), [2, 2]);
        assert_eq!(lanes(&t, Layout::All, Some(3)), [2, 1, 1]);
        assert_eq!(lanes(&t, Layout::All, None), [1; 4]);
    }

    #[test]
    fn auto_lanes_keep_one_lane_when_its_sboxes_fit_l1() {
        // Apple Silicon: 128 KiB L1D holds one lane's 96 KiB of S-boxes.
        let mut t = Topology::uniform(4);
        t.max_l1d_bytes = Some(128 * 1024);
        assert_eq!(lanes(&t, Layout::All, Some(1)), [1]);
        assert_eq!(lanes(&t, Layout::All, Some(2)), [1, 1]);
        // Smaller L1D (e.g. 48 KiB): unchanged rules.
        t.max_l1d_bytes = Some(48 * 1024);
        assert_eq!(lanes(&t, Layout::All, Some(2)), [2, 2]);
    }
}
