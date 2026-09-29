//! Share and network targets using the cpuminer-opt yespower convention.
//!
//! Uses cpuminer-opt `diff_to_hash` with yespower's `opt_target_factor = 65536`:
//! the high 128 bits are `(1 / (D / 65536)) * 2^96` truncated, the low 128 bits
//! are all ones. Difficulty 1 is therefore `2^240 + 2^128 - 1`, not Bitcoin's
//! `0x1d00ffff`. A hash qualifies when its yespower digest, read as a
//! little-endian 256-bit integer, is `<=` the target.
use anyhow::{Result, ensure};
use std::cmp::Ordering;

/// Supported difficulty floor: difficulties at or below 1/65536 are rejected.
pub const UNSAFE_DIFFICULTY: f64 = 1.0 / 65536.0;

/// Unsigned 256-bit threshold; limbs are little-endian (`limbs[3]` most significant).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target([u64; 4]);

impl Ord for Target {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.iter().rev().cmp(other.0.iter().rev())
    }
}

impl PartialOrd for Target {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Target {
    pub const ZERO: Self = Self([0; 4]);
    pub const MAX: Self = Self([u64::MAX; 4]);

    pub fn from_be_bytes(bytes: [u8; 32]) -> Self {
        let limb =
            |i: usize| u64::from_be_bytes(bytes[32 - 8 * (i + 1)..32 - 8 * i].try_into().unwrap());
        Self([limb(0), limb(1), limb(2), limb(3)])
    }

    pub fn to_be_bytes(self) -> [u8; 32] {
        let mut bytes = [0; 32];
        for (i, limb) in self.0.iter().enumerate() {
            bytes[32 - 8 * (i + 1)..32 - 8 * i].copy_from_slice(&limb.to_be_bytes());
        }
        bytes
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.to_be_bytes())
    }

    /// Stratum difficulty to share target, identical to the pool's f64 formula.
    pub fn from_difficulty(difficulty: f64) -> Result<Self> {
        ensure!(
            difficulty.is_finite() && difficulty > UNSAFE_DIFFICULTY,
            "pool difficulty {difficulty} is outside the supported range"
        );
        let high = (1.0 / (difficulty / 65536.0)) * 2f64.powi(96);
        ensure!(
            high >= 1.0 && high < 2f64.powi(128),
            "pool difficulty {difficulty} has no representable target"
        );
        let high = high as u128;
        Ok(Self([u64::MAX, u64::MAX, high as u64, (high >> 64) as u64]))
    }

    /// Compact `nBits` with Bitcoin Core `SetCompact` validity rules.
    pub fn from_compact(bits: u32) -> Option<Self> {
        let exponent = (bits >> 24) as usize;
        let mantissa = bits & 0x007f_ffff;
        if bits & 0x0080_0000 != 0 && mantissa != 0 {
            return None;
        }
        if exponent > 34
            || (mantissa > 0xff && exponent > 33)
            || (mantissa > 0xffff && exponent > 32)
        {
            return None;
        }
        let mut bytes = [0u8; 32];
        if exponent <= 3 {
            let value = mantissa >> (8 * (3 - exponent));
            bytes[28..].copy_from_slice(&value.to_be_bytes());
        } else {
            // Mantissa byte k (k = 0 least significant) is worth 256^(exponent - 3 + k).
            for k in 0..3 {
                let byte = (mantissa >> (8 * k)) as u8;
                let power = exponent - 3 + k;
                if byte != 0 {
                    bytes[31 - power] = byte;
                }
            }
        }
        Some(Self::from_be_bytes(bytes))
    }

    /// True when the raw digest, read as a little-endian integer, is `<=` this target.
    #[inline]
    pub fn is_met_by(&self, digest: &[u8; 32]) -> bool {
        for i in (0..4).rev() {
            let word = u64::from_le_bytes(digest[8 * i..8 * i + 8].try_into().unwrap());
            if word != self.0[i] {
                return word < self.0[i];
            }
        }
        true
    }

    /// Approximate pool-scale difficulty (`2^240 / target`) for display.
    pub fn difficulty(self) -> f64 {
        let value = self
            .0
            .iter()
            .rev()
            .fold(0.0f64, |sum, &limb| sum * 2f64.powi(64) + limb as f64);
        if value == 0.0 {
            f64::INFINITY
        } else {
            2f64.powi(240) / value
        }
    }
}

/// Digest as a big-endian hex number, the form block explorers display.
pub fn digest_display(digest: &[u8; 32]) -> String {
    let mut reversed = *digest;
    reversed.reverse();
    hex::encode(reversed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_golden_targets() {
        // Values computed from cpuminer-opt b34565b util.c::diff_to_hash.
        for (difficulty, high) in [
            (0.001, "03e80000000000000000000000000000"),
            (0.02, "00320000000000000000000000000000"),
            (0.054931640625, "00123456789abcdf0000000000000000"),
            (0.54931640625, "0001d208a5a912e32000000000000000"),
            (1.0, "00010000000000000000000000000000"),
            (4096.0, "00000010000000000000000000000000"),
        ] {
            let target = Target::from_difficulty(difficulty).unwrap();
            assert_eq!(target.to_hex(), format!("{high}{}", "ff".repeat(16)));
        }
        assert!(Target::from_difficulty(UNSAFE_DIFFICULTY).is_err());
        assert!(Target::from_difficulty(0.0).is_err());
        assert!(Target::from_difficulty(f64::NAN).is_err());
        assert!(Target::from_difficulty(-1.0).is_err());
        assert!((Target::from_difficulty(0.02).unwrap().difficulty() / 0.02 - 1.0).abs() < 1e-12);
    }

    #[test]
    fn digest_comparison_is_little_endian_and_inclusive() {
        let target = Target::from_difficulty(1.0).unwrap();
        // Digest bytes equal to the target read little-endian: accepted (inclusive).
        let mut equal = target.to_be_bytes();
        equal.reverse();
        assert!(target.is_met_by(&equal));
        let mut above = equal;
        above[16] = 0x01; // target+2^128: raise the least significant byte of the high half
        assert!(!target.is_met_by(&above));
        let mut below = equal;
        below[29] = 0x00; // clear the 0x01 of 2^240
        assert!(target.is_met_by(&below));
        // Byte 0 is least significant: 0xff there is tiny, byte 31 decides.
        let mut trap = [0u8; 32];
        trap[0] = 0xff;
        assert!(target.is_met_by(&trap));
        trap[31] = 0x02;
        assert!(!target.is_met_by(&trap));
    }

    #[test]
    fn compact_targets_follow_core_rules() {
        let expect = |bits: u32, hex: &str| {
            assert_eq!(
                Target::from_compact(bits).unwrap().to_hex(),
                hex,
                "{bits:08x}"
            );
        };
        expect(
            0x1d00ffff,
            "00000000ffff0000000000000000000000000000000000000000000000000000",
        );
        expect(
            0x207fffff,
            "7fffff0000000000000000000000000000000000000000000000000000000000",
        );
        expect(
            0x1b0404cb,
            "00000000000404cb000000000000000000000000000000000000000000000000",
        );
        expect(0x03123456, &format!("{}123456", "0".repeat(58)));
        expect(0x02123456, &format!("{}1234", "0".repeat(60)));
        expect(0x22000001, &format!("01{}", "0".repeat(62)));
        assert_eq!(Target::from_compact(0x00000000), Some(Target::ZERO));
        assert_eq!(Target::from_compact(0x04923456), None); // negative
        assert_eq!(Target::from_compact(0x23000001), None); // overflow
        assert_eq!(Target::from_compact(0x22000100), None);
        assert_eq!(Target::from_compact(0x21010000), None);
    }

    #[test]
    fn ordering_uses_most_significant_limb_first() {
        let low = Target::from_difficulty(2.0).unwrap();
        let high = Target::from_difficulty(1.0).unwrap();
        assert!(low < high);
        assert_eq!(high.max(low), high);
        assert!(Target::MAX > high && Target::ZERO < low);
    }
}
