//! Miner-neutral yardstick: a one-connection Stratum pool using Stratum V1 that
//! re-hashes every submitted share and counts valid unique shares inside a fixed
//! window. Expected valid shares = hashes * P(share), so any honest miner is measured
//! by the same external metric (accepted work per second), independent of its own
//! hashrate reporting.
//!
//!   cargo run --release --example mockpool -- --port 3399 --difficulty 0.002 \
//!       --warmup 20 --seconds 60
//!   tideminer mine -o stratum+tcp://127.0.0.1:3399 -u ADDRESS.bench
//!   cpuminer -a yespower -N 2048 -R 8 -o stratum+tcp://127.0.0.1:3399 -u ADDRESS.bench -p x
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};
use tideminer::pow::Hasher;
use tideminer::target::Target;
use tideminer::work::Job;

fn arg(name: &str, default: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| default.to_owned())
}

fn main() -> Result<()> {
    let port: u16 = arg("--port", "3399").parse()?;
    let difficulty: f64 = arg("--difficulty", "0.002").parse()?;
    let warmup = Duration::from_secs_f64(arg("--warmup", "20").parse()?);
    let window = Duration::from_secs_f64(arg("--seconds", "60").parse()?);
    let target = Target::from_difficulty(difficulty)?;
    // P(share) = (target + 1) / 2^256, from the target's own difficulty scale.
    let probability = 1.0 / (65536.0 * target.difficulty());
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    eprintln!("mockpool listening on 127.0.0.1:{port}, difficulty {difficulty}");
    let (stream, peer) = listener.accept()?;
    eprintln!("miner connected from {peer}");
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let send = |w: &mut std::net::TcpStream, v: Value| -> Result<()> {
        let mut bytes = serde_json::to_vec(&v)?;
        bytes.push(b'\n');
        w.write_all(&bytes).context("write")
    };
    let extranonce1 = "aabbccdd";
    let notify = json!([
        "4242424242424242424242424242424242424242424242424242424242424242",
        "0000000000000000000000000000000000000000000000000000000000000001",
        "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff",
        "ffffffff0100f2052a01000000016a00000000",
        ["ab".repeat(32)],
        "20000000",
        "1d00ffff",
        "6553f123",
        true
    ]);
    let job = Job::from_notify(&notify, difficulty)?;
    let mut hasher = Hasher::new()?;
    let mut seen = HashSet::new();
    let (mut subscribed, mut authorized, mut announced) = (false, false, false);
    let mut started = None::<Instant>;
    let (mut valid, mut invalid, mut duplicate, mut outside) = (0u64, 0u64, 0u64, 0u64);
    let mut line = String::new();
    loop {
        if started.is_some_and(|t| t.elapsed() >= warmup + window) {
            break;
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => bail!("miner disconnected"),
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message["id"].clone();
        match message["method"].as_str().unwrap_or("") {
            "mining.subscribe" => {
                subscribed = true;
                send(
                    &mut writer,
                    json!({"id": id, "error": null, "result":
                    [[["mining.set_difficulty", "mm"], ["mining.notify", "mm"]], extranonce1, 4]}),
                )?;
            }
            "mining.authorize" => {
                authorized = true;
                send(
                    &mut writer,
                    json!({"id": id, "result": true, "error": null}),
                )?;
            }
            "mining.submit" => {
                let p = &message["params"];
                let field = |i: usize| p[i].as_str().unwrap_or("");
                let in_window = started.is_some_and(|t| t.elapsed() >= warmup);
                let ok = (|| -> Option<bool> {
                    let e2 = hex::decode(field(2)).ok()?;
                    let ntime = u32::from_str_radix(field(3), 16).ok()?;
                    let nonce = u32::from_str_radix(field(4), 16).ok()?;
                    if e2.len() != 4 || ntime != job.ntime || field(1) != job.id {
                        return Some(false);
                    }
                    let mut header = job.header(&hex::decode(extranonce1).ok()?, &e2);
                    header[76..].copy_from_slice(&nonce.to_le_bytes());
                    if !seen.insert(header) {
                        return None;
                    }
                    Some(target.is_met_by(&hasher.hash(&header).ok()?))
                })();
                let reply = match ok {
                    Some(true) => {
                        if in_window {
                            valid += 1
                        } else {
                            outside += 1
                        }
                        json!({"id": id, "result": true, "error": null})
                    }
                    None => {
                        duplicate += 1;
                        json!({"id": id, "result": false, "error": [22, "Duplicate share", null]})
                    }
                    Some(false) => {
                        invalid += 1;
                        json!({"id": id, "result": false, "error": [26, "Share was rejected!", null]})
                    }
                };
                send(&mut writer, reply)?;
            }
            _ if !id.is_null() => send(
                &mut writer,
                json!({"id": id, "result": true, "error": null}),
            )?,
            _ => {}
        }
        if subscribed && authorized && !announced {
            announced = true;
            send(
                &mut writer,
                json!({"id": null, "method": "mining.set_difficulty", "params": [difficulty]}),
            )?;
            send(
                &mut writer,
                json!({"id": null, "method": "mining.notify", "params": notify}),
            )?;
            started = Some(Instant::now());
        }
    }
    let seconds = window.as_secs_f64();
    let report = json!({
        "difficulty": difficulty,
        "window_seconds": seconds,
        "valid_shares": valid,
        "invalid": invalid,
        "duplicate": duplicate,
        "valid_outside_window": outside,
        "implied_hashrate": valid as f64 / probability / seconds,
        "relative_error_1sigma": 1.0 / (valid.max(1) as f64).sqrt(),
    });
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}
