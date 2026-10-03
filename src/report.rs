//! Human-facing output: colored event lines, periodic reports, number formatting
//! and a few unprivileged system sensors (Linux: CPU temperature and clocks).
use crate::topology::CoreKind;
use std::collections::VecDeque;
use std::fmt::Display;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy)]
pub enum Color {
    Green,
    Red,
    Yellow,
    Cyan,
    Magenta,
    Blue,
    Dim,
    Bold,
}

impl Color {
    fn code(self) -> &'static str {
        match self {
            Self::Green => "32",
            Self::Red => "31",
            Self::Yellow => "33",
            Self::Cyan => "36",
            Self::Magenta => "35",
            Self::Blue => "34",
            Self::Dim => "2",
            Self::Bold => "1",
        }
    }
}

/// Prints to stdout. Colors only on a terminal, never with `NO_COLOR` set or
/// `--no-color`.
#[derive(Clone)]
pub struct Reporter {
    color: bool,
    /// Suppress per-share lines (cpuminer `-q`); reports and warnings remain.
    pub quiet: bool,
    pub silent: bool,
    output: Option<Arc<AsyncOutput>>,
}

const LOG_CAPACITY: usize = 1024;
const MAX_LOG_BYTES: usize = 8192;

enum LogEntry {
    Line(String),
    Flush(mpsc::Sender<()>),
}

struct AsyncOutput {
    tx: mpsc::SyncSender<LogEntry>,
    dropped: Arc<AtomicU64>,
}

impl AsyncOutput {
    fn start(mut writer: impl std::io::Write + Send + 'static) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(LOG_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let lost = dropped.clone();
        std::thread::Builder::new()
            .name("tm-log".into())
            .spawn(move || {
                while let Ok(entry) = rx.recv() {
                    match entry {
                        LogEntry::Line(line) => {
                            let count = lost.swap(0, Relaxed);
                            if count > 0
                                && writeln!(
                                    writer,
                                    "[logging] dropped {count} lines while output was stalled"
                                )
                                .is_err()
                            {
                                break;
                            }
                            if writeln!(writer, "{line}").is_err() {
                                break;
                            }
                        }
                        LogEntry::Flush(done) => {
                            let _ = writer.flush();
                            let _ = done.send(());
                        }
                    }
                }
            })?;
        Ok(Self { tx, dropped })
    }

    fn line(&self, mut line: String) {
        if line.len() > MAX_LOG_BYTES {
            let mut end = MAX_LOG_BYTES - " [truncated]".len();
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
            line.push_str(" [truncated]");
            line.shrink_to_fit();
        }
        if self.tx.try_send(LogEntry::Line(line)).is_err() {
            self.dropped.fetch_add(1, Relaxed);
        }
    }

    fn flush(&self, timeout: Duration) {
        let (tx, rx) = mpsc::channel();
        if self.tx.try_send(LogEntry::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(timeout);
        }
    }
}

impl Reporter {
    pub fn new(no_color: bool, quiet: bool) -> Self {
        let color =
            !no_color && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
        #[cfg(windows)]
        let color = color && crate::os::enable_ansi_colors();
        Self {
            color,
            quiet,
            silent: false,
            output: None,
        }
    }

    /// No output at all (tests).
    pub fn silent() -> Self {
        Self {
            color: false,
            quiet: true,
            silent: true,
            output: None,
        }
    }

    /// Mining must never wait for a terminal, pipe, or log file. Use one bounded
    /// process-wide writer; a stalled sink cannot create more blocked threads.
    pub fn background(mut self) -> anyhow::Result<Self> {
        if !self.silent && self.output.is_none() {
            static OUTPUT: OnceLock<Result<Arc<AsyncOutput>, String>> = OnceLock::new();
            self.output = Some(
                OUTPUT
                    .get_or_init(|| {
                        AsyncOutput::start(std::io::stdout())
                            .map(Arc::new)
                            .map_err(|e| e.to_string())
                    })
                    .as_ref()
                    .map_err(|e| anyhow::anyhow!("start log writer: {e}"))?
                    .clone(),
            );
        }
        Ok(self)
    }

    /// Best effort at exit. A blocked sink must not prevent shutdown.
    pub fn flush(&self, timeout: Duration) {
        if let Some(output) = &self.output {
            output.flush(timeout);
        }
    }

    fn emit(&self, text: String) {
        if let Some(output) = &self.output {
            output.line(text);
        } else {
            println!("{text}");
        }
    }

    pub fn paint(&self, color: Color, text: impl Display) -> String {
        if self.color {
            format!("\x1b[{}m{text}\x1b[0m", color.code())
        } else {
            text.to_string()
        }
    }

    /// `[time] LABEL     message`, label padded and colored.
    pub fn line(&self, label: &str, color: Color, message: impl Display) {
        if self.silent {
            return;
        }
        self.emit(format!(
            "{} {} {message}",
            self.paint(Color::Dim, format!("[{}]", timestamp())),
            self.paint(color, format!("{label:<10}"))
        ));
    }

    /// A share-level line, hidden with `-q`.
    pub fn share_line(&self, label: &str, color: Color, message: impl Display) {
        if !self.quiet {
            self.line(label, color, message);
        }
    }

    /// Continuation line aligned under a report header.
    pub fn detail(&self, key: &str, value: impl Display) {
        if self.silent {
            return;
        }
        self.emit(format!(
            "{:22}{} {value}",
            "",
            self.paint(Color::Cyan, format!("{key:<10}"))
        ));
    }

    pub fn raw(&self, text: impl Display) {
        if !self.silent {
            self.emit(text.to_string());
        }
    }
}

/// Local wall-clock time, `YYYY-MM-DD HH:MM:SS`.
pub fn timestamp() -> String {
    let utc = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let secs = utc + crate::os::local_utc_offset(utc);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

pub fn format_rate(rate: f64) -> String {
    if rate >= 1e6 {
        format!("{:.2} MH/s", rate / 1e6)
    } else if rate >= 1e3 {
        format!("{:.2} kH/s", rate / 1e3)
    } else {
        format!("{rate:.0} H/s")
    }
}

/// Compact difficulty: 0.7000, 3.21, 842.1, 8.23k, 1.20M.
pub fn format_diff(d: f64) -> String {
    if !d.is_finite() {
        "∞".into()
    } else if d >= 1e6 {
        format!("{:.2}M", d / 1e6)
    } else if d >= 1e4 {
        format!("{:.2}k", d / 1e3)
    } else if d >= 100.0 {
        format!("{d:.1}")
    } else if d >= 1.0 {
        format!("{d:.3}")
    } else {
        format!("{d:.4}")
    }
}

/// `2d03h`, `1h02m`, `3m05s`, `12s`.
pub fn format_duration(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        return "<1s".into();
    }
    let s = d.as_secs();
    if s >= 86_400 {
        format!("{}d{:02}h", s / 86_400, s % 86_400 / 3600)
    } else if s >= 3600 {
        format!("{}h{:02}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// `2746422` -> `2,746,422`.
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Pool-scale difficulty of a digest (`2^240 / value`, digest read little-endian):
/// the highest share difficulty this hash would satisfy.
pub fn digest_difficulty(digest: &[u8; 32]) -> f64 {
    let value = digest
        .iter()
        .rev()
        .fold(0.0f64, |sum, &b| sum * 256.0 + f64::from(b));
    if value == 0.0 {
        f64::INFINITY
    } else {
        2f64.powi(240) / value
    }
}

/// BIP34 block height from coinbase part 1 (version, input count, prevout,
/// scriptSig length, then a push of the height).
pub fn coinbase_height(coinbase1: &[u8]) -> Option<u64> {
    // 4 version + 1 input count + 36 prevout + 1 script length (< 0xfd).
    let script = coinbase1.get(42..)?;
    let push = usize::from(*script.first()?);
    if !(1..=8).contains(&push) {
        return None;
    }
    let bytes = script.get(1..1 + push)?;
    Some(
        bytes
            .iter()
            .rev()
            .fold(0u64, |h, &b| (h << 8) | u64::from(b)),
    )
}

/// Hash rate over a sliding window from cumulative hash counts.
pub struct RateMeter {
    samples: VecDeque<(Instant, u64)>,
    window: Duration,
}

impl RateMeter {
    pub fn new(window: Duration) -> Self {
        Self {
            samples: VecDeque::new(),
            window,
        }
    }

    pub fn needs_sample(&self, now: Instant) -> bool {
        self.samples
            .back()
            .is_none_or(|(t, _)| now - *t >= Duration::from_millis(500))
    }

    pub fn push(&mut self, now: Instant, total: u64) {
        if !self.needs_sample(now) {
            return;
        }
        self.samples.push_back((now, total));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now - *t > self.window)
            && self.samples.len() > 2
        {
            self.samples.pop_front();
        }
    }

    /// True once two samples span some time.
    pub fn ready(&self) -> bool {
        self.samples.len() >= 2
    }

    pub fn rate(&self) -> f64 {
        match (self.samples.front(), self.samples.back()) {
            (Some(&(t0, h0)), Some(&(t1, h1))) if t1 > t0 => {
                (h1 - h0) as f64 / (t1 - t0).as_secs_f64()
            }
            _ => 0.0,
        }
    }
}

/// Unprivileged readings; each is `None` where unavailable.
pub struct Sensors {
    temperature: Option<PathBuf>,
    cpus: Vec<(usize, CoreKind)>,
}

impl Sensors {
    pub fn new(cpus: Vec<(usize, CoreKind)>) -> Self {
        Self {
            temperature: find_package_temperature(),
            cpus,
        }
    }

    /// CPU package (Intel coretemp) or Tctl/Tdie (AMD k10temp) in °C.
    pub fn temperature(&self) -> Option<f64> {
        let text = std::fs::read_to_string(self.temperature.as_ref()?).ok()?;
        text.trim().parse::<f64>().ok().map(|m| m / 1000.0)
    }

    /// Average current clock of the mining CPUs of one kind, MHz.
    pub fn average_mhz(&self, kind: CoreKind) -> Option<f64> {
        let readings: Vec<f64> = self
            .cpus
            .iter()
            .filter(|(_, k)| *k == kind)
            .filter_map(|(cpu, _)| {
                std::fs::read_to_string(format!(
                    "/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_cur_freq"
                ))
                .ok()?
                .trim()
                .parse::<f64>()
                .ok()
            })
            .collect();
        (!readings.is_empty())
            .then(|| readings.iter().sum::<f64>() / readings.len() as f64 / 1000.0)
    }
}

fn find_package_temperature() -> Option<PathBuf> {
    let entries = std::fs::read_dir("/sys/class/hwmon").ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        let name = std::fs::read_to_string(dir.join("name")).unwrap_or_default();
        if matches!(name.trim(), "coretemp" | "k10temp" | "zenpower") {
            let input = dir.join("temp1_input");
            if input.exists() {
                return Some(input);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_log_sink_cannot_block_producers_or_shutdown() {
        struct Stalled {
            entered: Option<mpsc::Sender<()>>,
            release: mpsc::Receiver<()>,
        }
        impl std::io::Write for Stalled {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if let Some(entered) = self.entered.take() {
                    let _ = entered.send(());
                    let _ = self.release.recv();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (entered, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let output = AsyncOutput::start(Stalled {
            entered: Some(entered),
            release: wait,
        })
        .unwrap();
        output.line("first".into());
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        // Flush itself must have a deadline even before the queue fills.
        output.flush(Duration::from_millis(20));
        for _ in 0..LOG_CAPACITY * 2 {
            output.line("more".into());
        }
        output.flush(Duration::from_millis(20));
        let elapsed = started.elapsed();
        let dropped = output.dropped.load(Relaxed);
        release.send(()).unwrap();
        assert!(elapsed < Duration::from_secs(1));
        assert!(dropped >= LOG_CAPACITY as u64);
    }

    #[test]
    fn formatting() {
        assert_eq!(format_rate(950.0), "950 H/s");
        assert_eq!(format_rate(14_523.0), "14.52 kH/s");
        assert_eq!(format_count(2_746_422), "2,746,422");
        assert_eq!(format_count(12), "12");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h02m");
        assert_eq!(format_duration(Duration::from_secs(185)), "3m05s");
        assert_eq!(format_duration(Duration::from_millis(300)), "<1s");
        assert_eq!(format_diff(0.7), "0.7000");
        assert_eq!(format_diff(8226.0), "8226.0");
        assert_eq!(format_diff(82_260.0), "82.26k");
        assert_eq!(timestamp().len(), 19);
    }

    #[test]
    fn height_from_rplant_coinbase() {
        let cb1 = hex::decode("01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff4e0336e82904").unwrap();
        assert_eq!(coinbase_height(&cb1), Some(2_746_422));
        assert_eq!(coinbase_height(&[0u8; 10]), None);
    }

    #[test]
    fn digest_difficulty_matches_target_scale() {
        // A digest exactly at the difficulty-1 target has difficulty ~1.
        let mut digest = crate::target::Target::from_difficulty(1.0)
            .unwrap()
            .to_be_bytes();
        digest.reverse();
        assert!((digest_difficulty(&digest) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn rate_meter_windows() {
        let t = Instant::now();
        let mut m = RateMeter::new(Duration::from_secs(10));
        m.push(t, 0);
        m.push(t + Duration::from_secs(1), 1000);
        m.push(t + Duration::from_secs(2), 2000);
        assert!((m.rate() - 1000.0).abs() < 1e-6);
    }
}
