//! Native throughput of the Rust kernel (native only).
//!   single thread, every lane width vs the C:  speed [seconds]
//!   alternating C vs Rust K=1, R rounds:       speed <seconds> ab [R]
//!   sustained, N unpinned threads x lanes:     speed <seconds> <threads> <lanes>
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};
use tidecoin_yespower::Hasher;

fn run<const K: usize>(seconds: f64) -> f64 {
    let mut h = Hasher::<K>::new();
    let mut headers = [[0x11u8; 80]; K];
    let mut n = 0u32;
    h.hash(&headers);
    let t = Instant::now();
    while t.elapsed().as_secs_f64() < seconds {
        for (k, header) in headers.iter_mut().enumerate() {
            header[76..].copy_from_slice(&(n + k as u32).to_le_bytes());
        }
        std::hint::black_box(h.hash(&headers));
        n += K as u32;
    }
    f64::from(n) / t.elapsed().as_secs_f64()
}

/// The C reference is not built on Windows (MSVC); it reports no rate there.
#[cfg(windows)]
fn c_rate(_seconds: f64) -> f64 {
    f64::NAN
}

#[cfg(not(windows))]
fn c_rate(seconds: f64) -> f64 {
    let mut c = rust_yespower::TidecoinHasher::new().unwrap();
    let mut header = [0x11u8; 80];
    c.hash(&header).unwrap();
    let (mut n, t) = (0u32, Instant::now());
    while t.elapsed().as_secs_f64() < seconds {
        header[76..].copy_from_slice(&n.to_le_bytes());
        std::hint::black_box(c.hash(&header).unwrap());
        n += 1;
    }
    f64::from(n) / t.elapsed().as_secs_f64()
}

/// Same shape as bench/bench-workers.mjs: 3 s warmup, then count for `seconds`.
fn sustained<const K: usize>(seconds: f64, threads: usize) -> f64 {
    let phase = Arc::new(AtomicU8::new(0));
    let workers: Vec<_> = (0..threads)
        .map(|id| {
            let phase = phase.clone();
            std::thread::spawn(move || {
                let mut h = Hasher::<K>::new();
                let mut headers = [[id as u8; 80]; K];
                let (mut n, mut counted) = (0u32, 0u64);
                loop {
                    let p = phase.load(Ordering::Relaxed);
                    if p == 2 {
                        return counted;
                    }
                    for (k, header) in headers.iter_mut().enumerate() {
                        header[76..].copy_from_slice(&(n + k as u32).to_le_bytes());
                    }
                    std::hint::black_box(h.hash(&headers));
                    n += K as u32;
                    if p == 1 {
                        counted += K as u64;
                    }
                }
            })
        })
        .collect();
    std::thread::sleep(Duration::from_secs(3));
    phase.store(1, Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs_f64(seconds));
    phase.store(2, Ordering::Relaxed);
    let total: u64 = workers.into_iter().map(|w| w.join().unwrap()).sum();
    total as f64 / seconds
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seconds: f64 = args.get(1).map_or(3.0, |s| s.parse().unwrap());
    match args.get(2).map(String::as_str) {
        Some("ab") => {
            // Alternate which runs first (C, R / R, C / ...) to cancel thermal drift.
            let rounds: usize = args.get(3).map_or(4, |r| r.parse().unwrap());
            let (mut c_sum, mut r_sum) = (0.0, 0.0);
            for round in 0..rounds {
                let (c, r) = if round % 2 == 0 {
                    let c = c_rate(seconds);
                    (c, run::<1>(seconds))
                } else {
                    let r = run::<1>(seconds);
                    (c_rate(seconds), r)
                };
                println!(
                    "round {round}: C {c:.0}  Rust K=1 {r:.0}  ({:+.1}%)",
                    (r / c - 1.0) * 100.0
                );
                c_sum += c;
                r_sum += r;
            }
            let n = rounds as f64;
            println!(
                "mean: C {:.0}  Rust K=1 {:.0}  ({:+.2}%)",
                c_sum / n,
                r_sum / n,
                (r_sum / c_sum - 1.0) * 100.0
            );
        }
        Some(threads) => {
            let threads: usize = threads.parse().unwrap();
            let lanes = args.get(3).map_or("1", String::as_str);
            let rate = match lanes {
                "1" => sustained::<1>(seconds, threads),
                "2" => sustained::<2>(seconds, threads),
                _ => panic!("lanes must be 1 or 2"),
            };
            println!(
                "native threads={threads} lanes={lanes}: {rate:.0} H/s total, {:.0} per thread",
                rate / threads as f64
            );
        }
        None => {
            println!("C optimized   {:7.0} H/s", c_rate(seconds));
            println!("Rust K=1      {:7.0} H/s", run::<1>(seconds));
            println!("Rust K=2      {:7.0} H/s", run::<2>(seconds));
            println!("Rust K=3      {:7.0} H/s", run::<3>(seconds));
            println!("Rust K=4      {:7.0} H/s", run::<4>(seconds));
        }
    }
}
