//! What CPU are we on: logical CPUs we may use, physical cores, SMT siblings,
//! shared L2 groups and hybrid core classes. Unprivileged reads only; every
//! source is optional and failures degrade to "uniform cores".
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CoreKind {
    Performance,
    Efficiency,
    /// Uniform or undetectable; treated like performance cores.
    Unknown,
    /// The GPU worker (`--gpu`, macOS); never part of the CPU topology.
    Gpu,
}

impl CoreKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Performance => "P",
            Self::Efficiency => "E",
            Self::Unknown => "C",
            Self::Gpu => "GPU",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Cpu {
    /// OS logical CPU number; `None` where the OS offers no pinning (macOS).
    pub id: Option<usize>,
    pub kind: CoreKind,
    /// Physical core index (dense, 0-based).
    pub core: usize,
    /// 0 for the first hardware thread of a core, 1 for its SMT sibling, ...
    pub smt_rank: usize,
    /// Dense index of the L2 cache group, when known.
    pub l2_group: Option<usize>,
    pub max_mhz: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Topology {
    pub model: String,
    pub os: &'static str,
    pub arch: &'static str,
    pub features: Vec<&'static str>,
    pub pinning: bool,
    pub cpus: Vec<Cpu>,
    pub l2_bytes: BTreeMap<usize, u64>,
    /// Largest L1 data cache a worker may run on (known on macOS, where workers
    /// cannot be pinned and so may land on any core).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_l1d_bytes: Option<u64>,
}

impl Topology {
    pub fn detect() -> Self {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let Some(t) = linux::detect() {
            return t;
        }
        #[cfg(target_os = "macos")]
        if let Some(t) = macos::detect() {
            return t;
        }
        Self::uniform(std::thread::available_parallelism().map_or(1, usize::from))
    }

    pub fn uniform(count: usize) -> Self {
        Self {
            model: String::from("unknown"),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            features: features(),
            pinning: false,
            cpus: (0..count)
                .map(|i| Cpu {
                    id: None,
                    kind: CoreKind::Unknown,
                    core: i,
                    smt_rank: 0,
                    l2_group: None,
                    max_mhz: None,
                })
                .collect(),
            l2_bytes: BTreeMap::new(),
            max_l1d_bytes: None,
        }
    }

    pub fn count(&self, kind: CoreKind) -> usize {
        self.cpus.iter().filter(|c| c.kind == kind).count()
    }

    pub fn physical_cores(&self) -> usize {
        self.cpus.iter().filter(|c| c.smt_rank == 0).count()
    }

    pub fn is_hybrid(&self) -> bool {
        self.count(CoreKind::Performance) > 0 && self.count(CoreKind::Efficiency) > 0
    }

    pub fn summary(&self) -> String {
        let p = self.count(CoreKind::Performance);
        let e = self.count(CoreKind::Efficiency);
        let smt = self.cpus.iter().filter(|c| c.smt_rank > 0).count();
        let kinds = if self.is_hybrid() {
            format!("{p} P + {e} E logical")
        } else {
            format!("{} logical", self.cpus.len())
        };
        format!(
            "{} | {} | {} physical cores, {} SMT siblings | {} {}{}",
            self.model,
            kinds,
            self.physical_cores(),
            smt,
            self.os,
            self.arch,
            if self.pinning { "" } else { " (no pinning)" }
        )
    }
}

fn features() -> Vec<&'static str> {
    let mut out = Vec::new();
    #[cfg(target_arch = "x86_64")]
    {
        macro_rules! probe {
            ($($f:tt),*) => {$( if std::arch::is_x86_feature_detected!($f) { out.push($f); } )*};
        }
        probe!(
            "sse2", "ssse3", "sse4.1", "avx", "avx2", "avx512f", "avx512vl", "sha"
        );
    }
    #[cfg(target_arch = "aarch64")]
    {
        macro_rules! probe {
            ($($f:tt),*) => {$( if std::arch::is_aarch64_feature_detected!($f) { out.push($f); } )*};
        }
        probe!("neon", "sha2", "sve");
    }
    out
}

/// Parse a kernel CPU list such as `0-3,8,10-11`.
pub fn parse_cpu_list(text: &str) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for part in text.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (
                    a.trim().parse::<usize>().ok()?,
                    b.trim().parse::<usize>().ok()?,
                );
                if a > b || b - a > 65536 {
                    return None;
                }
                out.extend(a..=b);
            }
            None => out.push(part.trim().parse().ok()?),
        }
    }
    Some(out)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux {
    use super::*;
    use std::fs;

    fn read(path: &str) -> Option<String> {
        fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
    }

    fn allowed_cpus() -> Option<Vec<usize>> {
        let status = fs::read_to_string("/proc/self/status").ok()?;
        let line = status
            .lines()
            .find(|l| l.starts_with("Cpus_allowed_list:"))?;
        parse_cpu_list(line.split_once(':')?.1)
    }

    fn model() -> String {
        fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|info| {
                info.lines()
                    .find(|l| l.starts_with("model name") || l.starts_with("Model"))
                    .and_then(|l| l.split_once(':'))
                    .map(|(_, v)| v.trim().to_owned())
            })
            .unwrap_or_else(|| "unknown".into())
    }

    fn cache_parse_size(text: &str) -> Option<u64> {
        let text = text.trim();
        let (digits, scale) = match text.chars().last()? {
            'K' => (&text[..text.len() - 1], 1024),
            'M' => (&text[..text.len() - 1], 1024 * 1024),
            _ => (text, 1),
        };
        digits.parse::<u64>().ok().map(|v| v * scale)
    }

    pub fn detect() -> Option<Topology> {
        let base = "/sys/devices/system/cpu";
        let allowed = allowed_cpus()
            .or_else(|| read(&format!("{base}/online")).and_then(|s| parse_cpu_list(&s)))?;
        if allowed.is_empty() {
            return None;
        }
        // Intel hybrid exposes one PMU per core type.
        let p_list = read("/sys/devices/cpu_core/cpus").and_then(|s| parse_cpu_list(&s));
        let e_list = read("/sys/devices/cpu_atom/cpus").and_then(|s| parse_cpu_list(&s));

        struct Raw {
            id: usize,
            siblings: String,
            capacity: Option<u32>,
            max_khz: Option<u32>,
            l2: Option<(String, Option<u64>)>,
        }
        let raw: Vec<Raw> = allowed
            .iter()
            .map(|&id| {
                let dir = format!("{base}/cpu{id}");
                let siblings = read(&format!("{dir}/topology/core_cpus_list"))
                    .or_else(|| read(&format!("{dir}/topology/thread_siblings_list")))
                    .unwrap_or_else(|| id.to_string());
                let mut l2 = None;
                for index in 0..8 {
                    let cache = format!("{dir}/cache/index{index}");
                    if read(&format!("{cache}/level")).as_deref() == Some("2") {
                        if let Some(shared) = read(&format!("{cache}/shared_cpu_list")) {
                            let size =
                                read(&format!("{cache}/size")).and_then(|s| cache_parse_size(&s));
                            l2 = Some((shared, size));
                        }
                        break;
                    }
                }
                Raw {
                    id,
                    siblings,
                    capacity: read(&format!("{dir}/cpu_capacity")).and_then(|s| s.parse().ok()),
                    max_khz: read(&format!("{dir}/cpufreq/cpuinfo_max_freq"))
                        .and_then(|s| s.parse().ok()),
                    l2,
                }
            })
            .collect();

        let kind_of = |r: &Raw| -> CoreKind {
            if let (Some(p), Some(e)) = (&p_list, &e_list) {
                if p.contains(&r.id) {
                    return CoreKind::Performance;
                }
                if e.contains(&r.id) {
                    return CoreKind::Efficiency;
                }
            }
            // Generic heterogeneous systems (ARM big.LITTLE, others): capacity, then max clock.
            let caps: Vec<u32> = raw.iter().filter_map(|r| r.capacity).collect();
            if caps.len() == raw.len() && caps.iter().min() != caps.iter().max() {
                let top = *caps.iter().max().unwrap();
                return if r.capacity == Some(top) {
                    CoreKind::Performance
                } else {
                    CoreKind::Efficiency
                };
            }
            let clocks: Vec<u32> = raw.iter().filter_map(|r| r.max_khz).collect();
            if clocks.len() == raw.len() {
                let (lo, hi) = (*clocks.iter().min().unwrap(), *clocks.iter().max().unwrap());
                // Treat >15% max-clock spread as distinct classes (turbo-bin noise is smaller).
                if hi as f64 > lo as f64 * 1.15 {
                    return if r.max_khz.unwrap_or(0) as f64 > (lo as f64 + hi as f64) / 2.0 {
                        CoreKind::Performance
                    } else {
                        CoreKind::Efficiency
                    };
                }
            }
            CoreKind::Unknown
        };

        let mut core_index: BTreeMap<String, usize> = BTreeMap::new();
        let mut l2_index: BTreeMap<String, usize> = BTreeMap::new();
        let mut l2_bytes = BTreeMap::new();
        let mut cpus = Vec::with_capacity(raw.len());
        for r in &raw {
            let next = core_index.len();
            let core = *core_index.entry(r.siblings.clone()).or_insert(next);
            let smt_rank = parse_cpu_list(&r.siblings)
                .and_then(|s| s.iter().position(|&c| c == r.id))
                .unwrap_or(0);
            let l2_group = r.l2.as_ref().map(|(shared, size)| {
                let next = l2_index.len();
                let group = *l2_index.entry(shared.clone()).or_insert(next);
                if let Some(size) = size {
                    l2_bytes.insert(group, *size);
                }
                group
            });
            cpus.push(Cpu {
                id: Some(r.id),
                kind: kind_of(r),
                core,
                smt_rank,
                l2_group,
                max_mhz: r.max_khz.map(|k| k / 1000),
            });
        }
        Some(Topology {
            model: model(),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            features: features(),
            pinning: true,
            cpus,
            l2_bytes,
            max_l1d_bytes: None,
        })
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn cache_sizes() {
            assert_eq!(super::cache_parse_size("2048K"), Some(2 << 20));
            assert_eq!(super::cache_parse_size("4M"), Some(4 << 20));
            assert_eq!(super::cache_parse_size("x"), None);
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use crate::os::{sysctl_string, sysctl_u32};

    /// Apple Silicon reports performance levels: perflevel0 = P, perflevel1 = E.
    /// Logical CPU IDs are not exposed and threads cannot be pinned.
    pub fn detect() -> Option<Topology> {
        let total = sysctl_u32("hw.logicalcpu")? as usize;
        let levels = sysctl_u32("hw.nperflevels").unwrap_or(1);
        let p = if levels >= 2 {
            sysctl_u32("hw.perflevel0.logicalcpu").unwrap_or(total as u32) as usize
        } else {
            total
        };
        let physical = sysctl_u32("hw.physicalcpu").unwrap_or(total as u32) as usize;
        let smt = total > physical;
        let model = sysctl_string("machdep.cpu.brand_string").unwrap_or_else(|| "Apple".into());
        let max_l1d_bytes = (0..levels)
            .filter_map(|l| sysctl_u32(&format!("hw.perflevel{l}.l1dcachesize")))
            .chain(sysctl_u32("hw.l1dcachesize"))
            .max()
            .map(u64::from);
        let cpus = (0..total)
            .map(|i| Cpu {
                id: None,
                kind: if levels < 2 {
                    CoreKind::Unknown
                } else if i < p {
                    CoreKind::Performance
                } else {
                    CoreKind::Efficiency
                },
                core: if smt { i / 2 } else { i },
                smt_rank: if smt { i % 2 } else { 0 },
                l2_group: None,
                max_mhz: None,
            })
            .collect();
        Some(Topology {
            model,
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            features: features(),
            pinning: false,
            cpus,
            l2_bytes: BTreeMap::new(),
            max_l1d_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists() {
        assert_eq!(
            parse_cpu_list("0-3,8,10-11"),
            Some(vec![0, 1, 2, 3, 8, 10, 11])
        );
        assert_eq!(parse_cpu_list("5"), Some(vec![5]));
        assert_eq!(parse_cpu_list("\n"), Some(vec![]));
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("a"), None);
    }

    #[test]
    fn detection_finds_at_least_one_cpu() {
        let t = Topology::detect();
        assert!(!t.cpus.is_empty());
        assert!(!t.summary().is_empty());
    }
}
