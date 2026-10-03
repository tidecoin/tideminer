//! Pool mining coordinator for Stratum V1 pools (including
//! yiimp/rplant-style pools), over plain TCP or TLS.
//!
//! One thread owns the session state: it handles pool messages (fed by a per-
//! connection I/O thread), publishes work to the engine, and submits shares.
//! Session rules:
//! - every `mining.notify` is an immutable assignment with the difficulty from the
//!   `set_difficulty` sent just before it; always switch to the newest notify and
//!   keep older same-parent jobs submittable;
//! - `clean_jobs=true`, reconnects and extranonce changes invalidate older work;
//! - by default at most 4 submits in flight; "submit queue full" (20) and "Backend
//!   unavailable" (26) are retried with the identical share, to preserve its
//!   identity; an unanswered submit times out the connection, even if jobs flow;
//! - the pool may never ping, so we ping it when the line goes quiet.
use crate::engine::{Engine, Found, Placement, Work, WorkerEvent};
use crate::report::{
    Color, RateMeter, Reporter, Sensors, coinbase_height, digest_difficulty, format_count,
    format_diff, format_duration, format_rate,
};
use crate::stratum::{AGENT, Endpoint};
use crate::target::digest_display;
use crate::topology::CoreKind;
use crate::transport::{self, Stream, TlsSettings};
use crate::work::{Job, extranonce2_bytes, extranonce2_space};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_LINE: usize = 64 * 1024;
const MAX_FOUND_BACKLOG: usize = 4096;
const MAX_QUEUED_SHARES: usize = 1024;
const MAX_TEMPLATES: usize = 64;
const MAX_SUBMIT_ATTEMPTS: u32 = 8;
const MAX_OUTBOUND: usize = 128;
const MAX_POOL_BACKLOG: usize = 256;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Pool difficulty 1 corresponds to 2^16 hashes per share (yespower target scale).
const HASHES_PER_DIFF: f64 = 65536.0;

#[derive(Clone)]
pub struct Config {
    pub pools: Vec<Endpoint>,
    pub user: String,
    pub password: String,
    pub placements: Vec<Placement>,
    pub nice: Option<i32>,
    pub tls: TlsSettings,
    pub reporter: Reporter,
    /// Periodic report interval.
    pub stats_interval: Duration,
    pub max_inflight: usize,
    /// Maximum time to wait for any individual share response before reconnecting.
    pub submit_timeout: Duration,
    pub idle_ping: Duration,
    pub ping_timeout: Duration,
    pub handshake_timeout: Duration,
    pub connect_timeout: Duration,
    /// Give up after this many consecutive failed connections (None: never).
    pub retries: Option<u32>,
    /// Upper bound of the reconnect backoff.
    pub max_backoff: Duration,
    /// Stop mining after this long.
    pub time_limit: Option<Duration>,
    /// Print every Stratum line sent and received (password redacted).
    pub protocol_dump: bool,
    /// Extra diagnostics: every job, extranonce details.
    pub debug: bool,
    /// Pause hashing at this CPU package temperature (°C); resume 5 °C lower.
    pub max_temp: Option<f64>,
}

impl Config {
    pub fn new(
        pools: Vec<Endpoint>,
        user: String,
        password: String,
        placements: Vec<Placement>,
    ) -> Self {
        Self {
            pools,
            user,
            password,
            placements,
            nice: None,
            tls: TlsSettings::default(),
            reporter: Reporter::new(false, false),
            stats_interval: Duration::from_secs(60),
            max_inflight: 4,
            submit_timeout: Duration::from_secs(5),
            idle_ping: Duration::from_secs(45),
            ping_timeout: Duration::from_secs(45),
            handshake_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(10),
            retries: None,
            max_backoff: Duration::from_secs(32),
            time_limit: None,
            protocol_dump: false,
            debug: false,
            max_temp: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Summary {
    pub hashes: u64,
    pub seconds: f64,
    pub accepted: u64,
    pub rejected: u64,
    pub stale: u64,
    pub duplicate: u64,
    pub retried: u64,
    pub blocks_submitted: u64,
    /// Sum of difficulties of accepted shares (pool-credited work).
    pub accepted_difficulty: f64,
    /// Shares found locally but never submitted (job/session ended first).
    pub discarded: u64,
    pub connections: u64,
    pub best_share_difficulty: f64,
    /// Share of worker time spent hashing under --max-temp (None without it).
    pub thermal_active_fraction: Option<f64>,
    pub last_reject: Option<String>,
}

enum Event {
    Line {
        session: u64,
        value: Value,
        _permit: PoolPermit,
    },
    Closed {
        session: u64,
        error: String,
    },
    Worker(WorkerEvent),
}

/// Bound queued pool messages across current and already retired sessions.
struct PoolPermit(Arc<AtomicUsize>);

impl PoolPermit {
    fn acquire(count: &Arc<AtomicUsize>) -> Option<Self> {
        if count.fetch_add(1, Relaxed) >= MAX_POOL_BACKLOG {
            count.fetch_sub(1, Relaxed);
            None
        } else {
            Some(Self(count.clone()))
        }
    }
}

impl Drop for PoolPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
    }
}

struct IoProgress {
    epoch: Instant,
    writing_since: AtomicU64,
    stopped: AtomicBool,
}

impl IoProgress {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
            writing_since: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
        }
    }

    fn write_expired(&self) -> bool {
        let since = self.writing_since.load(Relaxed);
        since != 0
            && self.epoch.elapsed().as_millis() as u64 + 1 - since
                >= WRITE_TIMEOUT.as_millis() as u64
    }
}

/// Dropping a session cancels queued sends and interrupts socket I/O immediately,
/// even when the I/O thread cannot get back to checking its outbound receiver.
struct ConnectionControl {
    socket: TcpStream,
    progress: Arc<IoProgress>,
}

impl Drop for ConnectionControl {
    fn drop(&mut self) {
        self.progress.stopped.store(true, Relaxed);
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

#[derive(Clone, Debug)]
struct Share {
    session: u64,
    clean_generation: u64,
    job_id: String,
    extranonce2: String,
    ntime: String,
    nonce: String,
    difficulty: f64,
    share_difficulty: f64,
    block: bool,
    digest: [u8; 32],
    attempts: u32,
    not_before: Instant,
}

enum Request {
    Subscribe,
    Authorize,
    Ping,
    Submit { share: Share, sent: Instant },
}

struct Session {
    id: u64,
    endpoint: Endpoint,
    outbound: mpsc::SyncSender<Vec<u8>>,
    control: ConnectionControl,
    dump: Option<Reporter>,
    next_id: u64,
    extranonce: Option<(Arc<[u8]>, usize)>,
    authorized: bool,
    next_difficulty: f64,
    latest_job: Option<Arc<Job>>,
    requests: HashMap<u64, Request>,
    inflight: usize,
    started: Instant,
    last_rx: Instant,
    ping: Option<u64>,
    ready_at: Option<Instant>,
}

impl Session {
    fn send(&mut self, method: &str, params: Value, request: Request) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({"id": id, "method": method, "params": params}))?;
        self.requests.insert(id, request);
        Ok(id)
    }

    fn write(&mut self, message: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&message)?;
        if let Some(reporter) = &self.dump {
            reporter.line("send", Color::Dim, redacted_message(message).to_string());
        }
        bytes.push(b'\n');
        anyhow::ensure!(bytes.len() <= MAX_LINE, "outbound Stratum frame too large");
        self.outbound.try_send(bytes).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => anyhow::anyhow!("pool outbound queue full"),
            mpsc::TrySendError::Disconnected(_) => anyhow::anyhow!("pool connection closed"),
        })
    }

    fn ready(&self) -> bool {
        self.extranonce.is_some() && self.authorized
    }
}

/// Owns one connection: writes queued lines, reads and parses incoming lines.
/// Ends (closing the socket) when the session drops its sender or on I/O error.
fn io_loop(
    session: u64,
    mut stream: Stream,
    outbound: mpsc::Receiver<Vec<u8>>,
    events: mpsc::Sender<Event>,
    backlog: Arc<AtomicUsize>,
    progress: Arc<IoProgress>,
) {
    // Timeouts are configured and checked before this thread is started.
    let mut buffer = vec![0u8; 16 * 1024];
    let mut pending: Vec<u8> = Vec::new();
    let error = 'io: loop {
        // Bound each batch so peer requests cannot starve reads indefinitely.
        for _ in 0..16 {
            if progress.stopped.load(Relaxed) {
                stream.shutdown();
                return;
            }
            match outbound.try_recv() {
                Ok(bytes) => {
                    progress
                        .writing_since
                        .store(progress.epoch.elapsed().as_millis() as u64 + 1, Relaxed);
                    if let Err(e) = stream.write_all(&bytes).and_then(|()| stream.flush()) {
                        break 'io format!("write to pool: {e}");
                    }
                    progress.writing_since.store(0, Relaxed);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    stream.shutdown();
                    return;
                }
            }
        }
        match stream.read(&mut buffer) {
            Ok(0) => break "pool closed the connection".to_owned(),
            Ok(n) => {
                pending.extend_from_slice(&buffer[..n]);
                while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                    if end + 1 > MAX_LINE {
                        break 'io "pool sent an oversized line".to_owned();
                    }
                    let line: Vec<u8> = pending.drain(..=end).collect();
                    if line.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    match serde_json::from_slice::<Value>(&line) {
                        Ok(value) if value.is_object() => {
                            let Some(permit) = PoolPermit::acquire(&backlog) else {
                                break 'io "pool message backlog exceeded".to_owned();
                            };
                            if events
                                .send(Event::Line {
                                    session,
                                    value,
                                    _permit: permit,
                                })
                                .is_err()
                            {
                                stream.shutdown();
                                return;
                            }
                        }
                        _ => break 'io "pool sent invalid JSON".to_owned(),
                    }
                }
                if pending.len() > MAX_LINE {
                    break "pool sent an oversized line".to_owned();
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => break format!("read from pool: {e}"),
        }
    };
    stream.shutdown();
    let _ = events.send(Event::Closed { session, error });
}

/// Pool error `[code, "message", ...]` as (code, bounded printable message).
fn pool_error(error: &Value) -> (i64, String) {
    let code = error.get(0).and_then(Value::as_i64).unwrap_or(-1);
    let text = error
        .get(1)
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("unspecified");
    let text: String = text.chars().filter(|c| !c.is_control()).take(160).collect();
    (code, text)
}

/// Shorten a long payout address for display: `TBa6n1DB…C45nPwpc`.
fn short_user(user: &str) -> String {
    let (address, rig) = user.split_once('.').unwrap_or((user, ""));
    let address = if address.chars().count() > 20 {
        let head: String = address.chars().take(8).collect();
        let tail: String = address
            .chars()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("{head}…{tail}")
    } else {
        address.to_owned()
    };
    if rig.is_empty() {
        address
    } else {
        format!("{address}.{rig}")
    }
}

/// `--max-temp` control: on this class of hardware the CPU package goes from the
/// limit to 15 °C below it within a second of stopping, so cpuminer-style
/// all-or-nothing pausing just flaps. Instead the number of active workers is
/// adjusted (lowest-priority workers park first) until the temperature settles.
struct ThermalController {
    max: f64,
    workers: usize,
    active: usize,
    smoothed: Option<f64>,
    last_step: Option<Instant>,
    last_change: Instant,
    last_account: Instant,
    parked_worker_seconds: f64,
    started: Instant,
}

/// Seconds between control steps.
const THERMAL_STEP: Duration = Duration::from_secs(2);
/// Add a worker back only after this long below `max - THERMAL_MARGIN`.
const THERMAL_RAISE_AFTER: Duration = Duration::from_secs(6);
const THERMAL_MARGIN: f64 = 3.0;

impl ThermalController {
    fn new(max: f64, workers: usize, now: Instant) -> Self {
        Self {
            max,
            workers,
            active: workers,
            smoothed: None,
            last_step: None,
            last_change: now,
            last_account: now,
            parked_worker_seconds: 0.0,
            started: now,
        }
    }

    fn account(&mut self, now: Instant) {
        let dt = (now - self.last_account).as_secs_f64();
        self.parked_worker_seconds += (self.workers - self.active) as f64 * dt;
        self.last_account = now;
    }

    /// Share of worker time spent hashing since start (1.0 = never throttled).
    fn active_fraction(&mut self, now: Instant) -> f64 {
        self.account(now);
        let total = self.workers as f64 * (now - self.started).as_secs_f64();
        if total <= 0.0 {
            1.0
        } else {
            1.0 - self.parked_worker_seconds / total
        }
    }

    /// Returns (old, new, smoothed temperature) when the active count changes.
    /// A missing reading changes nothing.
    fn update(&mut self, now: Instant, reading: Option<f64>) -> Option<(usize, usize, f64)> {
        if self.last_step.is_some_and(|t| now - t < THERMAL_STEP) {
            return None;
        }
        self.last_step = Some(now);
        let t = reading?;
        let smoothed = self.smoothed.map_or(t, |prev| (prev + t) / 2.0);
        self.smoothed = Some(smoothed);
        self.account(now);
        let old = self.active;
        if smoothed >= self.max {
            let cut = ((smoothed - self.max) / 2.0).ceil().max(1.0) as usize;
            self.active = self.active.saturating_sub(cut);
        } else if smoothed <= self.max - THERMAL_MARGIN
            && now - self.last_change >= THERMAL_RAISE_AFTER
            && self.active < self.workers
        {
            let step = if smoothed <= self.max - 10.0 { 2 } else { 1 };
            self.active = (self.active + step).min(self.workers);
        }
        (self.active != old).then(|| {
            self.last_change = now;
            (old, self.active, smoothed)
        })
    }
}

struct Miner<'a> {
    config: &'a Config,
    out: Reporter,
    engine: Engine,
    events_tx: mpsc::Sender<Event>,
    pool_backlog: Arc<AtomicUsize>,
    found_backlog: Arc<AtomicUsize>,
    found_dropped: Arc<AtomicU64>,
    session: Option<Session>,
    session_counter: u64,
    clean_generation: u64,
    templates: HashMap<[u8; 32], Arc<AtomicU64>>,
    template_order: VecDeque<[u8; 32]>,
    queue: VecDeque<Share>,
    summary: Summary,
    started: Instant,
    /// Time spent mining with a ready pool session (for pool-view hashrate).
    mining_time: Duration,
    mining_since: Option<Instant>,
    meter: RateMeter,
    sensors: Sensors,
    last_report: (Instant, u64, u64),
    /// Per-worker hash counts at the last report (per-core-type rates).
    engine_snapshot: Option<Vec<u64>>,
    last_prevhash: Option<[u8; 32]>,
    last_height: Option<u64>,
    last_difficulty: Option<f64>,
    thermal: Option<ThermalController>,
    last_thermal_log: Option<Instant>,
}

impl Miner<'_> {
    fn total_hashes(&self) -> u64 {
        self.engine.total_hashes()
    }

    fn sample(&mut self) {
        let now = Instant::now();
        if self.meter.needs_sample(now) {
            let total = self.total_hashes();
            self.meter.push(now, total);
        }
    }

    /// Apply `--max-temp`: adjust how many workers hash.
    fn check_temperature(&mut self) {
        let Some(control) = self.thermal.as_mut() else {
            return;
        };
        let now = Instant::now();
        // Check before reading sensors: arguments are evaluated even when
        // update() would skip this step. Pool traffic must not drive sysfs I/O.
        if control.last_step.is_some_and(|t| now - t < THERMAL_STEP) {
            return;
        }
        let Some((old, new, t)) = control.update(now, self.sensors.temperature()) else {
            return;
        };
        let (max, workers) = (control.max, control.workers);
        self.engine.set_active(new);
        // Log the first throttle, full stop/start and full recovery at once;
        // otherwise at most every 20 s (the report always shows the state).
        let notable = self.last_thermal_log.is_none() || new == 0 || old == 0 || new == workers;
        if notable
            || self
                .last_thermal_log
                .is_some_and(|t| now - t >= Duration::from_secs(20))
        {
            self.last_thermal_log = Some(now);
            let (label, color) = if new < old {
                ("too hot", Color::Red)
            } else {
                ("cooler", Color::Green)
            };
            self.out.line(
                label,
                color,
                format!(
                    "CPU {t:.0}°C (limit {max:.0}°C): {old} -> {new} of {workers} workers hashing"
                ),
            );
        }
    }

    fn mining_elapsed(&self) -> Duration {
        self.mining_time + self.mining_since.map_or(Duration::ZERO, |t| t.elapsed())
    }

    fn connect(&mut self, endpoint: &Endpoint) -> Result<()> {
        let deadline = Instant::now() + self.config.connect_timeout;
        let (stream, tls) = transport::open(endpoint, deadline, &self.config.tls)?;
        stream
            .configure_mining(self.config.submit_timeout.max(WRITE_TIMEOUT))
            .context("configure pool socket")?;
        let progress = Arc::new(IoProgress::new());
        let control = ConnectionControl {
            socket: stream.control_socket()?,
            progress: progress.clone(),
        };
        self.session_counter += 1;
        self.clean_generation += 1;
        self.templates.clear();
        self.template_order.clear();
        let id = self.session_counter;
        let (outbound_tx, outbound_rx) = mpsc::sync_channel(MAX_OUTBOUND);
        let events = self.events_tx.clone();
        let backlog = self.pool_backlog.clone();
        std::thread::Builder::new()
            .name("tm-net".into())
            .spawn(move || io_loop(id, stream, outbound_rx, events, backlog, progress))?;
        let now = Instant::now();
        let mut session = Session {
            id,
            endpoint: endpoint.clone(),
            outbound: outbound_tx,
            control,
            dump: self.config.protocol_dump.then(|| self.out.clone()),
            next_id: 1,
            extranonce: None,
            authorized: false,
            next_difficulty: 1.0,
            latest_job: None,
            requests: HashMap::new(),
            inflight: 0,
            started: now,
            last_rx: now,
            ping: None,
            ready_at: None,
        };
        // Pipeline subscribe and authorize; pools accept either order.
        session.send("mining.subscribe", json!([AGENT]), Request::Subscribe)?;
        session.send(
            "mining.authorize",
            json!([self.config.user, self.config.password]),
            Request::Authorize,
        )?;
        self.summary.connections += 1;
        let transport = tls.map_or_else(|| "plain TCP".to_owned(), |t| format!("TLS: {t}"));
        self.out.line(
            "connected",
            Color::Blue,
            format!(
                "{}  {}",
                endpoint.url,
                self.out.paint(Color::Dim, format!("({transport})"))
            ),
        );
        self.session = Some(session);
        self.last_prevhash = None;
        Ok(())
    }

    fn disconnect(&mut self, reason: &str) {
        if let Some(session) = self.session.take() {
            if let Some(since) = self.mining_since.take() {
                self.mining_time += since.elapsed();
            }
            self.engine.publish(None);
            let lost = self.queue.len()
                + session
                    .requests
                    .values()
                    .filter(|r| matches!(r, Request::Submit { .. }))
                    .count();
            self.summary.discarded += lost as u64;
            self.queue.clear();
            let lost = if lost > 0 {
                format!(" ({lost} unconfirmed shares dropped)")
            } else {
                String::new()
            };
            self.out.line(
                "disconnect",
                Color::Yellow,
                format!("{}: {reason}{lost}", session.endpoint.url),
            );
        }
    }

    fn publish(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let (Some(job), Some((extranonce1, extranonce2_len)), true) = (
            session.latest_job.clone(),
            session.extranonce.clone(),
            session.authorized,
        ) else {
            return;
        };
        let session_id = session.id;
        let key = job.template_key(&extranonce1);
        let counter = match self.templates.get(&key) {
            Some(counter) => counter.clone(),
            None => {
                let counter = Arc::new(AtomicU64::new(0));
                self.templates.insert(key, counter.clone());
                self.template_order.push_back(key);
                while self.template_order.len() > MAX_TEMPLATES {
                    if let Some(old) = self.template_order.pop_front() {
                        self.templates.remove(&old);
                    }
                }
                counter
            }
        };
        self.announce_job(&job);
        let work = Work {
            session: session_id,
            clean_generation: self.clean_generation,
            submit_target: job.submit_target(),
            job,
            extranonce1,
            extranonce2_len,
            counter,
            extranonce2_space: extranonce2_space(extranonce2_len),
        };
        self.engine.publish(Some(Arc::new(work)));
    }

    /// "New block" when the previous block hash changes, difficulty changes,
    /// every job with --debug.
    fn announce_job(&mut self, job: &Job) {
        let height = coinbase_height(&job.coinbase1);
        let net = job.network_target.map(|t| t.difficulty());
        if self.last_prevhash != Some(job.prevhash) {
            self.last_prevhash = Some(job.prevhash);
            self.last_height = height;
            let height = height.map_or_else(|| "?".to_owned(), |h| format!("#{}", format_count(h)));
            self.out.line(
                "new block",
                Color::Magenta,
                format!(
                    "{}  job {}  net diff {}  pool diff {}",
                    self.out.paint(Color::Bold, height),
                    short_job(&job.id),
                    net.map_or_else(|| "?".to_owned(), format_diff),
                    format_diff(job.difficulty)
                ),
            );
        } else if self.config.debug {
            self.out.line(
                "job",
                Color::Dim,
                format!(
                    "{}  ntime {:08x}  clean {}",
                    short_job(&job.id),
                    job.ntime,
                    job.clean
                ),
            );
        }
        if self.last_difficulty != Some(job.difficulty) {
            if let Some(old) = self.last_difficulty {
                self.out.line(
                    "difficulty",
                    Color::Cyan,
                    format!(
                        "pool difficulty {} -> {}",
                        format_diff(old),
                        format_diff(job.difficulty)
                    ),
                );
            }
            self.last_difficulty = Some(job.difficulty);
        }
    }

    fn on_found(&mut self, found: Found) {
        self.found_backlog.fetch_sub(1, Relaxed);
        let current = self.session.as_ref().map(|s| s.id);
        if current != Some(found.work.session)
            || found.work.clean_generation != self.clean_generation
        {
            self.summary.discarded += 1;
            return;
        }
        let job = &found.work.job;
        let block = job
            .network_target
            .is_some_and(|n| n.is_met_by(&found.digest));
        let share = Share {
            session: found.work.session,
            clean_generation: found.work.clean_generation,
            job_id: job.id.clone(),
            extranonce2: hex::encode(extranonce2_bytes(
                found.extranonce2,
                found.work.extranonce2_len,
            )),
            ntime: format!("{:08x}", job.ntime),
            nonce: format!("{:08x}", found.nonce),
            difficulty: job.difficulty,
            share_difficulty: digest_difficulty(&found.digest),
            block,
            digest: found.digest,
            attempts: 0,
            not_before: Instant::now(),
        };
        if block {
            self.out.line(
                "BLOCK",
                Color::Magenta,
                format!(
                    "candidate found by worker {} (hash {})",
                    found.worker,
                    digest_display(&found.digest)
                ),
            );
            // Preserve bounded memory even if a pool advertises an extremely
            // easy network target and every worker reports block candidates.
            if self.queue.len() >= MAX_QUEUED_SHARES {
                self.queue.pop_back();
                self.summary.discarded += 1;
            }
            self.queue.push_front(share);
        } else if self.queue.len() < MAX_QUEUED_SHARES {
            self.queue.push_back(share);
        } else {
            self.summary.discarded += 1;
        }
    }

    fn pump(&mut self) -> Result<()> {
        let now = Instant::now();
        let Some(session) = self.session.as_mut() else {
            return Ok(());
        };
        if !session.ready() {
            return Ok(());
        }
        while session.inflight < self.config.max_inflight {
            let Some(position) = self.queue.iter().position(|s| s.not_before <= now) else {
                break;
            };
            let share = self.queue.remove(position).unwrap();
            if share.session != session.id || share.clean_generation != self.clean_generation {
                self.summary.discarded += 1;
                continue;
            }
            let params = json!([
                self.config.user,
                share.job_id,
                share.extranonce2,
                share.ntime,
                share.nonce
            ]);
            if share.block && share.attempts == 0 {
                self.summary.blocks_submitted += 1;
            }
            session.send(
                "mining.submit",
                params,
                Request::Submit { share, sent: now },
            )?;
            session.inflight += 1;
        }
        Ok(())
    }

    fn requeue(&mut self, mut share: Share, delay: Duration, why: &str) {
        share.attempts += 1;
        if share.attempts >= MAX_SUBMIT_ATTEMPTS {
            self.summary.discarded += 1;
            self.out.line(
                "dropped",
                Color::Yellow,
                format!("share dropped after {} attempts: {why}", share.attempts),
            );
            return;
        }
        self.summary.retried += 1;
        share.not_before = Instant::now() + delay;
        if self.queue.len() >= MAX_QUEUED_SHARES {
            self.queue.pop_back();
            self.summary.discarded += 1;
        }
        self.queue.push_front(share);
    }

    fn share_counts(&self) -> String {
        let s = &self.summary;
        let total = s.accepted + s.rejected + s.stale + s.duplicate;
        let percent = if total == 0 {
            100.0
        } else {
            s.accepted as f64 * 100.0 / total as f64
        };
        format!("{}/{} ({percent:.1}%)", s.accepted, total)
    }

    fn on_line(&mut self, value: Value) -> Result<()> {
        if self.config.protocol_dump {
            self.out.line("recv", Color::Dim, &value);
        }
        let session = self.session.as_mut().expect("line for live session");
        session.last_rx = Instant::now();
        // Traffic clears the silence watchdog because some pools (rplant) never
        // answer mining.ping. Share responses have their own independent deadline.
        if let Some(id) = session.ping.take() {
            session.requests.remove(&id);
        }
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let id = value.get("id").cloned().unwrap_or(Value::Null);
            match method {
                "mining.set_difficulty" => match params.get(0).and_then(Value::as_f64) {
                    Some(d) if d.is_finite() && d > 0.0 => session.next_difficulty = d,
                    _ => self.out.line(
                        "warning",
                        Color::Yellow,
                        "ignored invalid mining.set_difficulty",
                    ),
                },
                "mining.notify" => match Job::from_notify(&params, session.next_difficulty) {
                    Ok(job) => {
                        let clean = job.clean;
                        session.latest_job = Some(Arc::new(job));
                        if clean {
                            self.clean_generation += 1;
                            let before = self.queue.len();
                            let generation = self.clean_generation;
                            self.queue.retain(|s| s.clean_generation == generation);
                            self.summary.discarded += (before - self.queue.len()) as u64;
                        }
                        self.publish();
                    }
                    Err(error) => self.out.line(
                        "warning",
                        Color::Yellow,
                        format!("ignored invalid mining.notify: {error:#}"),
                    ),
                },
                "mining.set_extranonce" => {
                    let extranonce1 = params
                        .get(0)
                        .and_then(Value::as_str)
                        .and_then(|h| hex::decode(h).ok());
                    let size = params.get(1).and_then(Value::as_u64);
                    match (extranonce1, size) {
                        (Some(e1), Some(size @ 1..=32)) if e1.len() <= 32 => {
                            let new = (Arc::<[u8]>::from(e1), size as usize);
                            if session.extranonce.as_ref() != Some(&new) {
                                session.extranonce = Some(new);
                                self.clean_generation += 1;
                                self.templates.clear();
                                self.template_order.clear();
                                self.publish();
                            }
                        }
                        _ => self.out.line(
                            "warning",
                            Color::Yellow,
                            "ignored invalid mining.set_extranonce",
                        ),
                    }
                }
                "client.reconnect" => bail!("pool requested reconnect"),
                "client.show_message" => {
                    let text: String = params
                        .get(0)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(200)
                        .collect();
                    self.out.line("pool says", Color::Cyan, text);
                }
                "mining.ping" | "client.get_version" if !id.is_null() => {
                    let result = if method == "mining.ping" {
                        json!("pong")
                    } else {
                        json!(AGENT)
                    };
                    session.write(json!({"id": id, "result": result, "error": null}))?;
                }
                _ if !id.is_null() => {
                    session.write(json!({"id": id, "result": null, "error": [20, "unsupported method", null]}))?;
                }
                _ => {}
            }
            return Ok(());
        }
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            return Ok(());
        };
        let Some(request) = session.requests.remove(&id) else {
            return Ok(()); // Late reply to a request we already retried or abandoned.
        };
        let result = value.get("result").cloned().unwrap_or(Value::Null);
        let error = value.get("error").cloned().unwrap_or(Value::Null);
        match request {
            Request::Subscribe => {
                if !error.is_null() {
                    bail!("pool rejected subscribe: {}", pool_error(&error).1);
                }
                let extranonce1 = result
                    .get(1)
                    .and_then(Value::as_str)
                    .and_then(|h| hex::decode(h).ok())
                    .filter(|e| e.len() <= 32)
                    .context("invalid extranonce1 in subscribe result")?;
                let size = result
                    .get(2)
                    .and_then(Value::as_u64)
                    .filter(|s| (1..=32).contains(s))
                    .context("invalid extranonce2 size in subscribe result")?;
                session.extranonce = Some((extranonce1.into(), size as usize));
                self.after_handshake_step();
            }
            Request::Authorize => {
                if result != Value::Bool(true) {
                    let reason = if error.is_null() {
                        "authorization refused".to_owned()
                    } else {
                        pool_error(&error).1
                    };
                    return Err(anyhow::Error::new(Fatal(format!(
                        "pool rejected worker {:?}: {reason}",
                        self.config.user
                    ))));
                }
                session.authorized = true;
                self.after_handshake_step();
            }
            Request::Ping => session.ping = None,
            Request::Submit { share, sent } => {
                session.inflight = session.inflight.saturating_sub(1);
                let latency = sent.elapsed().as_millis();
                if result == Value::Bool(true) {
                    self.summary.accepted += 1;
                    self.summary.accepted_difficulty += share.difficulty;
                    self.summary.best_share_difficulty = self
                        .summary
                        .best_share_difficulty
                        .max(share.share_difficulty);
                    let rate = if self.meter.ready() {
                        format!("  {}", format_rate(self.meter.rate()))
                    } else {
                        String::new()
                    };
                    let message = format!(
                        "{}  diff {} / {}{rate}  {latency} ms{}",
                        self.out.paint(Color::Bold, self.share_counts()),
                        format_diff(share.share_difficulty),
                        format_diff(share.difficulty),
                        if share.block { "  BLOCK!" } else { "" }
                    );
                    if share.block {
                        self.out.line("BLOCK", Color::Magenta, message);
                    } else {
                        self.out.share_line("accepted", Color::Green, message);
                    }
                } else {
                    let (code, reason) = pool_error(&error);
                    let lower = reason.to_ascii_lowercase();
                    if lower.contains("queue full") || lower.contains("backend unavailable") {
                        self.requeue(share, Duration::from_millis(500), &reason);
                    } else if code == 21
                        || lower.contains("stale")
                        || lower.contains("job not found")
                    {
                        self.summary.stale += 1;
                        let counts = self.share_counts();
                        self.out
                            .share_line("stale", Color::Yellow, format!("{counts}  {reason}"));
                    } else if code == 22 || lower.contains("duplicate") {
                        self.summary.duplicate += 1;
                        let counts = self.share_counts();
                        self.out
                            .line("duplicate", Color::Yellow, format!("{counts}  {reason}"));
                    } else {
                        self.summary.rejected += 1;
                        self.summary.last_reject = Some(format!("[{code}] {reason}"));
                        let counts = self.share_counts();
                        self.out.line(
                            "rejected",
                            Color::Red,
                            format!(
                                "{counts}  [{code}] {reason}  (job {} ex2 {} ntime {} nonce {} diff {} hash {})",
                                short_job(&share.job_id),
                                share.extranonce2,
                                share.ntime,
                                share.nonce,
                                format_diff(share.share_difficulty),
                                digest_display(&share.digest)
                            ),
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn after_handshake_step(&mut self) {
        let session = self.session.as_mut().expect("live session");
        if session.ready() && session.ready_at.is_none() {
            session.ready_at = Some(Instant::now());
            let (e1, size) = session.extranonce.clone().unwrap();
            self.mining_since = Some(Instant::now());
            self.out.line(
                "authorized",
                Color::Blue,
                format!(
                    "{}  {}",
                    short_user(&self.config.user),
                    self.out.paint(
                        Color::Dim,
                        format!(
                            "(extranonce1 {}, extranonce2 {size} bytes)",
                            hex::encode(&e1)
                        )
                    )
                ),
            );
            self.publish();
        }
    }

    /// Timers: handshake deadline, keepalive ping, submit timeouts.
    fn housekeeping(&mut self) -> Result<()> {
        let now = Instant::now();
        let config = self.config;
        let Some(session) = self.session.as_mut() else {
            return Ok(());
        };
        if session.control.progress.write_expired() {
            bail!("pool socket write stalled for 5s");
        }
        if !session.ready() && now - session.started > config.handshake_timeout {
            bail!("pool did not complete subscribe/authorize in time");
        }
        if session.latest_job.is_none()
            && session
                .ready_at
                .is_some_and(|t| now - t >= config.handshake_timeout)
        {
            bail!("pool authorized but did not provide a valid mining job in time");
        }
        // Anchor the full silence budget to the last message, so scheduling a
        // ping late cannot extend the reconnect deadline.
        if now - session.last_rx >= config.idle_ping + config.ping_timeout {
            bail!("pool stopped answering (silence timeout)");
        }
        if session.ping.is_none() && now - session.last_rx >= config.idle_ping {
            let id = session.send("mining.ping", json!([]), Request::Ping)?;
            session.ping = Some(id);
        }
        if let Some((&id, Request::Submit { sent, .. })) =
            session.requests.iter().find(|(_, r)| match r {
                Request::Submit { sent, .. } => now - *sent >= config.submit_timeout,
                _ => false,
            })
        {
            // Incoming jobs only prove the receive side works. Retrying here
            // would let a broken submission path survive indefinitely.
            bail!(
                "pool did not answer share request {id} within {:.1}s",
                (now - *sent).as_secs_f64()
            );
        }
        Ok(())
    }

    fn rate_by_kind(&self, since: Duration, per_worker_delta: &[u64]) -> String {
        let mut by_kind: Vec<(CoreKind, u64)> = Vec::new();
        for (placement, hashes) in self.engine.placements.iter().zip(per_worker_delta) {
            match by_kind.iter_mut().find(|(k, _)| *k == placement.kind) {
                Some((_, sum)) => *sum += hashes,
                None => by_kind.push((placement.kind, *hashes)),
            }
        }
        if by_kind.len() < 2 {
            return String::new();
        }
        let parts: Vec<String> = by_kind
            .iter()
            .map(|(kind, h)| {
                let name = match kind {
                    CoreKind::Performance => "P-cores",
                    CoreKind::Efficiency => "E-cores",
                    CoreKind::Unknown => "cores",
                    CoreKind::Gpu => "GPU",
                };
                format!(
                    "{name} {}",
                    format_rate(*h as f64 / since.as_secs_f64().max(1e-9))
                )
            })
            .collect();
        format!("  ({})", parts.join(", "))
    }

    fn report(&mut self, force: bool) {
        let now = Instant::now();
        if !force && now - self.last_report.0 < self.config.stats_interval {
            return;
        }
        let per_worker = self.engine.hashes();
        let total: u64 = per_worker.iter().sum();
        let since = now - self.last_report.0;
        let current = (total - self.last_report.1) as f64 / since.as_secs_f64().max(1e-9);
        let average = total as f64 / (now - self.started).as_secs_f64().max(1e-9);
        let delta: Vec<u64> = match self.engine_snapshot.take() {
            Some(previous) => per_worker
                .iter()
                .zip(&previous)
                .map(|(a, b)| a - b)
                .collect(),
            None => per_worker.clone(),
        };
        self.engine_snapshot = Some(per_worker);
        let out = &self.out;
        let s = &self.summary;
        out.line(
            "report",
            Color::Cyan,
            out.paint(
                Color::Bold,
                format!("uptime {}", format_duration(now - self.started)),
            ),
        );
        out.detail(
            "hashrate",
            format!(
                "{} now, {} avg{}",
                out.paint(Color::Bold, format_rate(current)),
                format_rate(average),
                self.rate_by_kind(since, &delta)
            ),
        );
        let minutes = self.mining_elapsed().as_secs_f64() / 60.0;
        let per_minute = if minutes > 0.0 {
            s.accepted as f64 / minutes
        } else {
            0.0
        };
        out.detail(
            "shares",
            format!(
                "{} accepted, {} rejected, {} stale  ({}, {per_minute:.1}/min{})",
                out.paint(Color::Green, s.accepted),
                if s.rejected > 0 {
                    out.paint(Color::Red, s.rejected)
                } else {
                    "0".into()
                },
                s.stale,
                self.share_counts(),
                if s.best_share_difficulty > 0.0 {
                    format!(", best {}", format_diff(s.best_share_difficulty))
                } else {
                    String::new()
                }
            ),
        );
        let mining = self.mining_elapsed().as_secs_f64();
        if mining > 0.0 && s.accepted > 0 {
            out.detail(
                "pool view",
                format!(
                    "{} from accepted work (what the pool credits)",
                    format_rate(s.accepted_difficulty * HASHES_PER_DIFF / mining)
                ),
            );
        }
        if let Some(job) = self.session.as_ref().and_then(|s| s.latest_job.as_ref()) {
            let rate = self.meter.rate().max(current).max(1.0);
            let share_ttf = estimated_time(job.difficulty, rate);
            let mut network = format!(
                "block {}, pool diff {} (a share every ~{})",
                self.last_height
                    .map_or_else(|| "?".to_owned(), |h| format!("#{}", format_count(h))),
                format_diff(job.difficulty),
                format_duration(share_ttf)
            );
            if let Some(net) = job.network_target.map(|t| t.difficulty()) {
                network.push_str(&format!(
                    ", net diff {} (solo block every ~{})",
                    format_diff(net),
                    format_duration(estimated_time(net, rate))
                ));
            }
            out.detail("network", network);
        }
        let mut cpu = Vec::new();
        if let Some(t) = self.sensors.temperature() {
            let text = format!("{t:.0}°C");
            cpu.push(if t >= 95.0 {
                out.paint(Color::Red, text)
            } else {
                text
            });
        }
        for (kind, name) in [
            (CoreKind::Performance, "P"),
            (CoreKind::Efficiency, "E"),
            (CoreKind::Unknown, "cores"),
        ] {
            if let Some(mhz) = self.sensors.average_mhz(kind) {
                cpu.push(format!("{name} {:.2} GHz", mhz / 1000.0));
            }
        }
        let placements = &self.engine.placements;
        cpu.push(format!(
            "{} workers",
            placements.iter().filter(|p| p.gpu.is_none()).count()
        ));
        if let Some(gpu) = placements.iter().find_map(|p| p.gpu) {
            cpu.push(format!("GPU {} hashes", gpu.hashes));
        }
        if let Some(control) = self.thermal.as_mut() {
            let fraction = control.active_fraction(now);
            let state = format!(
                "limit {:.0}°C: {}/{} hashing (avg {:.0}%)",
                control.max,
                control.active,
                control.workers,
                fraction * 100.0
            );
            cpu.push(if control.active < control.workers {
                self.out.paint(Color::Yellow, state)
            } else {
                state
            });
        }
        out.detail("cpu", cpu.join(", "));
        self.last_report = (now, total, s.accepted);
    }
}

fn short_job(id: &str) -> String {
    if let Some((end, _)) = id.char_indices().nth(12) {
        format!("{}…", &id[..end])
    } else {
        id.to_owned()
    }
}

fn estimated_time(difficulty: f64, rate: f64) -> Duration {
    Duration::from_secs_f64((HASHES_PER_DIFF * difficulty / rate).clamp(0.0, 1e12))
}

fn redacted_message(mut message: Value) -> Value {
    if message["method"] == "mining.authorize"
        && let Some(params) = message["params"].as_array_mut()
        && let Some(password) = params.get_mut(1)
    {
        *password = json!("***");
    }
    message
}

/// Errors that must stop the miner instead of reconnecting (bad credentials).
#[derive(Debug)]
pub struct Fatal(pub String);

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Fatal {}

/// Failed attempts back off with jitter; a lost established session gets one
/// immediate recovery attempt. Rotate through configured pools on failure.
struct Reconnect {
    pool_index: usize,
    pools: usize,
    failures: u32,
    retry_at: Instant,
    max: Duration,
}

impl Reconnect {
    /// `healthy`: the session that just ended had been mining for over a minute.
    fn failed(&mut self, healthy: bool) {
        self.failures = if healthy { 1 } else { self.failures + 1 };
        self.pool_index = (self.pool_index + 1) % self.pools;
        let base = Duration::from_secs(1 << (self.failures.clamp(1, 6) - 1)).min(self.max);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let delay = (base + base.mul_f64(f64::from(nanos % 1000) / 4000.0)).min(self.max);
        self.retry_at = Instant::now() + delay;
    }

    fn lost_session(&mut self, established: bool, healthy: bool) {
        let immediate = established && (healthy || self.failures == 0);
        self.failed(healthy);
        if immediate {
            self.retry_at = Instant::now();
        }
    }
}

/// Mine until `stop` is set (or the time limit). Returns totals; errors only on
/// fatal conditions (rejected credentials, retries exhausted, worker failure).
pub fn run(config: &Config, stop: Arc<AtomicBool>) -> Result<Summary> {
    anyhow::ensure!(!config.pools.is_empty(), "no pool configured");
    let (events_tx, events_rx) = mpsc::channel::<Event>();
    let found_backlog = Arc::new(AtomicUsize::new(0));
    let found_dropped = Arc::new(AtomicU64::new(0));
    let sink = {
        let events = events_tx.clone();
        let backlog = found_backlog.clone();
        let dropped = found_dropped.clone();
        Arc::new(move |event: WorkerEvent| {
            if matches!(event, WorkerEvent::Found(_)) {
                // Bound memory if the target is absurdly easy (regtest) and the pool slow.
                if backlog.fetch_add(1, Relaxed) >= MAX_FOUND_BACKLOG {
                    backlog.fetch_sub(1, Relaxed);
                    dropped.fetch_add(1, Relaxed);
                    return;
                }
            }
            let _ = events.send(Event::Worker(event));
        }) as crate::engine::Sink
    };
    let engine = Engine::start(config.placements.clone(), config.nice, sink)?;
    let sensors = Sensors::new(
        config
            .placements
            .iter()
            .filter_map(|p| p.cpu.map(|c| (c, p.kind)))
            .collect(),
    );
    let mut miner = Miner {
        config,
        out: config.reporter.clone().background()?,
        engine,
        events_tx,
        pool_backlog: Arc::new(AtomicUsize::new(0)),
        found_backlog,
        found_dropped,
        session: None,
        session_counter: 0,
        clean_generation: 0,
        templates: HashMap::new(),
        template_order: VecDeque::new(),
        queue: VecDeque::new(),
        summary: Summary::default(),
        started: Instant::now(),
        mining_time: Duration::ZERO,
        mining_since: None,
        meter: RateMeter::new(Duration::from_secs(10)),
        sensors,
        last_report: (Instant::now(), 0, 0),
        engine_snapshot: None,
        last_prevhash: None,
        last_height: None,
        last_difficulty: None,
        thermal: config
            .max_temp
            .map(|max| ThermalController::new(max, config.placements.len(), Instant::now())),
        last_thermal_log: None,
    };
    if config.max_temp.is_some() && miner.sensors.temperature().is_none() {
        miner.out.line(
            "warning",
            Color::Yellow,
            "--max-temp: no CPU temperature sensor found (coretemp/k10temp); it has no effect",
        );
    }
    miner.engine.wait_ready(Duration::from_secs(60))?;
    miner.started = Instant::now();
    miner.last_report = (miner.started, 0, 0);

    let mut reconnect = Reconnect {
        pool_index: 0,
        pools: config.pools.len(),
        failures: 0,
        retry_at: Instant::now(),
        max: config.max_backoff,
    };
    let result = loop {
        if stop.load(Relaxed) {
            break Ok(());
        }
        if let Some(limit) = config.time_limit
            && miner.started.elapsed() >= limit
        {
            miner
                .out
                .line("time limit", Color::Cyan, format_duration(limit));
            break Ok(());
        }
        if miner.session.is_none() && Instant::now() >= reconnect.retry_at {
            let endpoint = config.pools[reconnect.pool_index].clone();
            if let Err(error) = miner.connect(&endpoint) {
                miner.out.line(
                    "error",
                    Color::Red,
                    format!("connect to {} failed: {error:#}", endpoint.url),
                );
                reconnect.failed(false);
                if config.retries.is_some_and(|r| reconnect.failures > r) {
                    break Err(anyhow::Error::new(Fatal(format!(
                        "giving up after {} failed connection attempts",
                        reconnect.failures
                    ))));
                }
            }
        }
        let wait = if miner.session.is_some() {
            Duration::from_millis(200)
        } else {
            reconnect
                .retry_at
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(200))
        };
        let step: Result<()> = match events_rx.recv_timeout(wait) {
            Ok(Event::Line {
                session,
                value,
                _permit,
            }) => {
                if miner.session.as_ref().is_some_and(|s| s.id == session) {
                    miner.on_line(value)
                } else {
                    Ok(())
                }
            }
            Ok(Event::Closed { session, error }) => {
                if miner.session.as_ref().is_some_and(|s| s.id == session) {
                    Err(anyhow::anyhow!(error))
                } else {
                    Ok(())
                }
            }
            Ok(Event::Worker(WorkerEvent::Found(found))) => {
                miner.on_found(found);
                Ok(())
            }
            Ok(Event::Worker(WorkerEvent::Failed { worker, error })) => {
                break Err(anyhow::anyhow!("worker {worker} failed: {error}"));
            }
            Ok(Event::Worker(WorkerEvent::Warning { message, .. })) => {
                miner.out.line("warning", Color::Yellow, message);
                Ok(())
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(anyhow::anyhow!("event channel closed"));
            }
        };
        let step = step
            .and_then(|()| miner.housekeeping())
            .and_then(|()| miner.pump());
        if let Err(error) = step {
            if error.downcast_ref::<Fatal>().is_some() {
                miner.disconnect("fatal error");
                break Err(error);
            }
            let lived = miner
                .session
                .as_ref()
                .and_then(|s| s.ready_at)
                .is_some_and(|t| t.elapsed() > Duration::from_secs(60));
            let established = miner.session.as_ref().is_some_and(|s| s.ready());
            miner.disconnect(&format!("{error:#}"));
            reconnect.lost_session(established, lived);
            if config.retries.is_some_and(|r| reconnect.failures > r) {
                break Err(anyhow::Error::new(Fatal(format!(
                    "giving up after {} failed sessions",
                    reconnect.failures
                ))));
            }
        }
        let dropped = miner.found_dropped.swap(0, Relaxed);
        miner.summary.discarded += dropped;
        miner.sample();
        miner.check_temperature();
        miner.report(false);
    };
    miner.report(true);
    miner.disconnect("shutting down");
    miner.summary.thermal_active_fraction = miner
        .thermal
        .as_mut()
        .map(|c| c.active_fraction(Instant::now()));
    miner.summary.hashes = miner.total_hashes();
    miner.summary.seconds = miner.started.elapsed().as_secs_f64();
    let summary = miner.summary.clone();
    miner.engine.stop();
    result.map(|()| summary)
}

/// Final summary for humans.
pub fn print_summary(out: &Reporter, summary: &Summary) {
    let rate = summary.hashes as f64 / summary.seconds.max(1e-9);
    out.line(
        "summary",
        Color::Cyan,
        out.paint(
            Color::Bold,
            format!(
                "{} hashes in {} ({} avg)",
                format_count(summary.hashes),
                format_duration(Duration::from_secs_f64(summary.seconds)),
                format_rate(rate)
            ),
        ),
    );
    out.detail(
        "shares",
        format!(
            "{} accepted, {} rejected, {} stale, {} duplicate, {} retried, {} discarded",
            summary.accepted,
            summary.rejected,
            summary.stale,
            summary.duplicate,
            summary.retried,
            summary.discarded
        ),
    );
    if summary.accepted > 0 {
        out.detail(
            "work",
            format!(
                "accepted difficulty {} (best share {}), {} connection(s)",
                format_diff(summary.accepted_difficulty),
                format_diff(summary.best_share_difficulty),
                summary.connections
            ),
        );
    }
    if let Some(fraction) = summary.thermal_active_fraction {
        out.detail(
            "thermal",
            format!(
                "--max-temp kept {:.0}% of worker time hashing",
                fraction * 100.0
            ),
        );
    }
    if summary.blocks_submitted > 0 {
        out.detail(
            "blocks",
            format!("{} candidate(s) submitted", summary.blocks_submitted),
        );
    }
    if let Some(reject) = &summary.last_reject {
        out.detail("last reject", reject);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_metadata_is_safe_to_display() {
        assert_eq!(short_job("12345678901é-more"), "12345678901é…");
        assert_eq!(short_job("é"), "é");
        assert_eq!(short_job("123456789012"), "123456789012");
        // This is a representable target, despite exceeding Duration's range.
        assert!(crate::target::Target::from_difficulty(1e30).is_ok());
        assert_eq!(
            estimated_time(1e30, 1.0),
            Duration::from_secs(1_000_000_000_000)
        );
        assert_eq!(
            estimated_time(f64::INFINITY, 1.0),
            Duration::from_secs(1_000_000_000_000)
        );
        assert_eq!(estimated_time(1.0, HASHES_PER_DIFF), Duration::from_secs(1));
    }

    #[test]
    fn protocol_dump_redacts_json_escaped_passwords() {
        for password in ["plain", "quote\"and\\slash\n", "", "é"] {
            let message =
                json!({"id": 2, "method": "mining.authorize", "params": ["worker", password]});
            let dump = redacted_message(message.clone());
            assert_eq!(dump["params"], json!(["worker", "***"]));
            assert_eq!(message["params"][1], password);
        }
        let message = json!({"id": 3, "method": "mining.submit", "params": ["worker", "job"]});
        assert_eq!(redacted_message(message.clone()), message);
    }

    struct TestIo {
        peer: TcpStream,
        control: ConnectionControl,
        outbound: mpsc::SyncSender<Vec<u8>>,
        events: mpsc::Receiver<Event>,
        thread: std::thread::JoinHandle<()>,
        backlog: Arc<AtomicUsize>,
    }

    fn test_io() -> TestIo {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = Stream::Plain(TcpStream::connect(listener.local_addr().unwrap()).unwrap());
        let (peer, _) = listener.accept().unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream.configure_mining(WRITE_TIMEOUT).unwrap();
        let progress = Arc::new(IoProgress::new());
        let control = ConnectionControl {
            socket: stream.control_socket().unwrap(),
            progress: progress.clone(),
        };
        let (outbound, rx) = mpsc::sync_channel(MAX_OUTBOUND);
        let (tx, events) = mpsc::channel();
        let backlog = Arc::new(AtomicUsize::new(0));
        let count = backlog.clone();
        let thread = std::thread::spawn(move || io_loop(1, stream, rx, tx, count, progress));
        TestIo {
            peer,
            control,
            outbound,
            events,
            thread,
            backlog,
        }
    }

    #[test]
    fn oversized_complete_frame_closes_connection() {
        let mut io = test_io();
        let bytes = format!("{{\"padding\":\"{}\"}}\n", "x".repeat(MAX_LINE));
        let _ = io.peer.write_all(bytes.as_bytes());
        let event = io.events.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(event, Event::Closed { error, .. } if error.contains("oversized")));
        io.thread.join().unwrap();
    }

    #[test]
    fn pool_flood_is_bounded_and_disconnects() {
        let mut io = test_io();
        io.peer
            .write_all("{}\n".repeat(MAX_POOL_BACKLOG + 1).as_bytes())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !io.thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(io.thread.is_finished());
        let mut lines = 0;
        loop {
            match io.events.recv_timeout(Duration::from_secs(1)).unwrap() {
                Event::Line { .. } => lines += 1,
                Event::Closed { error, .. } => {
                    assert!(error.contains("backlog"));
                    break;
                }
                _ => panic!("unexpected event"),
            }
        }
        assert_eq!(lines, MAX_POOL_BACKLOG);
        assert_eq!(io.backlog.load(Relaxed), 0);
        io.thread.join().unwrap();
    }

    #[test]
    fn session_teardown_interrupts_a_blocked_socket_write() {
        let io = test_io();
        socket2::SockRef::from(&io.control.socket)
            .set_send_buffer_size(4096)
            .unwrap();
        socket2::SockRef::from(&io.peer)
            .set_recv_buffer_size(4096)
            .unwrap();
        // Deliberately exceed socket capacity while the peer never reads.
        io.outbound.send(vec![b'x'; 8 * 1024 * 1024]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while io.control.progress.writing_since.load(Relaxed) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_ne!(io.control.progress.writing_since.load(Relaxed), 0);
        drop(io.control);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !io.thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let finished = io.thread.is_finished();
        let _ = io.peer.shutdown(Shutdown::Both);
        assert!(
            finished,
            "teardown must interrupt I/O without waiting for its timeout"
        );
        io.thread.join().unwrap();
    }

    #[test]
    fn pool_errors_are_bounded_and_printable() {
        assert_eq!(
            pool_error(&json!([21, "Share was stale!", null])),
            (21, "Share was stale!".into())
        );
        let (code, text) = pool_error(&json!([26, "x\u{7}".repeat(200), null]));
        assert_eq!(code, 26);
        assert_eq!(text.len(), 160);
        assert_eq!(pool_error(&json!("oops")), (-1, "oops".into()));
    }

    #[test]
    fn thermal_controller_parks_and_restores_workers() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut c = ThermalController::new(80.0, 32, t0);
        // 92 °C: cut ceil(12 / 2) = 6 workers.
        assert_eq!(c.update(at(0), Some(92.0)), Some((32, 26, 92.0)));
        // Rate-limited to one step per 2 s.
        assert_eq!(c.update(at(1000), Some(99.0)), None);
        // Smoothed (92 + 84) / 2 = 88: cut 4.
        assert_eq!(c.update(at(2000), Some(84.0)), Some((26, 22, 88.0)));
        // Smoothed (88 + 76) / 2 = 82: cut 1.
        assert_eq!(c.update(at(4000), Some(76.0)), Some((22, 21, 82.0)));
        // Smoothed (82 + 76) / 2 = 79, between the margin (77) and the limit: hold.
        assert_eq!(c.update(at(6000), Some(76.0)), None);
        // Cool, but not yet 6 s since the last change (at 4 s): hold.
        assert_eq!(c.update(at(8000), Some(70.0)), None);
        // Now 6 s later and well below: add 2.
        let step = c.update(at(10_000), Some(60.0)).unwrap();
        assert_eq!((step.0, step.1), (21, 23));
        // A missing reading changes nothing.
        assert_eq!(c.update(at(20_000), None), None);
        let fraction = c.active_fraction(at(20_000));
        assert!(fraction > 0.7 && fraction < 0.9, "{fraction}");
        // Extreme heat can park everything; recovery starts from zero.
        let mut hot = ThermalController::new(50.0, 4, t0);
        assert_eq!(hot.update(at(0), Some(99.0)).unwrap().1, 0);
    }

    #[test]
    fn users_are_shortened_for_display() {
        assert_eq!(
            short_user("TBa6n1DBKCubGT9hacEZ5967kPC45nPwpc"),
            "TBa6n1DB…C45nPwpc"
        );
        assert_eq!(
            short_user("TBa6n1DBKCubGT9hacEZ5967kPC45nPwpc.rig1"),
            "TBa6n1DB…C45nPwpc.rig1"
        );
        assert_eq!(short_user("short.rig"), "short.rig");
    }

    #[test]
    fn reconnect_backs_off_and_fails_over() {
        let mut r = Reconnect {
            pool_index: 0,
            pools: 2,
            failures: 0,
            retry_at: Instant::now(),
            max: Duration::from_secs(32),
        };
        let delay = |r: &Reconnect| r.retry_at.saturating_duration_since(Instant::now());
        r.lost_session(true, false);
        assert_eq!(delay(&r), Duration::ZERO, "first recovery is immediate");
        assert_eq!(r.pool_index, 1, "try the backup immediately");
        r.lost_session(true, false);
        assert!(
            delay(&r) >= Duration::from_secs(1),
            "flapping must back off"
        );
        assert_eq!(r.pool_index, 0);
        r.failed(false);
        assert_eq!(r.pool_index, 1);
        for _ in 0..10 {
            r.failed(false);
        }
        assert!(delay(&r) > Duration::from_secs(31) && delay(&r) <= Duration::from_secs(32));
        r.lost_session(true, true);
        assert_eq!(r.failures, 1);
        assert_eq!(
            delay(&r),
            Duration::ZERO,
            "healthy sessions reset recovery backoff"
        );
        r.max = Duration::from_secs(4);
        for _ in 0..10 {
            r.failed(false);
        }
        assert!(delay(&r) <= Duration::from_secs(5));
    }
}
