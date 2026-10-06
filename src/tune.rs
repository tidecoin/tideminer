//! Autotuning: measure worker configurations on this machine and keep the fastest.
//!
//! `--autotune` (quick, before mining, ~15-40 s) compares configurations at the
//! thread count you allowed: lanes per core, a P-core's SMT sibling versus a second
//! lane, E cores versus SMT siblings. Those effects come from how a core hides
//! S-box latency and show within seconds. `tideminer tune` (offline, minutes) also
//! varies the thread count, which power limits decide, so it measures after the
//! turbo window, reads power where the OS allows, and saves its winner for
//! normal mining startup. `--autotune` measures only when no matching saved result exists.
//!
//! Method: candidates run in mirrored rounds (A B C, C B A, ...) to reduce order
//! bias. A challenger must beat the baseline by a useful margin in every round.
//! When either has CPU interference, its slowest round must also beat the
//! baseline's fastest. Inconclusive runs keep the baseline without saving it.
use crate::benchmark::synthetic_work;
use crate::engine::{self, Engine, Layout, Placement, Sink, WorkerEvent};
use crate::report::{Color, Reporter, Sensors, format_duration, format_rate, timestamp};
use crate::topology::{CoreKind, Topology};
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// What the user fixed; tuning searches only inside these.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub layout: Layout,
    /// Maximum worker threads (`-t`); `None` means every CPU of the layout.
    pub threads: Option<usize>,
    /// Explicit CPU list (`--cpus`, `--cpu-affinity`).
    pub cpus: Option<Vec<usize>>,
    /// Fixed lanes (`--lanes 1|2`); `None` leaves lanes to the tuner.
    pub lanes: Option<usize>,
    /// The GPU worker (`--gpu`, macOS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuLimits>,
}

/// Default GPU hashes in flight per GPU core: on an M3 Max (30-core GPU, all CPU
/// cores mining too) 4-6 per core added the most; 16 and more lowered the total.
pub const GPU_HASHES_PER_CORE: usize = 6;

/// What `--gpu` fixes, and what the device allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuLimits {
    /// Fixed hashes in flight (`--gpu-hashes`); `None` leaves the load to the tuner.
    pub hashes: Option<usize>,
    pub threadgroup: usize,
    /// GPU cores (0: unknown).
    pub cores: usize,
    /// Hashes in flight that fit the GPU's memory.
    pub max_hashes: usize,
}

impl GpuLimits {
    /// The load without tuning: the fixed one, else a few hashes per GPU core.
    pub fn default_hashes(&self) -> usize {
        self.hashes.unwrap_or_else(|| {
            let hashes = if self.cores > 0 {
                GPU_HASHES_PER_CORE * self.cores
            } else {
                64
            };
            hashes.clamp(8, self.max_hashes.max(8))
        })
    }

    /// Loads the tuner compares besides the default: off, and 2 to 10 per core.
    /// Only the fixed load when `--gpu-hashes` is given.
    fn alternatives(&self) -> Vec<Option<usize>> {
        if self.hashes.is_some() {
            return Vec::new();
        }
        let per_core = |n: usize| {
            let cores = if self.cores > 0 { self.cores } else { 10 };
            (n * cores).clamp(8, self.max_hashes.max(8))
        };
        let default = self.default_hashes();
        let mut out = vec![None];
        for n in [2, 4, 8, 10] {
            let load = per_core(n);
            if load != default && !out.contains(&Some(load)) {
                out.push(Some(load));
            }
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LanePolicy {
    One,
    Two,
    /// The rules of [`engine::auto_lanes`].
    Auto,
    /// 2 lanes wherever no other worker shares the physical core.
    Idle,
}

/// One configuration, re-derivable from the topology and the limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    pub threads: usize,
    /// Take E cores before P-core SMT siblings.
    pub efficiency_first: bool,
    /// One worker per selected physical core; cores that had several get 2 lanes.
    pub merge_smt: bool,
    pub lanes: LanePolicy,
    /// GPU hashes in flight; `None`: no GPU worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_hashes: Option<usize>,
}

pub struct Candidate {
    pub recipe: Recipe,
    pub placements: Vec<Placement>,
}

/// Placements for a recipe: the limits' CPUs in priority order (optionally E cores
/// before SMT siblings), the first `threads` of them, then lanes.
pub fn build(topology: &Topology, limits: &Limits, recipe: Recipe) -> Result<Vec<Placement>> {
    let mut order = engine::plan(topology, limits.layout, None, limits.cpus.as_deref())?;
    if recipe.efficiency_first {
        order.sort_by_key(|p| match (p.kind, p.smt_rank) {
            (CoreKind::Efficiency, _) => 1,
            (_, 0) => 0,
            _ => 2,
        });
    }
    // 0 CPU threads only as GPU-only mining (`-t 0 --gpu`).
    ensure!(
        recipe.threads >= 1 || (recipe.gpu_hashes.is_some() && limits.gpu.is_some()),
        "thread count must be at least 1"
    );
    ensure!(
        !topology.pinning || recipe.threads <= order.len(),
        "{} threads requested but only {} CPUs are allowed",
        recipe.threads,
        order.len()
    );
    let mut selected: Vec<Placement> = (0..recipe.threads)
        .map(|i| order[i % order.len()].clone())
        .collect();
    let mut merged = HashSet::new();
    if recipe.merge_smt {
        let mut seen = HashSet::new();
        selected.retain(|p| {
            if seen.insert(p.core) {
                true
            } else {
                merged.insert(p.core);
                false
            }
        });
    }
    match recipe.lanes {
        LanePolicy::One => selected.iter_mut().for_each(|p| p.lanes = 1),
        LanePolicy::Two => selected.iter_mut().for_each(|p| p.lanes = 2),
        LanePolicy::Auto => engine::auto_lanes(&mut selected, topology),
        LanePolicy::Idle => {
            let mut per_core = HashMap::new();
            for p in &selected {
                *per_core.entry(p.core).or_insert(0usize) += 1;
            }
            for p in &mut selected {
                p.lanes = if per_core[&p.core] == 1 { 2 } else { 1 };
            }
        }
    }
    for p in &mut selected {
        if merged.contains(&p.core) {
            p.lanes = 2;
        }
    }
    if let (Some(hashes), Some(gpu)) = (recipe.gpu_hashes, limits.gpu) {
        selected.push(Placement::gpu(engine::GpuSpec {
            hashes,
            threadgroup: gpu.threadgroup,
        }));
    }
    Ok(selected)
}

/// The thread count the limits allow (all CPUs of the layout unless `-t`).
fn allowed_threads(topology: &Topology, limits: &Limits) -> Result<usize> {
    let all = engine::plan(topology, limits.layout, None, limits.cpus.as_deref())?.len();
    Ok(match (&limits.cpus, limits.threads) {
        (None, Some(threads)) => threads,
        _ => all,
    })
}

/// Thread counts worth comparing offline: the allowed maximum first (the baseline),
/// then every point in the priority order where a class of CPUs is complete: P
/// cores, their SMT siblings, then each round of one more E core per L2 cluster.
fn thread_counts(topology: &Topology, limits: &Limits) -> Result<Vec<usize>> {
    let max = allowed_threads(topology, limits)?;
    let order = engine::plan(topology, limits.layout, None, limits.cpus.as_deref())?;
    let mut counts = vec![max];
    let mut rounds: HashMap<Option<usize>, usize> = HashMap::new();
    let mut previous = None;
    for (i, p) in order.iter().enumerate() {
        let key = match p.kind {
            CoreKind::Efficiency => {
                let seen = rounds.entry(p.l2_group).or_default();
                let round = if p.l2_group.is_some() { *seen } else { 0 };
                *seen += 1;
                (1, p.smt_rank, round)
            }
            _ => (0, p.smt_rank, 0),
        };
        if previous.is_some_and(|k| k != key) && i < max && !counts.contains(&i) {
            counts.push(i);
        }
        previous = Some(key);
    }
    counts[1..].sort_unstable();
    Ok(counts)
}

/// Distinct candidates for the given thread counts. The first one is the baseline:
/// the rule-based configuration at the first count.
pub fn candidates(
    topology: &Topology,
    limits: &Limits,
    counts: &[usize],
) -> Result<Vec<Candidate>> {
    let policies = match limits.lanes {
        Some(1) => vec![LanePolicy::One],
        Some(_) => vec![LanePolicy::Two],
        None => vec![LanePolicy::Auto, LanePolicy::One, LanePolicy::Idle],
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for &threads in counts {
        for efficiency_first in [false, true] {
            for merge_smt in [false, true] {
                if merge_smt && limits.lanes == Some(1) {
                    continue;
                }
                for &lanes in &policies {
                    let recipe = Recipe {
                        threads,
                        efficiency_first,
                        merge_smt,
                        lanes,
                        gpu_hashes: limits.gpu.map(|g| g.default_hashes()),
                    };
                    let placements = build(topology, limits, recipe)?;
                    if seen.insert(signature(&placements)) {
                        out.push(Candidate { recipe, placements });
                    }
                }
            }
        }
    }
    // With --gpu: the baseline's CPU configuration at the other GPU loads, GPU off
    // included, so the tuner can conclude that this Mac mines best without it.
    if let (Some(gpu), Some(base)) = (limits.gpu, out.first().map(|c| c.recipe)) {
        for load in gpu.alternatives() {
            // GPU only: turning the GPU off would leave no workers.
            if base.threads == 0 && load.is_none() {
                continue;
            }
            let recipe = Recipe {
                gpu_hashes: load,
                ..base
            };
            let placements = build(topology, limits, recipe)?;
            if seen.insert(signature(&placements)) {
                out.push(Candidate { recipe, placements });
            }
        }
    }
    Ok(out)
}

fn signature(placements: &[Placement]) -> Vec<(Option<usize>, usize, usize, usize, usize)> {
    let mut sig: Vec<_> = placements
        .iter()
        .map(|p| {
            (
                p.cpu,
                p.core,
                p.smt_rank,
                p.lanes,
                p.gpu.map_or(0, |g| g.hashes),
            )
        })
        .collect();
    sig.sort_unstable();
    sig
}

/// Short description: `8 P + 8 SMT + 16 E, 2 lanes on P`.
pub fn label(placements: &[Placement]) -> String {
    const GROUPS: [&str; 4] = ["P", "SMT", "E", "core"];
    let group = |p: &Placement| match (p.smt_rank, p.kind) {
        (1.., _) => 1,
        (_, CoreKind::Performance) => 0,
        (_, CoreKind::Efficiency) => 2,
        (_, CoreKind::Unknown | CoreKind::Gpu) => 3,
    };
    let gpu = placements.iter().find_map(|p| p.gpu);
    let placements: Vec<&Placement> = placements.iter().filter(|p| p.gpu.is_none()).collect();
    let mut count = [0usize; 4];
    let mut paired = [0usize; 4];
    for p in &placements {
        count[group(p)] += 1;
        paired[group(p)] += usize::from(p.lanes > 1);
    }
    let cores: Vec<String> = (0..4)
        .filter(|&g| count[g] > 0)
        .map(|g| format!("{} {}", count[g], GROUPS[g]))
        .collect();
    let total: usize = paired.iter().sum();
    let lanes = if total == 0 {
        "1 lane".to_string()
    } else if total == placements.len() {
        "2 lanes".to_string()
    } else {
        let groups: Vec<usize> = (0..4).filter(|&g| paired[g] > 0).collect();
        let partial = groups.iter().any(|&g| paired[g] < count[g]);
        let on: Vec<String> = groups
            .iter()
            .map(|&g| {
                if partial {
                    format!("{} {}", paired[g], GROUPS[g])
                } else {
                    GROUPS[g].to_string()
                }
            })
            .collect();
        format!("2 lanes on {}", on.join(" + "))
    };
    match gpu {
        Some(spec) if cores.is_empty() => format!("GPU only {}", spec.hashes),
        Some(spec) => format!("{}, {lanes} + GPU {}", cores.join(" + "), spec.hashes),
        None => format!("{}, {lanes}", cores.join(" + ")),
    }
}

// ---------------------------------------------------------------------------
// Measurement

/// CPU or system power, where the OS lets an ordinary process read it.
#[derive(Clone)]
enum PowerSource {
    /// Linux powercap package energy counter (root-only on most distributions).
    Rapl { energy: PathBuf, range: u64 },
    /// Laptop battery discharge: the whole machine, and only while unplugged.
    Battery { dir: PathBuf },
}

impl PowerSource {
    fn detect() -> Option<Self> {
        let rapl = Path::new("/sys/class/powercap/intel-rapl:0");
        let energy = rapl.join("energy_uj");
        if read_u64(&energy).is_some() {
            let range = read_u64(&rapl.join("max_energy_range_uj")).unwrap_or(u64::MAX);
            return Some(Self::Rapl { energy, range });
        }
        let entries = std::fs::read_dir("/sys/class/power_supply").ok()?;
        for entry in entries.flatten() {
            let dir = entry.path();
            let kind = std::fs::read_to_string(dir.join("type")).unwrap_or_default();
            if kind.trim() == "Battery" && battery_watts(&dir).is_some() {
                return Some(Self::Battery { dir });
            }
        }
        None
    }

    fn describe(&self) -> &'static str {
        match self {
            Self::Rapl { .. } => "CPU package (RAPL)",
            Self::Battery { .. } => "whole laptop (battery discharge)",
        }
    }
}

fn read_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn battery_watts(dir: &Path) -> Option<f64> {
    let status = std::fs::read_to_string(dir.join("status")).ok()?;
    if status.trim() != "Discharging" {
        return None;
    }
    if let Some(uw) = read_u64(&dir.join("power_now")) {
        return (uw > 0).then(|| uw as f64 / 1e6);
    }
    let ua = read_u64(&dir.join("current_now"))?;
    let uv = read_u64(&dir.join("voltage_now"))?;
    (ua > 0).then(|| ua as f64 * uv as f64 / 1e12)
}

/// Why `tideminer tune` shows no power column.
fn power_unavailable_reason() -> &'static str {
    if Path::new("/sys/class/powercap/intel-rapl:0/energy_uj").exists() {
        "CPU energy counter is root-only: run `sudo tideminer tune` for watts"
    } else {
        "no readable power sensor on this system"
    }
}

struct Sample {
    rate: f64,
    watts: Option<f64>,
    temperature: Option<f64>,
    /// Estimated share of busy/steal time not accounted for by this process.
    foreign: Option<f64>,
}

/// Busy clock ticks of the given CPUs from `/proc/stat` (Linux).
fn busy_ticks(cpus: &[usize]) -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let mut total = 0;
    for line in stat.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let Some(Ok(cpu)) = fields.next().map(str::parse::<usize>) else {
            continue;
        };
        if !cpus.contains(&cpu) {
            continue;
        }
        let values: Vec<u64> = fields.filter_map(|f| f.parse().ok()).collect();
        // user nice system idle iowait irq softirq steal
        total += [0, 1, 2, 5, 6, 7]
            .iter()
            .filter_map(|&i| values.get(i))
            .sum::<u64>();
    }
    Some(total)
}

/// This process's CPU time in clock ticks (utime + stime, Linux).
fn own_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    Some(fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?)
}

/// Estimated CPU interference between two readings. Includes interrupts and VM
/// steal time, not just other applications. This is a share of accounted busy/
/// steal time, not wall-clock CPU capacity; container accounting may differ too.
fn foreign_share(start: Option<(u64, u64)>, cpus: &[usize]) -> Option<f64> {
    let (busy0, own0) = start?;
    let busy = busy_ticks(cpus)?.checked_sub(busy0)?;
    let own = own_ticks()?.checked_sub(own0)?;
    (busy > 0).then(|| busy.saturating_sub(own) as f64 / busy as f64)
}

struct Probe {
    nice: Option<i32>,
    sensors: Sensors,
    power: Option<PowerSource>,
}

impl Probe {
    fn new(nice: Option<i32>, power: bool) -> Self {
        Self {
            nice,
            sensors: Sensors::new(Vec::new()),
            power: if power { PowerSource::detect() } else { None },
        }
    }

    /// Run `placements` on synthetic work: `settle` seconds untimed, then count
    /// hashes (and energy) for `seconds`.
    fn measure(&self, placements: &[Placement], settle: f64, seconds: f64) -> Result<Sample> {
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
        let engine = Engine::start(placements.to_vec(), self.nice, sink)?;
        engine.wait_ready(Duration::from_secs(60))?;
        engine.publish(Some(synthetic_work()?));
        std::thread::sleep(Duration::from_secs_f64(settle));
        let energy_start = match &self.power {
            Some(PowerSource::Rapl { energy, .. }) => read_u64(energy),
            _ => None,
        };
        let cpus: Vec<usize> = placements.iter().filter_map(|p| p.cpu).collect();
        let ticks = (!cpus.is_empty())
            .then(|| busy_ticks(&cpus).zip(own_ticks()))
            .flatten();
        let before: u64 = engine.hashes().iter().sum();
        let started = Instant::now();
        let deadline = started + Duration::from_secs_f64(seconds);
        let (mut temps, mut battery) = (Vec::new(), Vec::new());
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(500)));
            temps.extend(self.sensors.temperature());
            if let Some(PowerSource::Battery { dir }) = &self.power {
                battery.extend(battery_watts(dir));
            }
        }
        let after: u64 = engine.hashes().iter().sum();
        let elapsed = started.elapsed().as_secs_f64();
        let foreign = foreign_share(ticks, &cpus);
        let watts = match &self.power {
            Some(PowerSource::Rapl { energy, range }) => {
                energy_start.zip(read_u64(energy)).map(|(start, end)| {
                    let used = if end >= start {
                        end - start
                    } else {
                        range - start + end
                    };
                    used as f64 / 1e6 / elapsed
                })
            }
            Some(PowerSource::Battery { .. }) => mean(&battery),
            None => None,
        };
        engine.stop();
        if let Some(error) = failure.lock().unwrap().take() {
            return Err(anyhow!(error));
        }
        Ok(Sample {
            rate: (after - before) as f64 / elapsed,
            watts,
            temperature: mean(&temps),
            foreign,
        })
    }
}

fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

#[derive(Clone, Copy)]
struct Schedule {
    rounds: usize,
    settle: f64,
    seconds: f64,
}

impl Schedule {
    fn duration(&self, candidates: usize) -> f64 {
        (self.rounds * candidates) as f64 * (self.settle + self.seconds + 0.05)
    }
}

/// Measured candidate.
#[derive(Clone, Debug, Serialize)]
pub struct Row {
    pub label: String,
    pub recipe: Recipe,
    pub threads: usize,
    pub hashes_in_flight: usize,
    /// The rule-based configuration.
    pub baseline: bool,
    pub rates: Vec<f64>,
    /// Mean over rounds, H/s.
    pub rate: f64,
    /// (max - min) / mean over rounds.
    pub spread: f64,
    pub watts: Option<f64>,
    pub temperature: Option<f64>,
    /// Largest estimated CPU interference in any round (see `foreign_share`).
    pub foreign: Option<f64>,
    /// Measured only in the short screening pass, not in the final rounds.
    pub screened_out: bool,
}

/// Above this estimated interference, require separated measurement ranges.
pub const DISTURBED: f64 = 0.03;

impl Row {
    pub fn disturbed(&self) -> bool {
        self.foreign.is_some_and(|f| f > DISTURBED)
    }

    pub fn hashes_per_joule(&self) -> Option<f64> {
        self.watts.filter(|w| *w > 0.0).map(|w| self.rate / w)
    }

    fn valid_rounds(&self) -> bool {
        self.rates.len() >= 2 && self.rates.iter().all(|r| r.is_finite() && *r > 0.0)
    }

    /// Compare matching rounds, so a common change in host speed does not count
    /// as uncertainty in the relative gain. These are sequential measurements,
    /// not independent statistical samples or a formal confidence interval.
    fn reliably_beats(&self, other: &Self, margin: f64) -> bool {
        if !self.valid_rounds()
            || !other.valid_rounds()
            || self.rates.len() != other.rates.len()
            || !self
                .rates
                .iter()
                .zip(&other.rates)
                .all(|(a, b)| *a > *b * (1.0 + margin))
        {
            return false;
        }
        if self.disturbed() || other.disturbed() {
            let slowest = self.rates.iter().copied().fold(f64::INFINITY, f64::min);
            let fastest = other.rates.iter().copied().fold(0.0, f64::max);
            return slowest > fastest * (1.0 + margin);
        }
        true
    }
}

/// Mirrored rounds over all candidates; rows in candidate order.
fn run_rounds(
    probe: &Probe,
    candidates: &[Candidate],
    baseline: &[Placement],
    schedule: Schedule,
    progress: &mut dyn FnMut(usize, usize, &str, f64),
) -> Result<Vec<Row>> {
    let mut samples: Vec<Vec<Sample>> = candidates.iter().map(|_| Vec::new()).collect();
    for round in 0..schedule.rounds {
        let order: Vec<usize> = if round % 2 == 0 {
            (0..candidates.len()).collect()
        } else {
            (0..candidates.len()).rev().collect()
        };
        for (step, &i) in order.iter().enumerate() {
            let sample =
                probe.measure(&candidates[i].placements, schedule.settle, schedule.seconds)?;
            progress(round, step, &label(&candidates[i].placements), sample.rate);
            samples[i].push(sample);
        }
    }
    Ok(candidates
        .iter()
        .zip(samples)
        .map(|(c, s)| {
            let rates: Vec<f64> = s.iter().map(|s| s.rate).collect();
            let rate = mean(&rates).unwrap_or(0.0);
            let (lo, hi) = rates
                .iter()
                .fold((f64::MAX, 0.0f64), |(lo, hi), &r| (lo.min(r), hi.max(r)));
            let watts: Vec<f64> = s.iter().filter_map(|s| s.watts).collect();
            let temps: Vec<f64> = s.iter().filter_map(|s| s.temperature).collect();
            let foreign = s.iter().filter_map(|s| s.foreign).reduce(f64::max);
            Row {
                label: label(&c.placements),
                recipe: c.recipe,
                threads: c.placements.len(),
                hashes_in_flight: c
                    .placements
                    .iter()
                    .map(|p| p.gpu.map_or(p.lanes, |g| g.hashes))
                    .sum(),
                baseline: signature(&c.placements) == signature(baseline),
                rates,
                rate,
                spread: if rate > 0.0 { (hi - lo) / rate } else { 0.0 },
                watts: mean(&watts),
                temperature: mean(&temps),
                foreign,
                screened_out: false,
            }
        })
        .collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Verdict {
    /// The rule-based baseline measured fastest by a clear margin.
    Confirmed,
    /// Repeated measurements were inconclusive, so the baseline stays unsaved.
    Kept,
    /// A challenger reliably beat the baseline in repeated measurements.
    Switched,
    /// Interference and inconclusive measurements; the baseline stays unsaved.
    Disturbed,
}

#[derive(Clone, Debug, Serialize)]
pub struct Outcome {
    /// Sorted fastest first.
    pub rows: Vec<Row>,
    /// Median round-to-round spread, descriptive only (not a decision threshold).
    pub noise: f64,
    /// Minimum useful gain required in every comparison round.
    pub margin: f64,
    pub chosen: usize,
    pub verdict: Verdict,
    /// Best hashes per joule, when power was measured.
    pub efficient: Option<usize>,
    pub power: Option<String>,
    pub seconds: f64,
    /// Candidates dropped after screening, fastest first (not part of the decision).
    pub screened: Vec<Row>,
    /// The baseline's screening rate, the reference for `screened`.
    pub screen_baseline: Option<f64>,
}

impl Outcome {
    /// The repeated comparison supports saving the selected configuration.
    pub fn trustworthy(&self) -> bool {
        matches!(self.verdict, Verdict::Confirmed | Verdict::Switched)
    }

    /// Largest estimated CPU interference among the final measurements.
    pub fn foreign(&self) -> Option<f64> {
        self.rows.iter().filter_map(|r| r.foreign).reduce(f64::max)
    }

    pub fn baseline(&self) -> &Row {
        self.rows
            .iter()
            .find(|r| r.baseline)
            .unwrap_or(&self.rows[0])
    }
}

fn decide(mut rows: Vec<Row>, min_margin: f64, power: Option<String>, seconds: f64) -> Outcome {
    rows.sort_by(|a, b| b.rate.total_cmp(&a.rate));
    let mut spreads: Vec<f64> = rows.iter().map(|r| r.spread).collect();
    spreads.sort_by(f64::total_cmp);
    let noise = spreads.get(spreads.len() / 2).copied().unwrap_or(0.0);
    let margin = min_margin;
    let base = rows.iter().position(|r| r.baseline).unwrap_or(0);
    let (chosen, verdict) = if base == 0 {
        let clear = rows[0].valid_rounds()
            && rows[1..]
                .iter()
                .all(|other| rows[0].reliably_beats(other, margin));
        (
            0,
            if clear {
                Verdict::Confirmed
            } else {
                Verdict::Kept
            },
        )
    } else if let Some(winner) = rows
        .iter()
        .position(|row| row.reliably_beats(&rows[base], margin))
    {
        // A one-off spike in the fastest mean must not hide a different
        // challenger whose improvement is repeatable.
        (winner, Verdict::Switched)
    } else {
        (
            base,
            if rows[0].disturbed() || rows[base].disturbed() {
                Verdict::Disturbed
            } else {
                Verdict::Kept
            },
        )
    };
    let efficient = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.hashes_per_joule().map(|e| (i, e)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i);
    Outcome {
        rows,
        noise,
        margin,
        chosen,
        verdict,
        efficient,
        power,
        seconds,
        screened: Vec::new(),
        screen_baseline: None,
    }
}

// ---------------------------------------------------------------------------
// Quick (--autotune) and offline (`tideminer tune`) runs

const QUICK_WARMUP: f64 = 4.0;
const QUICK: Schedule = Schedule {
    rounds: 2,
    settle: 0.5,
    seconds: 3.0,
};
const QUICK_MARGIN: f64 = 0.03;

const OFFLINE_WARMUP: f64 = 30.0;
/// One short pass: screening only has to find the contenders.
const SCREEN: Schedule = Schedule {
    rounds: 1,
    settle: 0.5,
    seconds: 2.5,
};
const FINAL_ROUNDS: usize = 3;
const FINAL_SETTLE: f64 = 2.0;
const FINALISTS: usize = 6;
/// Screening results within this of the best go to the final.
const FINAL_WINDOW: f64 = 0.10;
const OFFLINE_MARGIN: f64 = 0.015;

pub fn quick_candidates(topology: &Topology, limits: &Limits) -> Result<Vec<Candidate>> {
    candidates(topology, limits, &[allowed_threads(topology, limits)?])
}

pub fn offline_candidates(topology: &Topology, limits: &Limits) -> Result<Vec<Candidate>> {
    candidates(topology, limits, &thread_counts(topology, limits)?)
}

pub fn quick_seconds(count: usize) -> f64 {
    QUICK_WARMUP + QUICK.duration(count)
}

/// Quick comparison at the allowed thread count. `None` when only one
/// configuration fits the limits.
pub fn quick(topology: &Topology, limits: &Limits, nice: Option<i32>) -> Result<Option<Outcome>> {
    let candidates = quick_candidates(topology, limits)?;
    if candidates.len() < 2 {
        return Ok(None);
    }
    let started = Instant::now();
    let probe = Probe::new(nice, false);
    let baseline = candidates[0].placements.clone();
    probe.measure(&baseline, QUICK_WARMUP, 0.1)?;
    let rows = run_rounds(&probe, &candidates, &baseline, QUICK, &mut |_, _, _, _| {})?;
    let outcome = decide(rows, QUICK_MARGIN, None, started.elapsed().as_secs_f64());
    Ok(Some(outcome))
}

/// Offline tuning within `minutes`: warm up, screen every candidate briefly, then
/// run the finalists in longer mirrored rounds with power where readable.
pub fn offline(
    topology: &Topology,
    limits: &Limits,
    nice: Option<i32>,
    minutes: f64,
    out: &Reporter,
) -> Result<Outcome> {
    let candidates = offline_candidates(topology, limits)?;
    let started = Instant::now();
    let probe = Probe::new(nice, true);
    let power = probe.power.as_ref().map(|p| p.describe().to_string());
    let baseline = candidates[0].placements.clone();
    let budget = minutes * 60.0;
    let screen_time = SCREEN.duration(candidates.len());
    let finalists = candidates.len().min(FINALISTS + 1);
    let final_seconds = ((budget - OFFLINE_WARMUP - screen_time)
        / (FINAL_ROUNDS * finalists) as f64
        - FINAL_SETTLE)
        .max(5.0);
    out.raw(format!(
        "{} configurations; warm-up {}, screening {}, final ~{} ({} rounds of {:.0} s)",
        candidates.len(),
        format_duration(Duration::from_secs_f64(OFFLINE_WARMUP)),
        format_duration(Duration::from_secs_f64(screen_time)),
        format_duration(Duration::from_secs_f64(
            (FINAL_ROUNDS * finalists) as f64 * (final_seconds + FINAL_SETTLE)
        )),
        FINAL_ROUNDS,
        final_seconds
    ));
    out.raw(match &power {
        Some(source) => format!("power: {source}"),
        None => format!("power: unavailable ({})", power_unavailable_reason()),
    });
    out.raw(format!("warming up on {} ...", label(&baseline)));
    probe.measure(&baseline, OFFLINE_WARMUP, 0.1)?;

    let total = candidates.len();
    let show = |phase: &str, rounds: usize| {
        let phase = phase.to_string();
        move |round: usize, step: usize, name: &str, rate: f64| {
            out.raw(format!(
                "  {phase} {}/{rounds}  [{:>2}/{total}] {name:<34} {}",
                round + 1,
                step + 1,
                format_rate(rate)
            ));
        }
    };
    let screened = run_rounds(
        &probe,
        &candidates,
        &baseline,
        SCREEN,
        &mut show("screen", SCREEN.rounds),
    )?;
    let best = screened.iter().map(|r| r.rate).fold(0.0, f64::max);
    let mut ranked: Vec<usize> = (0..candidates.len()).collect();
    ranked.sort_by(|&a, &b| screened[b].rate.total_cmp(&screened[a].rate));
    let mut chosen: Vec<usize> = ranked
        .into_iter()
        .filter(|&i| screened[i].rate >= best * (1.0 - FINAL_WINDOW))
        .take(FINALISTS)
        .collect();
    if !chosen.contains(&0) {
        chosen.push(0);
    }
    let mut finals: Vec<Candidate> = chosen
        .iter()
        .map(|&i| Candidate {
            recipe: candidates[i].recipe,
            placements: candidates[i].placements.clone(),
        })
        .collect();
    // With --gpu the CPU configuration and the GPU load were varied one at a time
    // from the baseline; when both a CPU variant and another load beat it, the final
    // rounds also try that CPU variant at that load.
    if limits.gpu.is_some() {
        let base = candidates[0].recipe;
        let best = |gpu_axis: bool| {
            (1..candidates.len())
                .filter(|&i| (candidates[i].recipe.gpu_hashes != base.gpu_hashes) == gpu_axis)
                .filter(|&i| screened[i].rate > screened[0].rate)
                .max_by(|&a, &b| screened[a].rate.total_cmp(&screened[b].rate))
        };
        if let (Some(cpu), Some(gpu)) = (best(false), best(true)) {
            let recipe = Recipe {
                gpu_hashes: candidates[gpu].recipe.gpu_hashes,
                ..candidates[cpu].recipe
            };
            let placements = build(topology, limits, recipe)?;
            if !finals
                .iter()
                .any(|f| signature(&f.placements) == signature(&placements))
            {
                finals.push(Candidate { recipe, placements });
            }
        }
    }
    let schedule = Schedule {
        rounds: FINAL_ROUNDS,
        settle: FINAL_SETTLE,
        seconds: final_seconds,
    };
    let total = finals.len();
    let mut show_final = |round: usize, step: usize, name: &str, rate: f64| {
        out.raw(format!(
            "  final {}/{FINAL_ROUNDS}  [{:>2}/{total}] {name:<34} {}",
            round + 1,
            step + 1,
            format_rate(rate)
        ));
    };
    let rows = run_rounds(&probe, &finals, &baseline, schedule, &mut show_final)?;
    let mut outcome = decide(rows, OFFLINE_MARGIN, power, started.elapsed().as_secs_f64());
    outcome.screen_baseline = Some(screened[0].rate);
    outcome.screened = screened
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !chosen.contains(i))
        .map(|(_, mut row)| {
            row.screened_out = true;
            row
        })
        .collect();
    outcome.screened.sort_by(|a, b| b.rate.total_cmp(&a.rate));
    Ok(outcome)
}

/// Ranked table plus the decision.
pub fn print_outcome(out: &Reporter, outcome: &Outcome, indent: &str) {
    let base = outcome.baseline().rate;
    let power = outcome.rows.iter().any(|r| r.watts.is_some());
    let mut header = format!(
        "{indent}  #  {:<36} {:>7} {:>6} {:>11} {:>7} {:>9}",
        "configuration", "threads", "hashes", "H/s", "spread", "vs rules"
    );
    if power {
        header.push_str(&format!(" {:>7} {:>7}", "watts", "H/J"));
    }
    header.push_str(&format!(" {:>5}", "temp"));
    out.raw(out.paint(Color::Dim, header));
    let format_row = |rank: usize, mark: &str, row: &Row, base: f64| {
        let name = if row.baseline {
            format!("{} (rules)", row.label)
        } else {
            row.label.clone()
        };
        let versus = if row.baseline {
            "-".to_string()
        } else {
            format!("{:+.1}%", (row.rate / base - 1.0) * 100.0)
        };
        let spread = if row.rates.len() > 1 {
            format!("±{:.1}%", row.spread * 50.0)
        } else {
            "-".to_string()
        };
        let mut line = format!(
            "{indent}{rank:>3}{mark} {name:<36} {:>7} {:>6} {:>11} {spread:>7} {versus:>9}",
            row.threads,
            row.hashes_in_flight,
            format_rate(row.rate),
        );
        if power {
            match (row.watts, row.hashes_per_joule()) {
                (Some(w), Some(e)) => line.push_str(&format!(" {w:>6.1}W {e:>7.0}")),
                _ => line.push_str(&format!(" {:>7} {:>7}", "-", "-")),
            }
        }
        match row.temperature {
            Some(t) => line.push_str(&format!(" {t:>3.0}°C")),
            None => line.push_str(&format!(" {:>5}", "-")),
        }
        if row.disturbed() {
            line.push_str(" !");
        }
        line
    };
    for (i, row) in outcome.rows.iter().enumerate() {
        let chosen = i == outcome.chosen;
        let line = format_row(i + 1, if chosen { "*" } else { " " }, row, base);
        out.raw(if chosen {
            out.paint(Color::Bold, line)
        } else {
            line
        });
    }
    if !outcome.screened.is_empty() {
        let base = outcome.screen_baseline.unwrap_or(base);
        out.raw(out.paint(
            Color::Dim,
            format!("{indent}     dropped after the short screening pass (not in the decision):"),
        ));
        for (i, row) in outcome.screened.iter().enumerate() {
            let line = format_row(outcome.rows.len() + i + 1, " ", row, base);
            out.raw(out.paint(Color::Dim, line));
        }
    }
    if let Some(foreign) = outcome.foreign().filter(|f| *f > DISTURBED) {
        out.raw(format!(
            "{indent}{} estimated CPU interference reached {:.0}% of accounted busy/steal time \
             (rows marked !; includes interrupts and VM steal time); \
             affected comparisons require separated speed ranges",
            out.paint(Color::Yellow, "warning:"),
            foreign * 100.0
        ));
    }
    let chosen = &outcome.rows[outcome.chosen];
    let decision = match outcome.verdict {
        Verdict::Switched => format!(
            "{}: {:+.1}% over the rules (won every round by more than {:.1}%)",
            chosen.label,
            (chosen.rate / base - 1.0) * 100.0,
            outcome.margin * 100.0
        ),
        Verdict::Confirmed => format!(
            "{} (the rules' choice, confirmed by repeated measurements)",
            chosen.label
        ),
        Verdict::Disturbed => format!(
            "{} (fallback; {} measured {:+.1}%, but the comparison was inconclusive under CPU interference)",
            chosen.label,
            outcome.rows[0].label,
            (outcome.rows[0].rate / base - 1.0) * 100.0
        ),
        Verdict::Kept => format!(
            "{} (fallback; repeated comparisons did not establish a winner with a {:.1}% margin)",
            chosen.label,
            outcome.margin * 100.0
        ),
    };
    out.raw(format!(
        "{indent}{} {decision}",
        out.paint(Color::Green, "chosen:")
    ));
    if let Some(e) = outcome.efficient {
        let row = &outcome.rows[e];
        out.raw(format!(
            "{indent}{} {} ({:.0} H/J, {} at {:.1} W)",
            out.paint(Color::Green, "most efficient:"),
            row.label,
            row.hashes_per_joule().unwrap_or(0.0),
            format_rate(row.rate),
            row.watts.unwrap_or(0.0)
        ));
    }
}

// ---------------------------------------------------------------------------
// Saved results

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedPick {
    pub recipe: Recipe,
    pub label: String,
    pub rate: f64,
    pub hashes_per_joule: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Saved {
    pub fingerprint: String,
    pub limits: Limits,
    pub tuned_at: String,
    pub unix: u64,
    pub seconds: f64,
    pub baseline_rate: f64,
    pub speed: SavedPick,
    pub efficiency: Option<SavedPick>,
    pub power: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct SavedFile {
    version: u32,
    entries: Vec<Saved>,
}

/// Identifies "the same machine and miner": CPU model and layout, OS, kernel and
/// tideminer version. Any change means results must be measured again.
pub fn fingerprint(topology: &Topology) -> String {
    format!(
        "{} | {} | tideminer {}",
        topology.summary(),
        crate::pow::KERNEL,
        env!("CARGO_PKG_VERSION")
    )
}

impl Saved {
    fn placements(&self, topology: &Topology, limits: &Limits) -> Result<Vec<Placement>> {
        ensure!(
            self.fingerprint == fingerprint(topology) && &self.limits == limits,
            "saved tuning does not match this machine and limits"
        );
        let recipe = self.speed.recipe;
        ensure!(
            recipe.threads <= allowed_threads(topology, limits)?,
            "saved tuning exceeds the thread limit"
        );
        match (recipe.gpu_hashes, limits.gpu) {
            (Some(hashes), Some(gpu)) => ensure!(
                hashes > 0
                    && hashes <= gpu.max_hashes
                    && gpu.hashes.is_none_or(|fixed| hashes == fixed),
                "saved tuning exceeds the GPU limits"
            ),
            (Some(_), None) => anyhow::bail!("saved tuning requires a GPU"),
            (None, Some(gpu)) => ensure!(
                gpu.hashes.is_none(),
                "saved tuning ignores the fixed GPU load"
            ),
            (None, None) => {}
        }
        let placements = build(topology, limits, recipe)?;
        ensure!(
            limits.lanes.is_none_or(|lanes| placements
                .iter()
                .filter(|p| p.gpu.is_none())
                .all(|p| p.lanes == lanes)),
            "saved tuning ignores the fixed lane count"
        );
        Ok(placements)
    }

    pub fn new(topology: &Topology, limits: &Limits, outcome: &Outcome) -> Self {
        let pick = |row: &Row| SavedPick {
            recipe: row.recipe,
            label: row.label.clone(),
            rate: row.rate,
            hashes_per_joule: row.hashes_per_joule(),
        };
        Self {
            fingerprint: fingerprint(topology),
            limits: limits.clone(),
            tuned_at: timestamp(),
            unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            seconds: outcome.seconds,
            baseline_rate: outcome.baseline().rate,
            speed: pick(&outcome.rows[outcome.chosen]),
            efficiency: outcome.efficient.map(|i| pick(&outcome.rows[i])),
            power: outcome.power.clone(),
        }
    }
}

/// Where results live, and who should own new files (the invoking user under sudo).
struct Store {
    path: PathBuf,
    /// Set only on Unix (sudo hands files back to the invoking user).
    #[cfg_attr(not(unix), allow(dead_code))]
    owner: Option<(u32, u32)>,
}

impl Store {
    fn locate() -> Option<Self> {
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut owner = None;
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut home = std::env::var_os("HOME").map(PathBuf::from);
        #[cfg(unix)]
        if crate::os::is_root() {
            let id = |name| std::env::var(name).ok()?.parse::<u32>().ok();
            if let (Some(uid), Some(gid)) = (id("SUDO_UID"), id("SUDO_GID")) {
                home = crate::os::home_dir_of(uid).or(home);
                owner = Some((uid, gid));
            }
        }
        let xdg = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && owner.is_none());
        let base = match xdg {
            Some(dir) => dir,
            None if cfg!(target_os = "macos") => home?.join("Library/Caches"),
            None if cfg!(windows) => PathBuf::from(std::env::var_os("LOCALAPPDATA")?),
            None => home?.join(".cache"),
        };
        Some(Self {
            path: base.join("tideminer").join("tune.json"),
            owner,
        })
    }

    fn load(&self) -> SavedFile {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, entry: Saved) -> Result<()> {
        let mut file = self.load();
        file.version = 1;
        file.entries
            .retain(|e| !(e.fingerprint == entry.fingerprint && e.limits == entry.limits));
        file.entries.push(entry);
        let dir = self.path.parent().context("cache path has no directory")?;
        let mut missing = Vec::new();
        let mut ancestor = Some(dir);
        while let Some(d) = ancestor.filter(|d| !d.exists()) {
            missing.push(d.to_path_buf());
            ancestor = d.parent();
        }
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        // Each writer needs its own staging file: sharing tune.json.tmp lets
        // concurrent miners truncate or rename one another's in-progress JSON.
        // Cache updates remain last-writer-wins, but every published file is whole.
        let bytes = serde_json::to_vec_pretty(&file)?;
        let (temporary, mut output) = create_cache_temporary(&self.path)?;
        let result = (|| -> Result<()> {
            use std::io::Write;
            output.write_all(&bytes)?;
            drop(output);
            std::fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.with_context(|| format!("write {}", self.path.display()))?;
        #[cfg(unix)]
        if let Some((uid, gid)) = self.owner {
            for path in missing.iter().chain(std::iter::once(&self.path)) {
                std::os::unix::fs::chown(path, Some(uid), Some(gid))
                    .with_context(|| format!("hand {} back to uid {uid}", path.display()))?;
            }
        }
        Ok(())
    }
}

fn create_cache_temporary(path: &Path) -> Result<(PathBuf, std::fs::File)> {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..128 {
        let temporary = path.with_extension(format!(
            "json.{}.{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Relaxed)
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(anyhow!(
        "could not create a unique tuning cache temporary file"
    ))
}

/// The saved `tideminer tune` result for this machine and these limits.
pub fn load_saved(topology: &Topology, limits: &Limits) -> Option<Saved> {
    let fingerprint = fingerprint(topology);
    let file = Store::locate()?.load();
    if file.version != 1 {
        return None;
    }
    file.entries
        .into_iter()
        .filter(|e| e.fingerprint == fingerprint && &e.limits == limits)
        .max_by_key(|e| e.unix)
}

/// Save for normal mining startup; returns the file written.
pub fn save(entry: Saved) -> Result<PathBuf> {
    let store = Store::locate().context("no home directory to save tuning results in")?;
    store.save(entry)?;
    Ok(store.path)
}

/// Mining-flag equivalent of a configuration, e.g. `--cpus 0,2,4 --lanes 2`.
pub fn equivalent_flags(topology: &Topology, placements: &[Placement]) -> Option<String> {
    let gpu = placements.iter().find_map(|p| p.gpu);
    let placements: Vec<Placement> = placements
        .iter()
        .filter(|p| p.gpu.is_none())
        .cloned()
        .collect();
    let ids: Vec<usize> = placements.iter().map(|p| p.cpu).collect::<Option<_>>()?;
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < ids.len() {
        let mut j = i;
        while j + 1 < ids.len() && ids[j + 1] == ids[j] + 1 {
            j += 1;
        }
        ranges.push(if i == j {
            ids[i].to_string()
        } else {
            format!("{}-{}", ids[i], ids[j])
        });
        i = j + 1;
    }
    let lanes: Vec<usize> = placements.iter().map(|p| p.lanes).collect();
    let mut auto = placements.clone();
    engine::auto_lanes(&mut auto, topology);
    let lanes = if auto.iter().map(|p| p.lanes).eq(lanes.iter().copied()) {
        "auto".to_string()
    } else if lanes.iter().all(|&l| l == lanes[0]) {
        lanes[0].to_string()
    } else {
        return None;
    };
    let gpu = gpu.map_or(String::new(), |g| {
        format!(" --gpu --gpu-hashes {}", g.hashes)
    });
    Some(format!("--cpus {} --lanes {lanes}{gpu}", ranges.join(",")))
}

/// Reuse a compatible offline result without running measurements. Invalid or
/// obsolete cached recipes are ignored so they cannot prevent mining startup.
pub fn saved_placements(
    topology: &Topology,
    limits: &Limits,
    out: &Reporter,
) -> Option<Vec<Placement>> {
    let saved = load_saved(topology, limits)?;
    match saved.placements(topology, limits) {
        Ok(placements) => {
            out.line(
                "tuning",
                Color::Cyan,
                format!(
                    "using saved `tideminer tune` from {}: {}, {} ({:+.1}% over the rules)",
                    saved.tuned_at,
                    label(&placements),
                    format_rate(saved.speed.rate),
                    (saved.speed.rate / saved.baseline_rate - 1.0) * 100.0
                ),
            );
            Some(placements)
        }
        Err(error) => {
            out.line(
                "warning",
                Color::Yellow,
                format!("ignoring invalid saved tuning: {error:#}"),
            );
            None
        }
    }
}

/// `--autotune`: the saved offline result for these limits if there is one,
/// otherwise a quick comparison now. Prints what it did; returns the placements.
pub fn autotune(
    topology: &Topology,
    limits: &Limits,
    nice: Option<i32>,
    out: &Reporter,
) -> Result<Vec<Placement>> {
    let heading = |text: String| {
        out.raw(format!(
            "  {} {text}",
            out.paint(Color::Cyan, format!("{:<8}", "autotune"))
        ));
    };
    if let Some(placements) = saved_placements(topology, limits, out) {
        return Ok(placements);
    }
    let count = quick_candidates(topology, limits)?.len();
    if count < 2 {
        heading("only one configuration fits these limits; nothing to compare".into());
        return build(
            topology,
            limits,
            quick_candidates(topology, limits)?[0].recipe,
        );
    }
    heading(format!(
        "comparing {count} configurations, ~{} (offline and longer: `tideminer tune`)",
        format_duration(Duration::from_secs_f64(quick_seconds(count)))
    ));
    let outcome = quick(topology, limits, nice)?.context("no configurations to compare")?;
    print_outcome(out, &outcome, "           ");
    build(topology, limits, outcome.rows[outcome.chosen].recipe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::Cpu;

    /// 2 P cores with SMT (cpus 0-3), 4 E cores in two L2 groups (cpus 4-7).
    fn hybrid() -> Topology {
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

    fn free() -> Limits {
        Limits {
            layout: Layout::All,
            threads: None,
            cpus: None,
            lanes: None,
            gpu: None,
        }
    }

    fn labels(c: &[Candidate]) -> Vec<String> {
        c.iter().map(|c| label(&c.placements)).collect()
    }

    #[test]
    fn labels_are_short_and_exact() {
        let t = hybrid();
        let all = engine::plan(&t, Layout::All, None, None).unwrap();
        assert_eq!(label(&all), "2 P + 2 SMT + 4 E, 1 lane");
        let mut p = engine::plan(&t, Layout::Physical, None, None).unwrap();
        p[0].lanes = 2;
        p[1].lanes = 2;
        assert_eq!(label(&p), "2 P + 4 E, 2 lanes on P");
        p[2].lanes = 2;
        assert_eq!(label(&p), "2 P + 4 E, 2 lanes on 2 P + 1 E");
    }

    #[test]
    fn quick_candidates_keep_the_thread_limit_and_start_with_the_rules() {
        let t = hybrid();
        // All 8 CPUs: the rules (1 lane everywhere) first, then E cores paired,
        // and the P cores' SMT threads merged into second lanes.
        let c = quick_candidates(&t, &free()).unwrap();
        assert_eq!(
            labels(&c),
            [
                "2 P + 2 SMT + 4 E, 1 lane",
                "2 P + 2 SMT + 4 E, 2 lanes on E",
                "2 P + 4 E, 2 lanes on P",
                "2 P + 4 E, 2 lanes",
            ]
        );
        // P cores only, two threads: the rules already pair them; 1 lane is the rival.
        let limits = Limits {
            layout: Layout::Performance,
            threads: Some(2),
            ..free()
        };
        let c = quick_candidates(&t, &limits).unwrap();
        assert_eq!(labels(&c), ["2 P, 2 lanes", "2 P, 1 lane"]);
        // A fixed lane count is respected.
        let limits = Limits {
            lanes: Some(1),
            ..free()
        };
        assert!(
            quick_candidates(&t, &limits)
                .unwrap()
                .iter()
                .all(|c| c.placements.iter().all(|p| p.lanes == 1))
        );
    }

    #[test]
    fn offline_counts_follow_cpu_classes() {
        let t = hybrid();
        // Baseline (all 8) first, then P cores, P + SMT, one E per cluster.
        assert_eq!(thread_counts(&t, &free()).unwrap(), [8, 2, 4, 6]);
        let limits = Limits {
            threads: Some(5),
            ..free()
        };
        assert_eq!(thread_counts(&t, &limits).unwrap(), [5, 2, 4]);
        let c = offline_candidates(&t, &free()).unwrap();
        assert_eq!(label(&c[0].placements), "2 P + 2 SMT + 4 E, 1 lane");
        let unique: HashSet<_> = c.iter().map(|c| signature(&c.placements)).collect();
        assert_eq!(unique.len(), c.len());
    }

    #[test]
    fn efficiency_first_and_merge_recipes() {
        let t = hybrid();
        let recipe = Recipe {
            threads: 4,
            efficiency_first: true,
            merge_smt: false,
            lanes: LanePolicy::One,
            gpu_hashes: None,
        };
        let p = build(&t, &free(), recipe).unwrap();
        assert_eq!(label(&p), "2 P + 2 E, 1 lane");
        let p = build(
            &t,
            &free(),
            Recipe {
                efficiency_first: false,
                merge_smt: true,
                ..recipe
            },
        )
        .unwrap();
        assert_eq!(label(&p), "2 P, 2 lanes");
        assert_eq!(
            equivalent_flags(&t, &p).as_deref(),
            Some("--cpus 0,2 --lanes auto")
        );
    }

    fn row(label: &str, baseline: bool, rates: &[f64]) -> Row {
        let rate = rates.iter().sum::<f64>() / rates.len() as f64;
        let lo = rates.iter().copied().fold(f64::MAX, f64::min);
        let hi = rates.iter().copied().fold(0.0, f64::max);
        Row {
            label: label.into(),
            recipe: Recipe {
                threads: 1,
                efficiency_first: false,
                merge_smt: false,
                lanes: LanePolicy::Auto,
                gpu_hashes: None,
            },
            threads: 1,
            hashes_in_flight: 1,
            baseline,
            rates: rates.to_vec(),
            rate,
            spread: (hi - lo) / rate,
            watts: None,
            temperature: None,
            foreign: None,
            screened_out: false,
        }
    }

    #[test]
    fn epyc_two_lanes_win_and_can_be_saved_despite_interference() {
        // Regression: a real EPYC 9V74 run used to discard this 12.9% gain.
        let mut rules = row("1 lane", true, &[5880.0, 6060.0, 5980.0]);
        let mut faster = row("2 lanes", false, &[7050.0, 6800.0, 6380.0]);
        rules.foreign = Some(0.11);
        faster.foreign = Some(0.11);
        let outcome = decide(vec![rules, faster], OFFLINE_MARGIN, None, 1.0);
        assert_eq!(outcome.verdict, Verdict::Switched);
        assert_eq!(outcome.rows[outcome.chosen].label, "2 lanes");
        assert!(outcome.trustworthy());
        let saved = Saved::new(&hybrid(), &free(), &outcome);
        assert_eq!(saved.speed.label, "2 lanes");
    }

    #[test]
    fn interference_requires_separated_ranges_for_the_compared_candidates() {
        let rules = row("rules", true, &[90.0, 100.0, 110.0]);
        let faster = row("b", false, &[100.0, 110.0, 120.0]);
        for disturbed_index in [0, 1] {
            let mut rows = vec![rules.clone(), faster.clone()];
            rows[disturbed_index].foreign = Some(0.11);
            let outcome = decide(rows, OFFLINE_MARGIN, None, 1.0);
            assert_eq!(outcome.verdict, Verdict::Disturbed);
            assert!(outcome.rows[outcome.chosen].baseline);
            assert!(!outcome.trustworthy());
        }
        // Interference on an unrelated, slower candidate changes neither the
        // evidence comparing the winner and baseline nor permission to save.
        let mut slower = row("slow", false, &[40.0, 80.0, 60.0]);
        slower.foreign = Some(0.50);
        let outcome = decide(vec![rules, faster, slower], OFFLINE_MARGIN, None, 1.0);
        assert_eq!(outcome.verdict, Verdict::Switched);
        assert!(outcome.trustworthy());
    }

    #[test]
    fn a_large_mean_gain_cannot_hide_a_losing_round() {
        for interference in [None, Some(0.11)] {
            let rules = row("rules", true, &[100.0, 100.0, 100.0]);
            let mut faster = row("b", false, &[150.0, 140.0, 99.0]);
            faster.foreign = interference;
            let outcome = decide(vec![rules, faster], OFFLINE_MARGIN, None, 1.0);
            assert!(outcome.rows[outcome.chosen].baseline);
            assert!(!outcome.trustworthy());
        }
    }

    #[test]
    fn a_noisy_fastest_mean_does_not_hide_a_repeatable_improvement() {
        let outcome = decide(
            vec![
                row("rules", true, &[100.0, 100.0, 100.0]),
                row("spike", false, &[200.0, 200.0, 99.0]),
                row("steady", false, &[120.0, 121.0, 119.0]),
            ],
            OFFLINE_MARGIN,
            None,
            1.0,
        );
        assert_eq!(outcome.rows[0].label, "spike");
        assert_eq!(outcome.rows[outcome.chosen].label, "steady");
        assert_eq!(outcome.verdict, Verdict::Switched);
        assert!(outcome.trustworthy());
    }

    #[test]
    fn interference_does_not_prevent_confirming_the_baseline() {
        let mut rules = row("rules", true, &[120.0, 121.0, 119.0]);
        rules.foreign = Some(0.25);
        let outcome = decide(
            vec![rules, row("b", false, &[100.0, 101.0, 99.0])],
            OFFLINE_MARGIN,
            None,
            1.0,
        );
        assert_eq!(outcome.verdict, Verdict::Confirmed);
        assert!(outcome.trustworthy());
    }

    #[test]
    fn small_gaps_under_interference_stay_inconclusive() {
        let mut faster = row("b", false, &[104.0, 106.0]);
        faster.foreign = Some(0.11);
        let outcome = decide(
            vec![row("rules", true, &[100.0, 103.0]), faster],
            OFFLINE_MARGIN,
            None,
            1.0,
        );
        // Every paired round clears 1.5%, but 104 / 103 does not.
        assert_eq!(outcome.verdict, Verdict::Disturbed);
        assert!(!outcome.trustworthy());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cpu_accounting_reads_proc() {
        assert!(busy_ticks(&[0]).is_some());
        assert!(own_ticks().is_some());
    }

    #[test]
    fn a_challenger_must_win_every_round_by_the_minimum_margin() {
        let clear = decide(
            vec![
                row("rules", true, &[100.0, 101.0]),
                row("b", false, &[120.0, 121.0]),
            ],
            0.03,
            None,
            1.0,
        );
        assert_eq!(clear.verdict, Verdict::Switched);
        assert_eq!(clear.rows[clear.chosen].label, "b");
        let close = decide(
            vec![
                row("rules", true, &[100.0, 101.0]),
                row("b", false, &[102.0, 103.0]),
            ],
            0.03,
            None,
            1.0,
        );
        assert_eq!(close.verdict, Verdict::Kept);
        assert_eq!(close.rows[close.chosen].label, "rules");
        assert!(!close.trustworthy());
        // Common drift is not uncertainty in the relative gain.
        let drift = decide(
            vec![
                row("rules", true, &[90.0, 110.0]),
                row("b", false, &[98.0, 122.0]),
            ],
            0.03,
            None,
            1.0,
        );
        assert_eq!(drift.verdict, Verdict::Switched);
        assert!(drift.trustworthy());
        let confirmed = decide(
            vec![
                row("rules", true, &[130.0, 131.0]),
                row("b", false, &[100.0, 100.0]),
            ],
            0.03,
            None,
            1.0,
        );
        assert_eq!(confirmed.verdict, Verdict::Confirmed);
        assert!(confirmed.trustworthy());
    }

    #[test]
    fn insufficient_or_invalid_measurements_cannot_establish_a_winner() {
        for rates in [
            vec![],
            vec![120.0],
            vec![120.0, 0.0],
            vec![120.0, f64::NAN],
            vec![120.0, f64::INFINITY],
            vec![120.0, -1.0],
            vec![120.0, 120.0, 120.0],
        ] {
            let outcome = decide(
                vec![row("rules", true, &[100.0, 100.0]), row("b", false, &rates)],
                OFFLINE_MARGIN,
                None,
                1.0,
            );
            assert!(outcome.rows[outcome.chosen].baseline);
            assert!(!outcome.trustworthy());
        }
        let fixed = decide(
            vec![row("rules", true, &[100.0, 101.0, 100.0])],
            OFFLINE_MARGIN,
            None,
            1.0,
        );
        assert_eq!(fixed.verdict, Verdict::Confirmed);
        assert!(fixed.trustworthy());
    }

    #[test]
    fn saved_recipes_respect_cpu_and_gpu_limits() {
        let t = hybrid();
        let limits = Limits {
            cpus: Some(vec![0, 2]),
            lanes: Some(1),
            ..free()
        };
        let outcome = decide(vec![row("rules", true, &[100.0])], 0.03, None, 1.0);
        let mut saved = Saved::new(&t, &limits, &outcome);
        saved.speed.recipe.threads = 2;
        saved.speed.recipe.lanes = LanePolicy::One;
        let placements = saved.placements(&t, &limits).unwrap();
        assert_eq!(
            placements.iter().filter_map(|p| p.cpu).collect::<Vec<_>>(),
            [0, 2]
        );
        assert!(placements.iter().all(|p| p.lanes == 1));
        saved.speed.recipe.threads = 3;
        assert!(saved.placements(&t, &limits).is_err());
        saved.speed.recipe.threads = 2;
        saved.speed.recipe.gpu_hashes = Some(96);
        assert!(saved.placements(&t, &limits).is_err());
        let gpu_limits = Limits {
            gpu: Some(GpuLimits {
                hashes: Some(96),
                threadgroup: 32,
                cores: 30,
                max_hashes: 4000,
            }),
            ..limits
        };
        saved.limits = gpu_limits.clone();
        assert_eq!(
            saved
                .placements(&t, &gpu_limits)
                .unwrap()
                .last()
                .unwrap()
                .gpu
                .unwrap()
                .hashes,
            96
        );
        for hashes in [None, Some(0), Some(95), Some(4001)] {
            saved.speed.recipe.gpu_hashes = hashes;
            assert!(saved.placements(&t, &gpu_limits).is_err());
        }
    }

    #[test]
    fn saved_results_round_trip_by_machine_and_limits() {
        let t = hybrid();
        let dir = std::env::temp_dir().join(format!("tideminer-tune-{}", std::process::id()));
        let store = Store {
            path: dir.join("tideminer").join("tune.json"),
            owner: None,
        };
        let outcome = decide(
            vec![
                row("rules", true, &[100.0, 101.0]),
                row("b", false, &[130.0, 131.0]),
            ],
            0.03,
            None,
            1.0,
        );
        store.save(Saved::new(&t, &free(), &outcome)).unwrap();
        let limited = Limits {
            threads: Some(2),
            ..free()
        };
        store.save(Saved::new(&t, &limited, &outcome)).unwrap();
        store.save(Saved::new(&t, &free(), &outcome)).unwrap();
        let file = store.load();
        assert_eq!(file.entries.len(), 2);
        assert!(file.entries.iter().any(|e| e.limits == limited));
        assert_eq!(file.entries[0].speed.label, "b");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_cache_writers_publish_complete_files() {
        let dir =
            std::env::temp_dir().join(format!("tideminer-tune-concurrent-{}", std::process::id()));
        let store = Store {
            path: dir.join("tune.json"),
            owner: None,
        };
        let t = hybrid();
        let outcome = decide(vec![row("rules", true, &[100.0])], 0.03, None, 1.0);
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    barrier.wait();
                    for _ in 0..8 {
                        store.save(Saved::new(&t, &free(), &outcome)).unwrap();
                        let bytes = std::fs::read(&store.path).unwrap();
                        let file: SavedFile = serde_json::from_slice(&bytes).unwrap();
                        assert_eq!(file.entries.len(), 1);
                    }
                });
            }
        });
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn quick_run_measures_every_candidate() {
        let t = Topology::uniform(2);
        let limits = Limits {
            threads: Some(1),
            ..free()
        };
        let candidates = quick_candidates(&t, &limits).unwrap();
        assert_eq!(labels(&candidates), ["1 core, 2 lanes", "1 core, 1 lane"]);
        let probe = Probe::new(None, false);
        let schedule = Schedule {
            rounds: 2,
            settle: 0.05,
            seconds: 0.3,
        };
        let rows = run_rounds(
            &probe,
            &candidates,
            &candidates[0].placements,
            schedule,
            &mut |_, _, _, _| {},
        )
        .unwrap();
        assert!(rows.iter().all(|r| r.rates.len() == 2 && r.rate > 0.0));
        assert!(rows[0].baseline && !rows[1].baseline);
    }

    #[test]
    fn gpu_loads_are_compared_around_the_default() {
        let t = hybrid();
        let gpu = GpuLimits {
            hashes: None,
            threadgroup: 32,
            cores: 30,
            max_hashes: 4000,
        };
        let limits = Limits {
            gpu: Some(gpu),
            ..free()
        };
        let c = quick_candidates(&t, &limits).unwrap();
        // Baseline: the rules plus the default load; then CPU variants at that load,
        // then the baseline's CPUs with the GPU off and at 2, 4, 8, 10 per core.
        assert_eq!(
            label(&c[0].placements),
            "2 P + 2 SMT + 4 E, 1 lane + GPU 180"
        );
        let loads: Vec<Option<usize>> = c.iter().map(|c| c.recipe.gpu_hashes).collect();
        for load in [None, Some(60), Some(120), Some(180), Some(240), Some(300)] {
            assert!(loads.contains(&load), "{load:?} missing");
        }
        let off = c.iter().find(|c| c.recipe.gpu_hashes.is_none()).unwrap();
        assert!(off.placements.iter().all(|p| p.gpu.is_none()));
        let unique: HashSet<_> = c.iter().map(|c| signature(&c.placements)).collect();
        assert_eq!(unique.len(), c.len());
        // A fixed --gpu-hashes is used as is.
        let fixed = Limits {
            gpu: Some(GpuLimits {
                hashes: Some(96),
                ..gpu
            }),
            ..free()
        };
        let c = quick_candidates(&t, &fixed).unwrap();
        assert!(c.iter().all(|c| c.recipe.gpu_hashes == Some(96)));
    }

    #[test]
    fn gpu_only_compares_loads_but_never_turns_the_gpu_off() {
        let t = hybrid();
        let limits = Limits {
            threads: Some(0),
            gpu: Some(GpuLimits {
                hashes: None,
                threadgroup: 32,
                cores: 10,
                max_hashes: 4000,
            }),
            ..free()
        };
        let c = quick_candidates(&t, &limits).unwrap();
        assert_eq!(label(&c[0].placements), "GPU only 60");
        assert!(c.len() > 1);
        for candidate in &c {
            assert_eq!(candidate.placements.len(), 1);
            assert!(candidate.placements[0].gpu.is_some());
        }
        // Without --gpu, 0 threads is still refused.
        let cpu_only = Limits {
            threads: Some(0),
            ..free()
        };
        assert!(quick_candidates(&t, &cpu_only).is_err());
    }
}
