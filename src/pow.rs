#[cfg(feature = "c-kernel")]
use anyhow::anyhow;
use anyhow::{Result, ensure};
use std::ops::Range;

pub const HEADER_LEN: usize = 80;
pub const NONCE_OFFSET: usize = 76;
pub const NONCE_SPACE: u64 = 1u64 << 32;
/// Scratch per hash lane (V, S, XY, B), excluding allocator rounding and stack.
pub const SCRATCH_BYTES: usize = tidecoin_yespower::SCRATCH_BYTES;

/// The yespower kernel compiled into this binary.
#[cfg(not(feature = "c-kernel"))]
pub const KERNEL: &str = "tidecoin-yespower (Rust)";
#[cfg(feature = "c-kernel")]
pub const KERNEL: &str = "rust-yespower 0.3.0 (Openwall C)";

pub const VECTOR_HEADER: &str = "0000002009f42768de3cfb4e58fc56368c1477f87f60e248d7130df3fb8acd7f6208b83a72f90dd3ad8fe06c7f70d73f256f1e07185dcc217a58b9517c699226ac0297d2ad60ba61b62a021d9b7700f0";
pub const VECTOR_HASH: &str = "9d90c21b5a0bb9566d2999c5d703d7327ee3ac97c020d387aa2dfd0700000000";

/// Canonical serialized header bytes; not cpuminer's intermediate word array.
#[derive(Clone)]
pub struct Header(pub [u8; HEADER_LEN]);

impl Header {
    pub fn from_hex(input: &str) -> Result<Self> {
        let mut bytes = [0; HEADER_LEN];
        hex::decode_to_slice(input, &mut bytes)?;
        Ok(Self(bytes))
    }

    pub fn set_nonce(&mut self, nonce: u32) {
        self.0[NONCE_OFFSET..].copy_from_slice(&nonce.to_le_bytes());
    }
}

/// Owned Tidecoin yespower context (yespower 1.0, N=2048, r=8, no personalization)
/// computing `K` hashes per call. The Rust kernel interleaves the K hashes to hide
/// S-box lookup latency (worthwhile on cores without SMT or with idle siblings);
/// K = 1 is the default and matches the C kernel's speed.
///
/// Create it on the worker thread after pinning: scratch (~2.10 MiB per lane) is
/// first touched there. Deliberately not `Send`: one hasher per thread.
pub struct Hasher<const K: usize = 1>(Inner<K>);

#[cfg(not(feature = "c-kernel"))]
type Inner<const K: usize> = tidecoin_yespower::Hasher<K>;

/// The C kernel has no lanes: K independent contexts, hashed one after another.
#[cfg(feature = "c-kernel")]
type Inner<const K: usize> = [rust_yespower::TidecoinHasher; K];

impl<const K: usize> Hasher<K> {
    pub fn new() -> Result<Self> {
        ensure!(K >= 1, "at least one lane");
        #[cfg(not(feature = "c-kernel"))]
        let inner = tidecoin_yespower::Hasher::<K>::new();
        #[cfg(feature = "c-kernel")]
        let inner = {
            let contexts = (0..K)
                .map(|_| rust_yespower::TidecoinHasher::new())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| anyhow!("yespower context allocation failed"))?;
            contexts
                .try_into()
                .map_err(|_| anyhow!("yespower lane count"))?
        };
        Ok(Self(inner))
    }

    /// Hash K headers at once; raw digests, compared as little-endian integers.
    #[inline]
    pub fn hash_lanes(&mut self, headers: &[[u8; HEADER_LEN]; K]) -> Result<[[u8; 32]; K]> {
        #[cfg(not(feature = "c-kernel"))]
        return Ok(self.0.hash(headers));
        #[cfg(feature = "c-kernel")]
        {
            let mut out = [[0u8; 32]; K];
            for ((context, header), digest) in self.0.iter_mut().zip(headers).zip(&mut out) {
                *digest = context
                    .hash(header)
                    .map_err(|_| anyhow!("yespower hash failed"))?;
            }
            Ok(out)
        }
    }

    /// Known-answer test in every lane; also performs the first-touch allocation.
    pub fn self_test(&mut self) -> Result<()> {
        let header = Header::from_hex(VECTOR_HEADER)?.0;
        for digest in self.hash_lanes(&[header; K])? {
            ensure!(
                hex::encode(digest) == VECTOR_HASH,
                "yespower known-answer test failed"
            );
        }
        Ok(())
    }
}

impl Hasher<1> {
    #[inline]
    pub fn hash(&mut self, header: &[u8; HEADER_LEN]) -> Result<[u8; 32]> {
        Ok(self.hash_lanes(&[*header])?[0])
    }
}

/// One-off hash with a temporary context. Mining loops keep a [`Hasher`] instead.
pub fn hash_once(header: &Header) -> Result<[u8; 32]> {
    Hasher::<1>::new()?.hash(&header.0)
}

pub fn self_test() -> Result<()> {
    Hasher::<1>::new()?.self_test()
}

/// Partition the full nonce space without wrapping or losing 0xffffffff.
pub fn nonce_range(worker: usize, workers: usize) -> Result<Range<u64>> {
    ensure!(
        workers > 0 && workers as u64 <= NONCE_SPACE,
        "invalid worker count"
    );
    ensure!(worker < workers, "invalid worker index");
    Ok(
        (u128::from(NONCE_SPACE) * worker as u128 / workers as u128) as u64
            ..(u128::from(NONCE_SPACE) * (worker as u128 + 1) / workers as u128) as u64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_answer() {
        self_test().unwrap();
    }

    #[test]
    fn hasher_is_reusable_and_matches_one_off() {
        let mut hasher = Hasher::<1>::new().unwrap();
        let mut header = Header([0x11; HEADER_LEN]);
        for nonce in [0, 1, u32::MAX] {
            header.set_nonce(nonce);
            assert_eq!(hasher.hash(&header.0).unwrap(), hash_once(&header).unwrap());
        }
    }

    #[test]
    fn two_lanes_match_single_lane() {
        let mut one = Hasher::<1>::new().unwrap();
        let mut two = Hasher::<2>::new().unwrap();
        two.self_test().unwrap();
        let (mut a, mut b) = (Header([0x21; HEADER_LEN]), Header([0x42; HEADER_LEN]));
        a.set_nonce(7);
        b.set_nonce(u32::MAX);
        let pair = two.hash_lanes(&[a.0, b.0]).unwrap();
        assert_eq!(pair[0], one.hash(&a.0).unwrap());
        assert_eq!(pair[1], one.hash(&b.0).unwrap());
    }

    #[test]
    fn uneven_partitions_cover_every_nonce() {
        for workers in [1, 3, 7, 24, 32] {
            let mut end = 0;
            for worker in 0..workers {
                let range = nonce_range(worker, workers).unwrap();
                assert_eq!(range.start, end);
                assert!(!range.is_empty());
                end = range.end;
            }
            assert_eq!(end, NONCE_SPACE);
        }
        assert!(nonce_range(0, 0).is_err());
        assert!(nonce_range(3, 3).is_err());
    }

    #[test]
    fn nonce_changes_only_last_four_bytes() {
        let mut header = Header([0x55; HEADER_LEN]);
        header.set_nonce(0x12345678);
        assert_eq!(&header.0[..76], &[0x55; 76]);
        assert_eq!(&header.0[76..], &[0x78, 0x56, 0x34, 0x12]);
    }
}
