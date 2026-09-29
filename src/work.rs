//! Stratum jobs to exact 80-byte Tidecoin headers.
//!
//! Header = LE32(version) || prevhash (each Stratum 4-byte word reversed) ||
//! merkle root (raw sha256d chain) || LE32(ntime) || LE32(nbits) || LE32(nonce).
use crate::target::Target;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const MAX_BRANCHES: usize = 32;
pub const MAX_COINBASE_PART: usize = 16 * 1024;
pub const MAX_JOB_ID: usize = 256;

pub fn sha256d(data: &[u8]) -> [u8; 32] {
    Sha256::digest(Sha256::digest(data)).into()
}

/// An immutable `mining.notify`, bound to the difficulty announced before it.
#[derive(Debug, Clone)]
pub struct Job {
    /// Opaque pool job identifier. Never parsed as a number.
    pub id: String,
    /// Header byte order (Stratum words already reversed).
    pub prevhash: [u8; 32],
    pub coinbase1: Vec<u8>,
    pub coinbase2: Vec<u8>,
    pub branches: Vec<[u8; 32]>,
    pub version: u32,
    pub nbits: u32,
    pub ntime: u32,
    pub clean: bool,
    pub difficulty: f64,
    pub share_target: Target,
    /// `None` when nBits is not a valid compact target (then only shares are possible).
    pub network_target: Option<Target>,
}

fn hex_bytes(value: &Value, what: &str, max: usize) -> Result<Vec<u8>> {
    let text = value
        .as_str()
        .with_context(|| format!("{what} must be a string"))?;
    ensure!(text.len() <= max * 2, "{what} too long");
    hex::decode(text).with_context(|| format!("{what} is not hex"))
}

fn hex32(value: &Value, what: &str) -> Result<[u8; 32]> {
    hex_bytes(value, what, 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("{what} must be 32 bytes"))
}

fn hex_u32(value: &Value, what: &str) -> Result<u32> {
    let text = value
        .as_str()
        .with_context(|| format!("{what} must be a string"))?;
    ensure!(text.len() == 8, "{what} must be 8 hex characters");
    u32::from_str_radix(text, 16).with_context(|| format!("{what} is not hex"))
}

impl Job {
    /// Parse the nine `mining.notify` params with the difficulty in effect for it.
    pub fn from_notify(params: &Value, difficulty: f64) -> Result<Self> {
        let fields = params
            .as_array()
            .context("notify params must be an array")?;
        ensure!(fields.len() >= 9, "mining.notify needs nine params");
        let id = fields[0].as_str().context("job id must be a string")?;
        ensure!(
            !id.is_empty() && id.len() <= MAX_JOB_ID,
            "job id length out of range"
        );
        let stratum_prev = hex32(&fields[1], "prevhash")?;
        let mut prevhash = [0u8; 32];
        for (dst, src) in prevhash
            .chunks_exact_mut(4)
            .zip(stratum_prev.chunks_exact(4))
        {
            dst.copy_from_slice(&[src[3], src[2], src[1], src[0]]);
        }
        let branches = fields[4]
            .as_array()
            .context("merkle branches must be an array")?;
        ensure!(branches.len() <= MAX_BRANCHES, "too many merkle branches");
        let branches = branches
            .iter()
            .map(|b| hex32(b, "merkle branch"))
            .collect::<Result<Vec<_>>>()?;
        let nbits = hex_u32(&fields[6], "nbits")?;
        Ok(Self {
            id: id.to_owned(),
            prevhash,
            coinbase1: hex_bytes(&fields[2], "coinbase1", MAX_COINBASE_PART)?,
            coinbase2: hex_bytes(&fields[3], "coinbase2", MAX_COINBASE_PART)?,
            branches,
            version: hex_u32(&fields[5], "version")?,
            nbits,
            ntime: hex_u32(&fields[7], "ntime")?,
            clean: fields[8]
                .as_bool()
                .context("clean_jobs must be a boolean")?,
            difficulty,
            share_target: Target::from_difficulty(difficulty)?,
            network_target: Target::from_compact(nbits),
        })
    }

    /// The loosest target a submitted hash must meet: the pool accepts
    /// `hash <= max(share_target, network_target)`.
    pub fn submit_target(&self) -> Target {
        self.network_target
            .map_or(self.share_target, |n| n.max(self.share_target))
    }

    /// Everything that determines header bytes except extranonces and nonce.
    /// Jobs with equal keys (e.g. a pure retarget) share one search space.
    pub fn template_key(&self, extranonce1: &[u8]) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"tideminer-template-v1");
        h.update(self.prevhash);
        for part in [&self.coinbase1, &self.coinbase2] {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part);
        }
        h.update((self.branches.len() as u64).to_le_bytes());
        for branch in &self.branches {
            h.update(branch);
        }
        h.update(self.version.to_le_bytes());
        h.update(self.nbits.to_le_bytes());
        h.update(self.ntime.to_le_bytes());
        h.update((extranonce1.len() as u64).to_le_bytes());
        h.update(extranonce1);
        h.finalize().into()
    }

    pub fn merkle_root(&self, extranonce1: &[u8], extranonce2: &[u8]) -> [u8; 32] {
        let mut coinbase = Vec::with_capacity(
            self.coinbase1.len() + extranonce1.len() + extranonce2.len() + self.coinbase2.len(),
        );
        coinbase.extend_from_slice(&self.coinbase1);
        coinbase.extend_from_slice(extranonce1);
        coinbase.extend_from_slice(extranonce2);
        coinbase.extend_from_slice(&self.coinbase2);
        let mut root = sha256d(&coinbase);
        let mut pair = [0u8; 64];
        for branch in &self.branches {
            pair[..32].copy_from_slice(&root);
            pair[32..].copy_from_slice(branch);
            root = sha256d(&pair);
        }
        root
    }

    /// Header with nonce 0; workers overwrite bytes 76..80.
    pub fn header(&self, extranonce1: &[u8], extranonce2: &[u8]) -> [u8; 80] {
        let mut header = [0u8; 80];
        header[0..4].copy_from_slice(&self.version.to_le_bytes());
        header[4..36].copy_from_slice(&self.prevhash);
        header[36..68].copy_from_slice(&self.merkle_root(extranonce1, extranonce2));
        header[68..72].copy_from_slice(&self.ntime.to_le_bytes());
        header[72..76].copy_from_slice(&self.nbits.to_le_bytes());
        header
    }
}

/// Extranonce2 bytes for a search-space index: big-endian, fixed width.
pub fn extranonce2_bytes(index: u64, len: usize) -> Vec<u8> {
    let full = index.to_be_bytes();
    if len >= 8 {
        let mut out = vec![0u8; len - 8];
        out.extend_from_slice(&full);
        out
    } else {
        full[8 - len..].to_vec()
    }
}

/// Number of distinct extranonce2 values, capped to what a u64 counter can index.
pub fn extranonce2_space(len: usize) -> u64 {
    if len >= 4 { 1 << 31 } else { 1u64 << (8 * len) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Fixed Stratum notification fixture with a known reconstructed header.
    #[test]
    fn pool_golden_header() {
        let params = json!([
            "4242424242424242424242424242424242424242424242424242424242424242",
            "00112233445566778899aabbccddeeff102132435465768798a9bacbdcedfe0f",
            "010203",
            "040506",
            ["ffeeddccbbaa998877665544332211000123456789abcdef1020304050607080"],
            "20000001",
            "1d00ffff",
            "6553f123",
            true
        ]);
        let job = Job::from_notify(&params, 0.02).unwrap();
        let ex1 = hex::decode("aabbccdd").unwrap();
        let ex2 = hex::decode("11223344").unwrap();
        assert_eq!(
            hex::encode(sha256d(
                &hex::decode("010203aabbccdd11223344040506").unwrap()
            )),
            "deee5ae0f6686cd8bb3b284f1ee0857c9af2fff659d986c7ea42ea99d2bab474"
        );
        assert_eq!(
            hex::encode(job.merkle_root(&ex1, &ex2)),
            "1d557b3fff1ed2133efa3ff9f2175876847f9ef932977756b064418bcea4ec70"
        );
        let mut header = job.header(&ex1, &ex2);
        header[76..].copy_from_slice(&0x89abcdefu32.to_le_bytes());
        assert_eq!(
            hex::encode(header),
            "010000203322110077665544bbaa9988ffeeddcc4332211087766554cbbaa9980ffeeddc\
             1d557b3fff1ed2133efa3ff9f2175876847f9ef932977756b064418bcea4ec70\
             23f15365ffff001defcdab89"
        );
        assert!(job.clean);
        assert_eq!(job.network_target, Target::from_compact(0x1d00ffff));
    }

    #[test]
    fn retarget_keeps_template_key_but_ntime_changes_it() {
        let notify = |ntime: &str| {
            json!([
                "j",
                "00".repeat(32),
                "01",
                "02",
                [],
                "20000000",
                "207fffff",
                ntime,
                false
            ])
        };
        let a = Job::from_notify(&notify("6553f123"), 0.02).unwrap();
        let b = Job::from_notify(&notify("6553f123"), 0.5).unwrap();
        let c = Job::from_notify(&notify("6553f124"), 0.5).unwrap();
        assert_eq!(a.template_key(b"ex"), b.template_key(b"ex"));
        assert_ne!(a.template_key(b"ex"), c.template_key(b"ex"));
        assert_ne!(a.template_key(b"ex"), a.template_key(b"ey"));
        // Regtest network target is looser than the share target: submit uses it.
        assert_eq!(a.submit_target(), a.network_target.unwrap());
    }

    #[test]
    fn malformed_notifications_are_rejected() {
        let good = json!([
            "j",
            "00".repeat(32),
            "01",
            "02",
            [],
            "20000000",
            "1d00ffff",
            "6553f123",
            true
        ]);
        assert!(Job::from_notify(&good, 1.0).is_ok());
        for (index, bad) in [
            (1, json!("00")),
            (4, json!(["00"])),
            (5, json!("2000000")),
            (7, json!(12)),
            (8, json!(1)),
            (0, json!("")),
        ] {
            let mut params = good.clone();
            params[index] = bad;
            assert!(Job::from_notify(&params, 1.0).is_err(), "field {index}");
        }
        assert!(Job::from_notify(&good, 0.0).is_err());
        assert!(Job::from_notify(&json!(["j"]), 1.0).is_err());
    }

    #[test]
    fn extranonce2_encoding() {
        assert_eq!(extranonce2_bytes(0x0102, 4), vec![0, 0, 1, 2]);
        assert_eq!(extranonce2_bytes(0x0102, 2), vec![1, 2]);
        assert_eq!(extranonce2_bytes(1, 10), vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(extranonce2_space(1), 256);
        assert_eq!(extranonce2_space(4), 1 << 31);
    }
}
