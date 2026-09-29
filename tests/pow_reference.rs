use serde::Deserialize;
use tideminer::{
    benchmark,
    pow::{Hasher, Header},
};

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

#[test]
fn scalar_reference_parity_on_independent_threads() {
    let corpus: Corpus = serde_json::from_str(include_str!("fixtures/yespower.json")).unwrap();
    std::thread::scope(|scope| {
        for _ in 0..3 {
            scope.spawn(|| {
                let mut hasher = Hasher::new().unwrap();
                for vector in &corpus.vectors {
                    let header = Header::from_hex(&vector.header).unwrap();
                    let hash = hasher.hash(&header.0).unwrap();
                    assert_eq!(hex::encode(hash), vector.hash, "{}", vector.name);
                }
            });
        }
    });
}

#[test]
fn benchmark_workers_cover_the_same_unique_corpus() {
    let single = benchmark::run(1, 17, 0).unwrap();
    let parallel = benchmark::run(3, 17, 0).unwrap();
    assert_eq!(parallel.workers.iter().map(|w| w.hashes).sum::<u64>(), 17);
    let mut combined = [0u8; 32];
    for worker in parallel.workers {
        for (sum, byte) in combined
            .iter_mut()
            .zip(hex::decode(worker.digest_xor).unwrap())
        {
            *sum ^= byte;
        }
    }
    assert_eq!(hex::encode(combined), single.workers[0].digest_xor);
    assert!(benchmark::run(0, 1, 0).is_err());
    assert!(benchmark::run(4, 3, 0).is_err());
}
