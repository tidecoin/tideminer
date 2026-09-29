//! Browser / Web Worker API (feature `web`, built with wasm-pack).
//!
//! One `YespowerMiner` per Web Worker. `scan` runs the whole nonce loop inside
//! WebAssembly and returns only qualifying nonces, so JavaScript touches the
//! module once per batch, never per hash.
use crate::{HEADER_LEN, Hasher, SCRATCH_BYTES};
use wasm_bindgen::prelude::*;

/// 1 or 2 interleaved hashes. Wider lanes spill registers on x86 (measured
/// slower) and each variant adds ~50 KB to the download.
enum Lanes {
    One(Box<Hasher<1>>),
    Two(Box<Hasher<2>>),
}

#[wasm_bindgen]
pub struct YespowerMiner {
    lanes: Lanes,
}

fn header(bytes: &[u8]) -> Result<[u8; HEADER_LEN], JsError> {
    bytes
        .try_into()
        .map_err(|_| JsError::new("header must be exactly 80 bytes"))
}

fn target(bytes: &[u8]) -> Result<[u8; 32], JsError> {
    bytes
        .try_into()
        .map_err(|_| JsError::new("target must be exactly 32 bytes (big-endian)"))
}

#[wasm_bindgen]
impl YespowerMiner {
    /// `lanes` hashes are computed together per call (1 or 2); each lane costs
    /// ~2.1 MiB. 2 gives more hashes per worker when the CPU has idle cores or
    /// SMT headroom; 1 is better when every core already runs a worker.
    #[wasm_bindgen(constructor)]
    pub fn new(lanes: u32) -> Result<YespowerMiner, JsError> {
        let lanes = match lanes {
            1 => Lanes::One(Box::default()),
            2 => Lanes::Two(Box::default()),
            _ => return Err(JsError::new("lanes must be 1 or 2")),
        };
        Ok(Self { lanes })
    }

    #[wasm_bindgen(getter)]
    pub fn lanes(&self) -> u32 {
        match self.lanes {
            Lanes::One(_) => 1,
            Lanes::Two(_) => 2,
        }
    }

    /// Scratch memory held by this miner, in bytes.
    #[wasm_bindgen(getter, js_name = scratchBytes)]
    pub fn scratch_bytes(&self) -> u32 {
        self.lanes() * SCRATCH_BYTES as u32
    }

    /// Raw 32-byte digest of one 80-byte header (compare little-endian).
    pub fn hash(&mut self, header_bytes: &[u8]) -> Result<Vec<u8>, JsError> {
        let h = header(header_bytes)?;
        let digest = match &mut self.lanes {
            Lanes::One(x) => x.hash(&[h])[0],
            Lanes::Two(x) => x.hash(&[h; 2])[0],
        };
        Ok(digest.to_vec())
    }

    /// Hash nonces `start .. start + count` (header bytes 76..80, little-endian)
    /// and return those whose digest is `<=` `target` (32 bytes, big-endian).
    pub fn scan(
        &mut self,
        header_bytes: &[u8],
        start: u32,
        count: u32,
        target_be: &[u8],
    ) -> Result<Vec<u32>, JsError> {
        let (h, t) = (header(header_bytes)?, target(target_be)?);
        let found = match &mut self.lanes {
            Lanes::One(x) => x.scan(&h, start, count, &t).0,
            Lanes::Two(x) => x.scan(&h, start, count, &t).0,
        };
        Ok(found)
    }
}

/// cpuminer-compatible share target (32 bytes, big-endian) for a Stratum difficulty.
#[wasm_bindgen(js_name = shareTarget)]
pub fn share_target(difficulty: f64) -> Option<Vec<u8>> {
    crate::share_target(difficulty).map(|t| t.to_vec())
}

/// True when this build uses WebAssembly SIMD128.
#[wasm_bindgen(js_name = simdEnabled)]
pub fn simd_enabled() -> bool {
    cfg!(target_feature = "simd128")
}
