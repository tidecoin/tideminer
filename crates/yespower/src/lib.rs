//! Pure-Rust Tidecoin yespower (yespower 1.0, N = 2048, r = 8, no personalization,
//! 80-byte input), built for WebAssembly first.
//!
//! - One kernel, four backends: WASM SIMD128, x86-64 SSE2, AArch64, portable integers.
//! - `Hasher<K>` computes K hashes in lock-step. yespower's cost is the latency
//!   of data-dependent S-box loads; interleaving independent hashes fills those
//!   stalls, so K = 2 gives far more hashes per thread (per Web Worker) than K = 1.
//! - `scan` runs the nonce loop inside the module: no per-hash JS calls.
//!
//! Output is bit-identical to Openwall's reference and to `rust-yespower`
//! (checked by the tests against 19 scalar-reference vectors and random headers).

mod kernel;
mod sha;
pub mod simd;
#[cfg(feature = "web")]
pub mod web;

pub use kernel::Lane;
use simd::{Best, Vec128};

pub const HEADER_LEN: usize = 80;
pub const HASH_LEN: usize = 32;
/// Bytes of scratch per lane (V + S + XY + B), about 2.10 MiB.
pub const SCRATCH_BYTES: usize = 2048 * 1024 + 3 * 32768 + 1024 + 1024;
/// Bytes of S-box tables per lane (96 KiB), read at random on every pwxform step.
pub const SBOX_BYTES: usize = 3 * 32768;

/// Reusable context computing `K` hashes per call on backend `V`.
pub struct Hasher<const K: usize = 1, V: Vec128 = Best> {
    lanes: [Lane<V>; K],
}

impl<const K: usize, V: Vec128> Hasher<K, V> {
    /// Allocates `K * SCRATCH_BYTES`.
    pub fn new() -> Self {
        assert!(K >= 1, "at least one lane");
        Self {
            lanes: core::array::from_fn(|_| Lane::new()),
        }
    }

    /// Hash K headers at once. Output bytes are the raw digest (compare as a
    /// little-endian 256-bit integer against a target).
    pub fn hash(&mut self, headers: &[[u8; HEADER_LEN]; K]) -> [[u8; HASH_LEN]; K] {
        kernel::hash_lanes(&mut self.lanes, headers)
    }

    /// Hash `count` consecutive nonces starting at `start` (header bytes 76..80,
    /// little-endian), K at a time. Returns every nonce whose digest is `<= target`
    /// (target given as 32 bytes, most significant first) and the hashes done.
    pub fn scan(
        &mut self,
        header: &[u8; HEADER_LEN],
        start: u32,
        count: u32,
        target_be: &[u8; 32],
    ) -> (Vec<u32>, u32) {
        let mut found = Vec::new();
        let mut headers = [*header; K];
        let mut done = 0u32;
        while done < count {
            let nonces: [u32; K] =
                core::array::from_fn(|k| start.wrapping_add(done).wrapping_add(k as u32));
            for k in 0..K {
                headers[k][76..].copy_from_slice(&nonces[k].to_le_bytes());
            }
            let digests = self.hash(&headers);
            // A final partial group still hashes K nonces; report only those asked for.
            let take = (count - done).min(K as u32) as usize;
            for k in 0..take {
                if meets_target(&digests[k], target_be) {
                    found.push(nonces[k]);
                }
            }
            done += take as u32;
        }
        (found, done)
    }
}

impl<const K: usize, V: Vec128> Default for Hasher<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

/// One-shot hash with a temporary context.
pub fn hash(header: &[u8; HEADER_LEN]) -> [u8; HASH_LEN] {
    Hasher::<1>::new().hash(&[*header])[0]
}

/// True when `digest`, read as a little-endian 256-bit integer, is `<=` the
/// target given big-endian (the pool's convention).
pub fn meets_target(digest: &[u8; HASH_LEN], target_be: &[u8; 32]) -> bool {
    for i in 0..32 {
        let d = digest[31 - i];
        let t = target_be[i];
        if d != t {
            return d < t;
        }
    }
    true
}

/// A hash split around its memory-hard core, for engines that run only the core
/// (tideminer's Metal GPU worker): [`prepare`] gives the core's input, the engine
/// runs smix and returns the last 16 words of B, and [`finish`] gives the digest.
/// The same SHA-256 / PBKDF2 / HMAC code as [`Hasher::hash`].
#[derive(Clone, Copy, Debug)]
pub struct Prepared {
    /// The first 32 words of B: the PBKDF2-SHA256 output as little-endian words.
    pub b: [u32; 32],
    /// The first 32 bytes of that output: the final HMAC message.
    pub prehash: [u8; 32],
}

/// The CPU part before the memory-hard core.
pub fn prepare(header: &[u8; HEADER_LEN]) -> Prepared {
    let digest = sha::sha256(header);
    let mut b0 = [0u8; 128];
    sha::pbkdf2_sha256_1(&digest, &[], &mut b0);
    let mut b = [0u32; 32];
    for (word, bytes) in b.iter_mut().zip(b0.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    let mut prehash = [0u8; 32];
    prehash.copy_from_slice(&b0[..32]);
    Prepared { b, prehash }
}

/// The CPU part after the core: HMAC-SHA256 keyed by B's last 16 words.
pub fn finish(b_tail: &[u32; 16], prehash: &[u8; 32]) -> [u8; HASH_LEN] {
    let mut key = [0u8; 64];
    for (bytes, word) in key.as_chunks_mut::<4>().0.iter_mut().zip(b_tail) {
        *bytes = word.to_le_bytes();
    }
    sha::hmac_sha256(&key, prehash)
}

/// cpuminer-opt share target for a Stratum difficulty (yespower
/// factor 65536): high 128 bits `(1 / (D / 65536)) * 2^96`, low 128 bits all
/// ones. `None` outside the pool's accepted range.
pub fn share_target(difficulty: f64) -> Option<[u8; 32]> {
    if !(difficulty.is_finite() && difficulty > 1.0 / 65536.0) {
        return None;
    }
    let high = (1.0 / (difficulty / 65536.0)) * 79_228_162_514_264_337_593_543_950_336.0; // 2^96
    if !(1.0..340_282_366_920_938_463_463_374_607_431_768_211_456.0).contains(&high) {
        return None;
    }
    let mut target = [0xffu8; 32];
    target[..16].copy_from_slice(&(high as u128).to_be_bytes());
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "0000002009f42768de3cfb4e58fc56368c1477f87f60e248d7130df3fb8acd7f6208b83a72f90dd3ad8fe06c7f70d73f256f1e07185dcc217a58b9517c699226ac0297d2ad60ba61b62a021d9b7700f0";
    const HASH: &str = "9d90c21b5a0bb9566d2999c5d703d7327ee3ac97c020d387aa2dfd0700000000";

    fn header() -> [u8; 80] {
        hex::decode(HEADER).unwrap().try_into().unwrap()
    }

    #[test]
    fn known_answer_all_backends_and_widths() {
        assert_eq!(hex::encode(hash(&header())), HASH);
        let h = header();
        assert_eq!(
            hex::encode(Hasher::<1, simd::Portable>::new().hash(&[h])[0]),
            HASH
        );
        let pair = Hasher::<2>::new().hash(&[h, h]);
        assert_eq!(hex::encode(pair[0]), HASH);
        assert_eq!(hex::encode(pair[1]), HASH);
    }

    #[test]
    fn targets() {
        let one = share_target(1.0).unwrap();
        assert_eq!(
            hex::encode(one),
            format!("{}{}", "00010000000000000000000000000000", "ff".repeat(16))
        );
        assert_eq!(
            hex::encode(&share_target(0.054931640625).unwrap()[..16]),
            "00123456789abcdf0000000000000000"
        );
        assert!(share_target(1.0 / 65536.0).is_none());
        let mut digest = one;
        digest.reverse();
        assert!(meets_target(&digest, &one));
        digest[16] = 0x01;
        assert!(!meets_target(&digest, &one));
    }
}
