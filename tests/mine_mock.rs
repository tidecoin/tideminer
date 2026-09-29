//! End-to-end mining against an in-process Stratum V1 pool and validates every share independently: header rebuilt here from the
//! pool recipe, yespower recomputed, digest compared little-endian and inclusive.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tideminer::engine::{Layout, plan};
use tideminer::miner::{self, Config};
use tideminer::stratum::Endpoint;
use tideminer::target::Target;
use tideminer::topology::Topology;

const ADDRESS: &str = "rtbc1qexampleexampleexampleexampleexample.rig1";

struct PoolJob {
    prevhash: String,
    coinbase1: String,
    coinbase2: String,
    branches: Vec<String>,
    version: String,
    nbits: String,
    ntime: String,
    target: Target,
}

impl PoolJob {
    fn new(prev_byte: u8, ntime: u32, difficulty: f64) -> Self {
        Self {
            prevhash: format!("{prev_byte:02x}").repeat(32),
            coinbase1:
                "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff"
                    .into(),
            coinbase2: "ffffffff0100f2052a01000000016a00000000".into(),
            branches: vec!["ab".repeat(32), "cd".repeat(32)],
            version: "20000000".into(),
            nbits: "1d00ffff".into(),
            ntime: format!("{ntime:08x}"),
            target: Target::from_difficulty(difficulty).unwrap(),
        }
    }

    fn notify(&self, id: &str, clean: bool) -> Value {
        json!({"id": null, "method": "mining.notify", "params": [id, self.prevhash,
            self.coinbase1, self.coinbase2, self.branches, self.version, self.nbits, self.ntime, clean]})
    }

    /// Independent reconstruction from the Stratum coinbase and header fields.
    fn header(&self, extranonce1: &str, extranonce2: &str, ntime: &str, nonce: &str) -> [u8; 80] {
        let sha256d = |d: &[u8]| -> [u8; 32] { Sha256::digest(Sha256::digest(d)).into() };
        let coinbase = hex::decode(format!(
            "{}{extranonce1}{extranonce2}{}",
            self.coinbase1, self.coinbase2
        ))
        .unwrap();
        let mut root = sha256d(&coinbase);
        for branch in &self.branches {
            let mut pair = root.to_vec();
            pair.extend(hex::decode(branch).unwrap());
            root = sha256d(&pair);
        }
        let le = |h: &str| u32::from_str_radix(h, 16).unwrap().to_le_bytes();
        let mut header = Vec::with_capacity(80);
        header.extend(le(&self.version));
        for word in hex::decode(&self.prevhash).unwrap().chunks(4) {
            header.extend(word.iter().rev());
        }
        header.extend(root);
        header.extend(le(ntime));
        header.extend(le(&self.nbits));
        header.extend(le(nonce));
        header.try_into().unwrap()
    }
}

#[derive(Default)]
struct Tally {
    accepted: u64,
    stale: u64,
    invalid: Vec<String>,
    retried_identical: bool,
    connections: u64,
}

fn send(stream: &mut TcpStream, message: Value) {
    let mut bytes = serde_json::to_vec(&message).unwrap();
    bytes.push(b'\n');
    let _ = stream.write_all(&bytes);
}

/// Phases per connection 1: job A (diff 0.01), after 4 shares a pure retarget to B
/// (same template, ntime+1, clean=false), after 4 more a clean job C, after 3 more
/// the pool drops the connection. Connection 2: job D, 3 shares, then stop.
fn pool(listener: TcpListener, stop: Arc<AtomicBool>, tally: Arc<Mutex<Tally>>) {
    let mut seen_headers = HashSet::new();
    for connection in 0..2u32 {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        tally.lock().unwrap().connections += 1;
        let extranonce1 = if connection == 0 {
            "aabbccdd"
        } else {
            "11223344"
        };
        let mut jobs: HashMap<String, PoolJob> = HashMap::new();
        let mut current_parent: u8 = 0;
        let mut parents: HashMap<String, u8> = HashMap::new();
        let mut accepted_here = 0u64;
        let mut phase = 0;
        let mut subscribed = false;
        let mut authorized = false;
        let mut refused_once: Option<Value> = None;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let message: Value = serde_json::from_str(&line).unwrap();
            let id = message["id"].clone();
            match message["method"].as_str().unwrap_or("") {
                "mining.subscribe" => {
                    subscribed = true;
                    send(
                        &mut writer,
                        json!({"id": id, "error": null,
                        "result": [[["mining.set_difficulty", "mm"], ["mining.notify", "mm"]], extranonce1, 4]}),
                    );
                }
                "mining.authorize" => {
                    assert_eq!(message["params"][0], ADDRESS);
                    authorized = true;
                    send(
                        &mut writer,
                        json!({"id": id, "result": true, "error": null}),
                    );
                }
                "mining.ping" => send(
                    &mut writer,
                    json!({"id": id, "result": true, "error": null}),
                ),
                "mining.submit" => {
                    let p = &message["params"];
                    let job_id = p[1].as_str().unwrap().to_owned();
                    let (e2, ntime, nonce) = (
                        p[2].as_str().unwrap(),
                        p[3].as_str().unwrap(),
                        p[4].as_str().unwrap(),
                    );
                    // First submit of connection 1 gets "submit queue full" once.
                    if connection == 0 && refused_once.is_none() {
                        refused_once = Some(p.clone());
                        send(
                            &mut writer,
                            json!({"id": id, "result": false, "error": [20, "submit queue full", null]}),
                        );
                        continue;
                    }
                    if refused_once.as_ref() == Some(p) {
                        tally.lock().unwrap().retried_identical = true;
                    }
                    let Some(job) = jobs.get(&job_id) else {
                        tally
                            .lock()
                            .unwrap()
                            .invalid
                            .push(format!("unknown job {job_id}"));
                        continue;
                    };
                    let mut problems = Vec::new();
                    if p[0] != ADDRESS {
                        problems.push("worker");
                    }
                    if e2.len() != 8 || ntime.len() != 8 || nonce.len() != 8 {
                        problems.push("field width");
                    }
                    if ntime != job.ntime {
                        problems.push("ntime rolled");
                    }
                    let header = job.header(extranonce1, e2, ntime, nonce);
                    // An independent implementation checks every share: the Openwall C,
                    // or on Windows (where it does not build) the Rust kernel.
                    #[cfg(not(windows))]
                    let digest = rust_yespower::TidecoinHasher::new()
                        .unwrap()
                        .hash(&header)
                        .unwrap();
                    #[cfg(windows)]
                    let digest = tidecoin_yespower::hash(&header);
                    if !job.target.is_met_by(&digest) {
                        problems.push("low difficulty");
                    }
                    if !seen_headers.insert(header) {
                        problems.push("duplicate");
                    }
                    if !problems.is_empty() {
                        tally
                            .lock()
                            .unwrap()
                            .invalid
                            .push(format!("{job_id}: {problems:?}"));
                        send(
                            &mut writer,
                            json!({"id": id, "result": false, "error": [26, "Share was rejected!", null]}),
                        );
                        continue;
                    }
                    if parents[&job_id] != current_parent {
                        tally.lock().unwrap().stale += 1;
                        send(
                            &mut writer,
                            json!({"id": id, "result": false, "error": [21, "Share was stale!", null]}),
                        );
                        continue;
                    }
                    tally.lock().unwrap().accepted += 1;
                    accepted_here += 1;
                    send(
                        &mut writer,
                        json!({"id": id, "result": true, "error": null}),
                    );
                }
                other => panic!("unexpected client method {other}"),
            }
            let announce = |writer: &mut TcpStream,
                            jobs: &mut HashMap<String, PoolJob>,
                            parents: &mut HashMap<String, u8>,
                            id: &str,
                            job: PoolJob,
                            difficulty: f64,
                            clean: bool,
                            parent: u8| {
                send(
                    writer,
                    json!({"id": null, "method": "mining.set_difficulty", "params": [difficulty]}),
                );
                send(writer, job.notify(id, clean));
                parents.insert(id.to_owned(), parent);
                jobs.insert(id.to_owned(), job);
            };
            if subscribed && authorized && phase == 0 {
                phase = 1;
                let (id, parent) = if connection == 0 {
                    ("a".repeat(64), 1)
                } else {
                    ("d".repeat(64), 4)
                };
                current_parent = parent;
                announce(
                    &mut writer,
                    &mut jobs,
                    &mut parents,
                    &id,
                    PoolJob::new(parent, 1_700_000_000, 0.01),
                    0.01,
                    true,
                    parent,
                );
            } else if connection == 0 && phase == 1 && accepted_here >= 4 {
                phase = 2;
                announce(
                    &mut writer,
                    &mut jobs,
                    &mut parents,
                    &"b".repeat(64),
                    PoolJob::new(1, 1_700_000_001, 0.02),
                    0.02,
                    false,
                    1,
                );
            } else if connection == 0 && phase == 2 && accepted_here >= 8 {
                phase = 3;
                current_parent = 3;
                announce(
                    &mut writer,
                    &mut jobs,
                    &mut parents,
                    &"c".repeat(64),
                    PoolJob::new(3, 1_700_000_060, 0.01),
                    0.01,
                    true,
                    3,
                );
            } else if connection == 0 && phase == 3 && accepted_here >= 11 {
                break; // drop the connection: the miner must reconnect
            } else if connection == 1 && accepted_here >= 3 {
                stop.store(true, Ordering::SeqCst);
                // Keep answering until the miner closes.
                phase = 9;
            }
        }
    }
}

#[test]
fn mines_valid_shares_through_retarget_clean_job_and_reconnect() {
    mine_against_mock_pool(1);
}

/// Same scenario with two interleaved hashes per worker (odd nonces in lane 1).
#[test]
fn two_lane_workers_mine_valid_shares() {
    mine_against_mock_pool(2);
}

fn mine_against_mock_pool(lanes: usize) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("stratum+tcp://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let tally = Arc::new(Mutex::new(Tally::default()));
    let pool_thread = {
        let (stop, tally) = (stop.clone(), tally.clone());
        std::thread::spawn(move || pool(listener, stop, tally))
    };
    let topology = Topology::detect();
    let threads = topology.cpus.len().min(2);
    let mut placements = plan(&topology, Layout::All, Some(threads), None).unwrap();
    for placement in &mut placements {
        placement.lanes = lanes;
    }
    let mut config = Config::new(
        vec![Endpoint::parse(&url).unwrap()],
        ADDRESS.into(),
        "x".into(),
        placements,
    );
    config.reporter = tideminer::report::Reporter::silent();
    let watchdog = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            while !stop.load(Ordering::SeqCst) && started.elapsed() < Duration::from_secs(240) {
                std::thread::sleep(Duration::from_millis(100));
            }
            stop.store(true, Ordering::SeqCst);
        })
    };
    let summary = miner::run(&config, stop.clone()).unwrap();
    watchdog.join().unwrap();
    pool_thread.join().unwrap();
    let tally = tally.lock().unwrap();
    assert!(
        tally.invalid.is_empty(),
        "invalid shares: {:?}",
        tally.invalid
    );
    assert_eq!(
        tally.connections, 2,
        "miner must reconnect after the pool drops it"
    );
    assert!(tally.accepted >= 14, "accepted {}", tally.accepted);
    assert!(
        tally.retried_identical,
        "queue-full share must be resent identically"
    );
    assert_eq!(summary.rejected, 0);
    assert_eq!(summary.duplicate, 0);
    assert_eq!(summary.stale, tally.stale);
    assert!(summary.accepted <= tally.accepted);
    assert!(summary.retried >= 1);
    assert_eq!(summary.connections, 2);
}

/// A pool that never answers mining.ping (like rplant) but keeps sending jobs
/// must not be treated as dead: any traffic clears an outstanding ping.
#[test]
fn unanswered_pings_do_not_disconnect_a_live_pool() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("stratum+tcp://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let pings = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let connections = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let pool_thread = {
        let (stop, pings, connections) = (stop.clone(), pings.clone(), connections.clone());
        std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let mut clients = Vec::new();
            let mut job = 0u32;
            let mut last_job = Instant::now();
            while !stop.load(Ordering::SeqCst) {
                if let Ok((stream, _)) = listener.accept() {
                    connections.fetch_add(1, Ordering::SeqCst);
                    stream.set_nonblocking(true).unwrap();
                    clients.push((BufReader::new(stream.try_clone().unwrap()), stream));
                }
                for (reader, writer) in &mut clients {
                    let mut line = String::new();
                    while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                        let message: Value = serde_json::from_str(&line).unwrap();
                        let id = message["id"].clone();
                        match message["method"].as_str() {
                            Some("mining.subscribe") => send(
                                writer,
                                json!({"id": id, "error": null,
                                "result": [[], "aabbccdd", 4]}),
                            ),
                            Some("mining.authorize") => {
                                send(writer, json!({"id": id, "result": true, "error": null}));
                                // Difficulty so high that no share is found: only jobs flow.
                                send(
                                    writer,
                                    json!({"id": null, "method": "mining.set_difficulty", "params": [1_000_000.0]}),
                                );
                            }
                            Some("mining.ping") => {
                                pings.fetch_add(1, Ordering::SeqCst); // never answered
                            }
                            _ => {}
                        }
                        line.clear();
                    }
                }
                // A new job every 800 ms: quieter than the 500 ms idle ping.
                if last_job.elapsed() >= Duration::from_millis(800) {
                    last_job = Instant::now();
                    job += 1;
                    let notify = PoolJob::new(1, 1_700_000_000 + job, 1_000_000.0)
                        .notify(&format!("{job:064x}"), false);
                    for (_, writer) in &mut clients {
                        send(writer, notify.clone());
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let topology = Topology::detect();
    let placements = plan(&topology, Layout::All, Some(1), None).unwrap();
    let mut config = Config::new(
        vec![Endpoint::parse(&url).unwrap()],
        ADDRESS.into(),
        "x".into(),
        placements,
    );
    config.reporter = tideminer::report::Reporter::silent();
    config.idle_ping = Duration::from_millis(500);
    config.ping_timeout = Duration::from_millis(1000);
    config.time_limit = Some(Duration::from_secs(5));
    let summary = miner::run(&config, stop.clone()).unwrap();
    stop.store(true, Ordering::SeqCst);
    pool_thread.join().unwrap();
    assert!(
        pings.load(Ordering::SeqCst) >= 1,
        "the miner should have pinged"
    );
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "no reconnect while jobs flow"
    );
    assert_eq!(summary.connections, 1);
}
