//! Tidecoin-only yespower miner (yespower 1.0, N=2048, r=8).
pub mod benchmark;
pub mod engine;
#[cfg(all(target_os = "macos", feature = "gpu"))]
pub mod gpu;
pub mod miner;
pub mod os;
pub mod pow;
pub mod report;
pub mod stratum;
pub mod target;
pub mod topology;
pub mod transport;
pub mod tune;
pub mod work;
