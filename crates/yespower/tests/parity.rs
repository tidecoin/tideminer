//! Bit-exact parity: independent scalar-reference vectors, then randomized
//! differential testing against the optimized C in `rust-yespower`, across
//! backends and lane counts.
use serde::Deserialize;
use tidecoin_yespower::Hasher;
use tidecoin_yespower::simd::Portable;
// The comparisons against the C need it, and it does not build with MSVC.
#[cfg(not(windows))]
use tidecoin_yespower::{meets_target, simd::Best};

#[derive(Deserialize)]
struct Corpus {
    vectors: Vec<Vector>,
}

#[derive(Deserialize)]
struct Vector {
    name: String,
    header: String,
    hash: String,
}

fn corpus() -> Vec<([u8; 80], [u8; 32], String)> {
    let corpus: Corpus =
        serde_json::from_str(include_str!("../../../tests/fixtures/yespower.json")).unwrap();
    corpus
        .vectors
        .into_iter()
        .map(|v| {
            (
                hex::decode(&v.header).unwrap().try_into().unwrap(),
                hex::decode(&v.hash).unwrap().try_into().unwrap(),
                v.name,
            )
        })
        .collect()
}

/// xorshift64*: deterministic, dependency-free header generator.
#[cfg(not(windows))]
struct Rng(u64);
#[cfg(not(windows))]
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn header(&mut self) -> [u8; 80] {
        let mut h = [0u8; 80];
        for chunk in h.chunks_mut(8) {
            chunk.copy_from_slice(&self.next().to_le_bytes()[..chunk.len()]);
        }
        h
    }
}

#[test]
fn scalar_reference_vectors_every_width() {
    let vectors = corpus();
    let mut one = Hasher::<1>::new();
    let mut portable = Hasher::<1, Portable>::new();
    let mut three = Hasher::<3>::new();
    for (header, expected, name) in &vectors {
        assert_eq!(&one.hash(&[*header])[0], expected, "{name} (K=1)");
        assert_eq!(&portable.hash(&[*header])[0], expected, "{name} (portable)");
    }
    for group in vectors.chunks_exact(3) {
        let out = three.hash(&[group[0].0, group[1].0, group[2].0]);
        for (i, (_, expected, name)) in group.iter().enumerate() {
            assert_eq!(&out[i], expected, "{name} (K=3 lane {i})");
        }
    }
}

#[test]
#[cfg(not(windows))] // the C reference does not build with MSVC
fn random_headers_match_optimized_c() {
    let mut c = rust_yespower::TidecoinHasher::new().unwrap();
    let mut rng = Rng(0x71de_c01d_9e37_79b9);
    let mut pair = Hasher::<2, Best>::new();
    let mut quad = Hasher::<4, Best>::new();
    for round in 0..40 {
        let hs: [[u8; 80]; 4] = std::array::from_fn(|_| rng.header());
        let want: Vec<[u8; 32]> = hs.iter().map(|h| c.hash(h).unwrap()).collect();
        let got2 = pair.hash(&[hs[0], hs[1]]);
        assert_eq!(&got2[..], &want[..2], "pair round {round}");
        let got4 = quad.hash(&hs);
        assert_eq!(&got4[..], &want[..], "quad round {round}");
    }
    // Context reuse across many hashes keeps S-box state isolated per call.
    let mut single = Hasher::<1>::new();
    for _ in 0..40 {
        let h = rng.header();
        assert_eq!(single.hash(&[h])[0], c.hash(&h).unwrap());
    }
}

#[test]
#[cfg(not(windows))]
fn scan_reports_exactly_the_qualifying_nonces() {
    let mut c = rust_yespower::TidecoinHasher::new().unwrap();
    let mut header = [0x5au8; 80];
    // Easy target: about 1 in 16 hashes qualify.
    let mut target = [0xffu8; 32];
    target[0] = 0x0f;
    for (start, count) in [(0u32, 37u32), (u32::MAX - 4, 5)] {
        let mut expect = Vec::new();
        for i in 0..count {
            let nonce = start.wrapping_add(i);
            header[76..].copy_from_slice(&nonce.to_le_bytes());
            if meets_target(&c.hash(&header).unwrap(), &target) {
                expect.push(nonce);
            }
        }
        let (found1, done1) = Hasher::<1>::new().scan(&header, start, count, &target);
        let (found2, done2) = Hasher::<2>::new().scan(&header, start, count, &target);
        assert_eq!((found1, done1), (expect.clone(), count));
        assert_eq!((found2, done2), (expect, count));
    }
}
