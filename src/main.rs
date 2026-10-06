use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tideminer::engine::{self, Layout, Placement};
use tideminer::report::{Color, Reporter};
use tideminer::topology::{Topology, parse_cpu_list};
use tideminer::transport::TlsSettings;
use tideminer::{benchmark, miner, pow, stratum, tune};

/// Tidecoin yespower miner (yespower 1.0, N=2048, r=8).
#[derive(Parser)]
#[command(
    version,
    args_conflicts_with_subcommands = true,
    arg_required_else_help = true,
    after_help = EXAMPLES
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    mine: MineArgs,
}

const EXAMPLES: &str = "\
Examples:
  tideminer -o POOL:PORT -u ADDRESS.rig                  mine (plain TCP)
  tideminer -o stratum+tls://POOL:PORT -u ADDRESS.rig    mine over TLS
  tideminer -o POOL:PORT --tls -u ADDRESS.rig            the same, TLS by flag
  tideminer -o POOL:PORT -u ADDRESS.rig -t 8 --autotune  8 threads, tuned first
  tideminer tune                                         offline tuning table
  tideminer topology -t 8                                where 8 workers would run

Mining automatically reuses saved tuning for the same machine and CPU/GPU limits.
With no matching result, --autotune runs a quick comparison; otherwise mining starts with built-in rules.";

#[derive(Args, Clone)]
struct CpuArgs {
    /// Worker threads; default uses every CPU of the layout. CPUs are taken in
    /// priority order (P cores, their SMT siblings, then E cores).
    #[cfg_attr(
        all(target_os = "macos", feature = "gpu"),
        doc = "`-t 0 --gpu` mines on the GPU only."
    )]
    #[arg(short = 't', long)]
    threads: Option<usize>,
    /// Which CPUs to use.
    #[arg(long, value_enum, default_value_t = Layout::All)]
    layout: Layout,
    /// Explicit logical CPU list, e.g. `0-15,16,20` (Linux). Overrides --layout.
    #[arg(long, conflicts_with = "cpu_affinity")]
    cpus: Option<String>,
    /// cpuminer-style CPU mask, e.g. 0xFFFF for CPUs 0-15 (Linux).
    #[arg(long, value_name = "MASK")]
    cpu_affinity: Option<String>,
    /// Hashes interleaved per worker: auto, 1 or 2. Auto gives 2 to workers whose
    /// core would otherwise idle (no busy SMT sibling, E cluster not shared) while
    /// the hashes in flight stay within the logical CPU count; else 1.
    #[arg(long, value_name = "auto|1|2", value_parser = parse_lanes)]
    lanes: Option<Lanes>,
    /// Worker nice value 0..=19 (Unix); higher keeps the desktop more responsive.
    #[arg(long, value_parser = clap::value_parser!(i32).range(0..=19), conflicts_with = "cpu_priority")]
    nice: Option<i32>,
    /// cpuminer priority 0 (idle) .. 5 (highest); raising above normal needs
    /// privileges, so 2-5 all mean normal.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=5))]
    cpu_priority: Option<u8>,
    /// Also mine on the Mac's GPU (Metal), next to the CPU workers. The GPU shares
    /// the chip's memory and power with the CPU: a light load adds hashes, a heavy
    /// one costs more than it adds. `tideminer tune --gpu` finds this Mac's load.
    #[cfg(all(target_os = "macos", feature = "gpu"))]
    #[arg(long)]
    gpu: bool,
    /// Hashes in flight on the GPU (default: the tuned value, else 6 per GPU core).
    #[cfg(all(target_os = "macos", feature = "gpu"))]
    #[arg(long, value_name = "N", requires = "gpu", value_parser = clap::value_parser!(u32).range(1..))]
    gpu_hashes: Option<u32>,
    /// GPU threads per threadgroup: a multiple of 8 (8 threads per hash).
    #[cfg(all(target_os = "macos", feature = "gpu"))]
    #[arg(long, value_name = "N", requires = "gpu", default_value_t = tideminer::gpu::DEFAULT_THREADGROUP)]
    gpu_threadgroup: usize,
}

impl CpuArgs {
    fn cpu_list(&self) -> Result<Option<Vec<usize>>> {
        Ok(match (&self.cpus, &self.cpu_affinity) {
            (Some(text), _) => Some(parse_cpu_list(text).context("invalid --cpus list")?),
            (None, Some(mask)) => Some(parse_mask(mask)?),
            (None, None) => None,
        })
    }

    fn plan(&self, topology: &Topology) -> Result<Vec<Placement>> {
        // `-t 0 --gpu`: the GPU worker alone. Without --gpu, `-t 0` stays an error.
        if self.threads == Some(0)
            && let Some(gpu) = self.gpu_placement()?
        {
            return Ok(vec![gpu]);
        }
        let list = self.cpu_list()?;
        let mut placements = engine::plan(topology, self.layout, self.threads, list.as_deref())?;
        match self.lanes {
            Some(Lanes::Fixed(lanes)) => placements.iter_mut().for_each(|p| p.lanes = lanes),
            None | Some(Lanes::Auto) => engine::auto_lanes(&mut placements, topology),
        }
        placements.extend(self.gpu_placement()?);
        Ok(placements)
    }

    /// What `--gpu` fixes and what this Mac's GPU allows.
    #[cfg(all(target_os = "macos", feature = "gpu"))]
    fn gpu_limits(&self) -> Result<Option<tune::GpuLimits>> {
        if !self.gpu {
            return Ok(None);
        }
        let info = tideminer::gpu::info().context("--gpu: no Metal GPU found")?;
        Ok(Some(tune::GpuLimits {
            hashes: self.gpu_hashes.map(|h| h as usize),
            threadgroup: self.gpu_threadgroup,
            cores: info.cores.unwrap_or(0),
            max_hashes: info.max_hashes,
        }))
    }

    #[cfg(not(all(target_os = "macos", feature = "gpu")))]
    fn gpu_limits(&self) -> Result<Option<tune::GpuLimits>> {
        Ok(None)
    }

    /// The GPU worker's placement when `--gpu` is given (untuned: the default load).
    fn gpu_placement(&self) -> Result<Option<Placement>> {
        Ok(self.gpu_limits()?.map(|gpu| {
            Placement::gpu(engine::GpuSpec {
                hashes: gpu.default_hashes(),
                threadgroup: gpu.threadgroup,
            })
        }))
    }

    /// What the user fixed, for the tuner to search within.
    fn limits(&self) -> Result<tune::Limits> {
        Ok(tune::Limits {
            layout: self.layout,
            threads: self.threads,
            cpus: self.cpu_list()?,
            lanes: match self.lanes {
                Some(Lanes::Fixed(lanes)) => Some(lanes),
                None | Some(Lanes::Auto) => None,
            },
            gpu: self.gpu_limits()?,
        })
    }

    fn nice(&self) -> Option<i32> {
        self.nice.or(match self.cpu_priority {
            Some(0) => Some(19),
            Some(1) => Some(10),
            _ => None,
        })
    }
}

fn parse_mask(mask: &str) -> Result<Vec<usize>> {
    let digits = mask.trim_start_matches("0x").trim_start_matches("0X");
    let bits = u128::from_str_radix(digits, 16).context("--cpu-affinity must be a hex mask")?;
    let cpus: Vec<usize> = (0..128).filter(|i| bits >> i & 1 == 1).collect();
    ensure!(!cpus.is_empty(), "--cpu-affinity selects no CPUs");
    Ok(cpus)
}

#[derive(Args, Clone)]
struct MineArgs {
    /// Pool URL: stratum+tcp://HOST:PORT, stratum+tls://HOST:PORT (also +ssl,
    /// +tcps) or HOST:PORT. Repeat for failover order.
    #[arg(short = 'o', long = "url", value_name = "URL")]
    urls: Vec<String>,
    /// Payout address and optional rig name: ADDRESS[.rig]
    #[arg(short = 'u', long = "user")]
    user: Option<String>,
    #[arg(
        short = 'p',
        long = "pass",
        env = "TIDEMINER_PASSWORD",
        default_value = "x",
        hide_env_values = true
    )]
    password: String,
    /// cpuminer-style USER:PASS.
    #[arg(short = 'O', long, value_name = "USER:PASS", conflicts_with = "user")]
    userpass: Option<String>,
    /// Connect to every -o pool over TLS (same as writing stratum+tls://).
    #[arg(long)]
    tls: bool,
    /// Also trust the CA certificate(s) in this PEM file (pools with a private CA).
    #[arg(long, value_name = "PEM")]
    cert: Vec<PathBuf>,
    /// Accepted for cpuminer compatibility: yespowertide (or yespower with
    /// -N 2048 -R 8). Tidecoin only; anything else is refused.
    #[arg(short = 'a', long = "algo")]
    algo: Option<String>,
    /// yespower N (must be 2048).
    #[arg(short = 'N', long = "param-n")]
    param_n: Option<u32>,
    /// yespower r (must be 8).
    #[arg(short = 'R', long = "param-r")]
    param_r: Option<u32>,
    /// yespower personalization (must be empty for Tidecoin).
    #[arg(short = 'K', long = "param-key")]
    param_key: Option<String>,
    #[command(flatten)]
    cpu: CpuArgs,
    /// Hide per-share lines; keep reports, blocks and errors.
    #[arg(short = 'q', long)]
    quiet: bool,
    /// Plain output (also when NO_COLOR is set or stdout is not a terminal).
    #[arg(long)]
    no_color: bool,
    /// Seconds between periodic reports.
    #[arg(long, visible_alias = "report-interval", default_value_t = 60)]
    stats_interval: u64,
    /// Extra diagnostics (every job).
    #[arg(short = 'D', long)]
    debug: bool,
    /// Print every Stratum message sent and received (password hidden).
    #[arg(short = 'P', long)]
    protocol_dump: bool,
    /// Give up after N consecutive failed connections (-1: never).
    #[arg(short = 'r', long, default_value_t = -1, allow_negative_numbers = true)]
    retries: i64,
    /// Longest pause between reconnect attempts, seconds.
    #[arg(long, default_value_t = 32)]
    retry_pause: u64,
    /// Total silent seconds before reconnecting; ping halfway through this window.
    #[arg(short = 'T', long, default_value_t = 90, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
    /// Reconnect if a share goes unanswered this many seconds, even if jobs arrive.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=3600))]
    submit_timeout: u64,
    /// Maximum unanswered mining.submit requests.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u64).range(1..=64))]
    max_inflight: u64,
    /// Stop after this many seconds.
    #[arg(long, value_name = "SECONDS")]
    time_limit: Option<u64>,
    /// Pause hashing when the CPU package reaches this temperature (°C) and
    /// resume 5 °C below it. Linux (coretemp/k10temp sensors).
    #[arg(long, value_name = "CELSIUS", value_parser = clap::value_parser!(u16).range(40..=110))]
    max_temp: Option<u16>,
    /// Run a quick comparison (~15-40 s) before connecting if no matching saved
    /// tuning exists. Saved `tideminer tune` results are used automatically,
    /// even without this flag, within the same CPU/GPU limits.
    #[arg(long)]
    autotune: bool,
    /// Hash offline for 30 s and report (cpuminer --benchmark); same as `bench`.
    #[arg(long)]
    benchmark: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Mine Tidecoin on a Stratum pool (same as running without a subcommand).
    Mine(Box<MineArgs>),
    /// Measure sustained hashrate on the mining engine (no network).
    Bench {
        #[command(flatten)]
        cpu: CpuArgs,
        /// Timed seconds (after warmup).
        #[arg(long, default_value_t = 30.0)]
        seconds: f64,
        /// Untimed warmup seconds.
        #[arg(long, default_value_t = 3.0)]
        warmup: f64,
        /// Reproducible corpus mode instead: hash exactly this many nonces
        /// (split evenly, unpinned) and report digest XORs.
        #[arg(long)]
        hashes: Option<u64>,
    },
    /// Measure worker configurations offline; save the fastest for normal mining startup.
    ///
    /// Searches within -t/--layout/--cpus/--lanes, prints a ranked table (with
    /// watts where readable) and the fastest and most efficient settings as flags.
    Tune {
        #[command(flatten)]
        cpu: CpuArgs,
        /// Time budget in minutes (warm-up, screening, then longer final rounds).
        #[arg(long, default_value_t = 5.0)]
        minutes: f64,
        /// Run only the quick comparison `--autotune` does; nothing is saved.
        #[arg(long)]
        quick: bool,
        /// List the configurations that would be measured, then exit.
        #[arg(long)]
        list: bool,
        /// Do not save the result.
        #[arg(long)]
        no_save: bool,
        /// Also print the full result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the detected CPU topology and the worker placement a layout would use.
    Topology {
        #[command(flatten)]
        cpu: CpuArgs,
    },
    /// Verify the known Tidecoin header/hash pair.
    SelfTest,
    /// Hash one serialized 80-byte header; print raw digest bytes as hex.
    Hash { header: String },
    /// Subscribe, authorize, and validate a job, then exit. Does not submit shares.
    Probe {
        #[arg(short = 'o', long)]
        url: String,
        #[arg(short = 'u', long)]
        user: String,
        #[arg(
            short = 'p',
            long,
            env = "TIDEMINER_PASSWORD",
            default_value = "x",
            hide_env_values = true
        )]
        password: String,
        #[arg(long)]
        tls: bool,
        #[arg(long, value_name = "PEM")]
        cert: Vec<PathBuf>,
        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=300))]
        timeout: u64,
    },
}

fn describe(placements: &[Placement]) -> String {
    let mut ids: Vec<usize> = placements.iter().filter_map(|p| p.cpu).collect();
    ids.sort_unstable();
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
    let mut kinds: Vec<(&str, usize)> = Vec::new();
    for p in placements.iter().filter(|p| p.gpu.is_none()) {
        match kinds.iter_mut().find(|(k, _)| *k == p.kind.label()) {
            Some((_, n)) => *n += 1,
            None => kinds.push((p.kind.label(), 1)),
        }
    }
    let mut kinds: Vec<String> = kinds.iter().map(|(k, n)| format!("{n} {k}")).collect();
    kinds.extend(
        placements
            .iter()
            .filter_map(|p| p.gpu)
            .map(|g| format!("GPU ({} hashes)", g.hashes)),
    );
    let paired = placements.iter().filter(|p| p.lanes > 1).count();
    let cpu_workers = placements.iter().filter(|p| p.gpu.is_none()).count();
    if cpu_workers == 0
        && let Some(gpu) = placements.iter().find_map(|p| p.gpu)
    {
        return format!("GPU only ({} hashes), no CPU workers", gpu.hashes);
    }
    let lanes = if paired == 0 {
        String::new()
    } else if paired == cpu_workers {
        ", 2 hashes each".to_string()
    } else {
        format!(", 2 hashes on {paired}")
    };
    if ranges.is_empty() {
        format!(
            "{} workers ({}{lanes}), OS-scheduled",
            placements.len(),
            kinds.join(" + ")
        )
    } else {
        format!(
            "{} workers ({}{lanes}) pinned to CPUs {}",
            placements.len(),
            kinds.join(" + "),
            ranges.join(",")
        )
    }
}

#[derive(Clone, Copy, Debug)]
enum Lanes {
    Auto,
    Fixed(usize),
}

fn parse_lanes(text: &str) -> Result<Lanes, String> {
    match text {
        "auto" => Ok(Lanes::Auto),
        "1" => Ok(Lanes::Fixed(1)),
        "2" => Ok(Lanes::Fixed(2)),
        _ => Err("expected auto, 1 or 2".into()),
    }
}

/// Refuse anything that is not Tidecoin's yespower, whatever cpuminer habit says.
fn check_algorithm(args: &MineArgs) -> Result<()> {
    const HINT: &str = "tideminer mines only Tidecoin: yespower 1.0, N=2048, r=8";
    if let Some(algo) = &args.algo {
        match algo.to_ascii_lowercase().as_str() {
            "yespowertide" | "yespower-tide" | "yespowertidecoin" | "yespower-tidecoin"
            | "tidecoin" | "tdc" => {}
            "yespower" => ensure!(
                args.param_n == Some(2048) && args.param_r == Some(8),
                "-a yespower needs -N 2048 -R 8 for Tidecoin (cpuminer's defaults differ); \
                 or use -a yespowertide"
            ),
            other => bail!("-a {other} is not supported: {HINT}"),
        }
    }
    if let Some(n) = args.param_n {
        ensure!(n == 2048, "-N {n}: {HINT}");
    }
    if let Some(r) = args.param_r {
        ensure!(r == 8, "-R {r}: {HINT}");
    }
    if let Some(key) = &args.param_key {
        ensure!(
            key.is_empty(),
            "-K: Tidecoin uses no yespower personalization"
        );
    }
    Ok(())
}

fn tls_settings(certs: &[PathBuf]) -> Result<TlsSettings> {
    let mut settings = TlsSettings::default();
    for path in certs {
        settings.add_pem_file(path)?;
    }
    Ok(settings)
}

fn mine(args: MineArgs) -> Result<()> {
    if args.benchmark {
        return bench(args.cpu, 30.0, 3.0);
    }
    check_algorithm(&args)?;
    let (user, password) = match &args.userpass {
        Some(pair) => {
            let (u, p) = pair.split_once(':').unwrap_or((pair, "x"));
            (u.to_owned(), p.to_owned())
        }
        None => (
            args.user
                .clone()
                .context("missing -u ADDRESS[.rig] (or -O USER:PASS)")?,
            args.password.clone(),
        ),
    };
    ensure!(!args.urls.is_empty(), "missing -o POOL_URL");
    ensure!(
        !user.is_empty() && user.len() <= 512 && password.len() <= 512,
        "invalid credential length"
    );
    let pools = args
        .urls
        .iter()
        .map(|u| {
            let mut endpoint = stratum::Endpoint::parse(u)?;
            if args.tls && !endpoint.tls {
                endpoint.set_tls(true);
            }
            Ok(endpoint)
        })
        .collect::<Result<Vec<_>>>()?;
    let out = Reporter::new(args.no_color, args.quiet).background()?;
    let topology = Topology::detect();
    out.raw(out.paint(
        Color::Bold,
        format!(
            "tideminer {}  Tidecoin yespower 1.0 (N=2048, r=8)",
            env!("CARGO_PKG_VERSION")
        ),
    ));
    let row = |k: &str, v: String| {
        out.raw(format!(
            "  {} {v}",
            out.paint(Color::Cyan, format!("{k:<8}"))
        ));
    };
    row(
        "kernel",
        format!("{} ({})", pow::KERNEL, topology.features.join(" ")),
    );
    row("cpu", topology.summary());
    pow::self_test()?;
    let placements = if args.autotune {
        tune::autotune(&topology, &args.cpu.limits()?, args.cpu.nice(), &out)?
    } else if let Some(placements) = tune::saved_placements(&topology, &args.cpu.limits()?, &out) {
        placements
    } else {
        args.cpu.plan(&topology)?
    };
    row("workers", describe(&placements));
    for (i, pool) in pools.iter().enumerate() {
        row(
            if i == 0 { "pool" } else { "failover" },
            format!("{}  user {user}", pool.url),
        );
    }
    let mut config = miner::Config::new(pools, user, password, placements);
    config.nice = args.cpu.nice();
    config.tls = tls_settings(&args.cert)?;
    config.reporter = out.clone();
    config.stats_interval = Duration::from_secs(args.stats_interval.max(1));
    config.max_inflight = args.max_inflight as usize;
    config.retries = u32::try_from(args.retries).ok();
    config.max_backoff = Duration::from_secs(args.retry_pause.max(1));
    config.idle_ping = Duration::from_secs(args.timeout) / 2;
    config.ping_timeout = Duration::from_secs(args.timeout) - config.idle_ping;
    config.submit_timeout = Duration::from_secs(args.submit_timeout);
    config.time_limit = args.time_limit.map(Duration::from_secs);
    config.protocol_dump = args.protocol_dump;
    config.debug = args.debug;
    config.max_temp = args.max_temp.map(f64::from);
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        let out = out.clone();
        ctrlc::set_handler(move || {
            if stop.swap(true, Ordering::SeqCst) {
                std::process::exit(130);
            }
            out.raw("stopping (Ctrl-C again to force)...");
        })?;
    }
    let result = miner::run(&config, stop);
    if let Ok(summary) = &result {
        miner::print_summary(&out, summary);
    }
    out.flush(Duration::from_millis(200));
    result?;
    Ok(())
}

fn bench(cpu: CpuArgs, seconds: f64, warmup: f64) -> Result<()> {
    let topology = Topology::detect();
    let placements = cpu.plan(&topology)?;
    eprintln!("cpu: {}", topology.summary());
    eprintln!("workers: {}", describe(&placements));
    eprintln!("measuring {seconds:.0} s after {warmup:.0} s warmup...");
    let report = benchmark::sustained(placements, seconds, warmup, cpu.nice())?;
    eprintln!(
        "sustained: {}",
        tideminer::report::format_rate(report.hashes_per_second)
    );
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn tune_command(
    cpu: CpuArgs,
    minutes: f64,
    quick: bool,
    list: bool,
    no_save: bool,
    json: bool,
) -> Result<()> {
    ensure!(
        (1.0..=240.0).contains(&minutes),
        "--minutes must be between 1 and 240"
    );
    let out = Reporter::new(false, false);
    let topology = Topology::detect();
    let limits = cpu.limits()?;
    out.raw(format!("cpu: {}", topology.summary()));
    let candidates = if quick {
        tune::quick_candidates(&topology, &limits)?
    } else {
        tune::offline_candidates(&topology, &limits)?
    };
    if list {
        for (i, c) in candidates.iter().enumerate() {
            out.raw(format!(
                "{:>3}  {:<40} {}",
                i + 1,
                tune::label(&c.placements),
                tune::equivalent_flags(&topology, &c.placements).unwrap_or_default()
            ));
        }
        return Ok(());
    }
    pow::self_test()?;
    let outcome = if quick {
        out.raw(format!(
            "quick comparison of {} configurations, ~{}",
            candidates.len(),
            tideminer::report::format_duration(Duration::from_secs_f64(tune::quick_seconds(
                candidates.len()
            )))
        ));
        match tune::quick(&topology, &limits, cpu.nice())? {
            Some(outcome) => outcome,
            None => {
                out.raw("only one configuration fits these limits; nothing to compare");
                return Ok(());
            }
        }
    } else {
        tune::offline(&topology, &limits, cpu.nice(), minutes, &out)?
    };
    out.raw("");
    tune::print_outcome(&out, &outcome, "");
    let chosen = &outcome.rows[outcome.chosen];
    let flags = |recipe| -> Result<String> {
        let placements = tune::build(&topology, &limits, recipe)?;
        Ok(tune::equivalent_flags(&topology, &placements)
            .unwrap_or_else(|| "(not expressible as flags; use --autotune)".into()))
    };
    out.raw(format!(
        "  chosen as flags:         {}",
        flags(chosen.recipe)?
    ));
    if outcome.chosen != 0 {
        out.raw(format!(
            "  measured fastest flags:  {}",
            flags(outcome.rows[0].recipe)?
        ));
    }
    if let Some(e) = outcome.efficient {
        out.raw(format!(
            "  most efficient as flags: {}",
            flags(outcome.rows[e].recipe)?
        ));
    }
    if !quick && !no_save && !outcome.trustworthy() {
        out.raw("not saved: comparison inconclusive; existing saved tuning left unchanged");
    } else if !quick && !no_save {
        let path = tune::save(tune::Saved::new(&topology, &limits, &outcome))?;
        out.raw(format!(
            "saved to {}: mining with the same -t/--layout/--cpus/--lanes/--gpu uses it automatically",
            path.display()
        ));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Hashing commands keep a Mac or Windows PC out of idle sleep (the display may
    // still turn off).
    #[cfg(any(target_os = "macos", windows))]
    let _awake = matches!(
        cli.command,
        None | Some(Command::Mine(_) | Command::Bench { .. } | Command::Tune { .. })
    )
    .then(|| {
        let awake = tideminer::os::keep_awake("tideminer is hashing");
        if awake.is_none() {
            eprintln!(
                "warning: could not prevent idle sleep; the computer may sleep and stop hashing"
            );
        }
        awake
    });
    match cli.command {
        None => mine(cli.mine)?,
        Some(Command::Mine(args)) => mine(*args)?,
        Some(Command::Bench {
            cpu,
            seconds,
            warmup,
            hashes,
        }) => {
            if let Some(hashes) = hashes {
                let topology = Topology::detect();
                let threads = cpu.threads.unwrap_or(topology.cpus.len());
                println!(
                    "{}",
                    serde_json::to_string_pretty(&benchmark::run(threads, hashes, 8)?)?
                );
            } else {
                bench(cpu, seconds, warmup)?;
            }
        }
        Some(Command::Tune {
            cpu,
            minutes,
            quick,
            list,
            no_save,
            json,
        }) => tune_command(cpu, minutes, quick, list, no_save, json)?,
        Some(Command::Topology { cpu }) => {
            let topology = Topology::detect();
            let placements = cpu.plan(&topology)?;
            println!("{}", topology.summary());
            println!("{}", describe(&placements));
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "topology": topology,
                    "placements": placements,
                }))?
            );
        }
        Some(Command::SelfTest) => {
            pow::self_test()?;
            println!(
                "Tidecoin yespower 1.0 N=2048 r=8: known-answer test passed ({})",
                pow::KERNEL
            );
        }
        Some(Command::Hash { header }) => {
            println!(
                "{}",
                hex::encode(pow::hash_once(&pow::Header::from_hex(&header)?)?)
            )
        }
        Some(Command::Probe {
            url,
            user,
            password,
            tls,
            cert,
            timeout,
        }) => {
            let mut endpoint = stratum::Endpoint::parse(&url)?;
            if tls {
                endpoint.set_tls(true);
            }
            let report = stratum::probe(
                &endpoint,
                &user,
                &password,
                Duration::from_secs(timeout),
                &tls_settings(&cert)?,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn pool_timeout_options() {
        let cli = Cli::try_parse_from(["tideminer", "-o", "h:1", "-u", "a"]).unwrap();
        assert_eq!(cli.mine.timeout, 90);
        assert_eq!(cli.mine.submit_timeout, 5);
        let cli = Cli::try_parse_from([
            "tideminer",
            "-o",
            "h:1",
            "-u",
            "a",
            "-T",
            "1",
            "--submit-timeout",
            "2",
        ])
        .unwrap();
        assert_eq!(cli.mine.timeout, 1);
        assert_eq!(cli.mine.submit_timeout, 2);
        for option in ["--timeout", "--submit-timeout"] {
            for invalid in ["0", "3601"] {
                assert!(
                    Cli::try_parse_from(["tideminer", "-o", "h:1", "-u", "a", option, invalid,])
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn cpuminer_style_command_lines_parse() {
        let cli = Cli::try_parse_from([
            "tideminer",
            "-a",
            "yespowertide",
            "-o",
            "na.rplant.xyz:17059",
            "--tls",
            "-u",
            "TBa6n1DBKCubGT9hacEZ5967kPC45nPwpc",
            "-t",
            "8",
            "-q",
            "--max-temp",
            "90",
        ])
        .unwrap();
        assert!(cli.command.is_none());
        assert!(cli.mine.tls && cli.mine.quiet);
        assert_eq!(cli.mine.max_temp, Some(90));
        assert!(
            Cli::try_parse_from(["tideminer", "-o", "h:1", "-u", "a", "--max-temp", "200"])
                .is_err()
        );
        check_algorithm(&cli.mine).unwrap();
        let cli = Cli::try_parse_from([
            "tideminer",
            "-a",
            "yespower",
            "-N",
            "2048",
            "-R",
            "8",
            "-o",
            "h:1",
            "-u",
            "a",
        ])
        .unwrap();
        check_algorithm(&cli.mine).unwrap();
        for bad in [
            vec!["tideminer", "-a", "yespower", "-o", "h:1", "-u", "a"],
            vec!["tideminer", "-a", "yespowerr16", "-o", "h:1", "-u", "a"],
            vec![
                "tideminer",
                "-a",
                "yespowertide",
                "-R",
                "32",
                "-o",
                "h:1",
                "-u",
                "a",
            ],
        ] {
            let cli = Cli::try_parse_from(bad.clone()).unwrap();
            assert!(check_algorithm(&cli.mine).is_err(), "{bad:?}");
        }
        assert!(Cli::try_parse_from(["tideminer", "mine", "-o", "h:1", "-u", "a"]).is_ok());
        assert!(Cli::try_parse_from(["tideminer", "-o", "h:1", "self-test"]).is_err());
    }

    #[test]
    fn cpu_masks() {
        assert_eq!(parse_mask("0xF").unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(parse_mask("10000").unwrap(), vec![16]);
        assert!(parse_mask("0").is_err());
        assert!(parse_mask("zz").is_err());
    }
}
