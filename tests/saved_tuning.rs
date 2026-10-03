use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tideminer::engine::Layout;
use tideminer::topology::Topology;
use tideminer::tune::{self, LanePolicy, Limits, Recipe, Saved, SavedPick};

fn mine(home: &Path, threads: usize, extra: &[&str]) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tideminer"))
        .env("HOME", home)
        .env("XDG_CACHE_HOME", home)
        .env("LOCALAPPDATA", home)
        .env_remove("SUDO_UID")
        .env_remove("SUDO_GID")
        .args(["-o", "127.0.0.1:1", "-u", "test", "-p", "x", "-t"])
        .arg(threads.to_string())
        .args(["--time-limit", "0", "--no-color"])
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A cache miss must not accidentally launch a 15-40 second quick tune.
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("mining startup unexpectedly blocked or ran measurements");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

struct CacheDir(PathBuf);
impl Drop for CacheDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn mining_automatically_uses_only_compatible_saved_tuning() {
    let home = CacheDir(
        std::env::temp_dir().join(format!("tideminer-startup-cache-{}", std::process::id())),
    );
    let base = if cfg!(target_os = "macos") {
        home.0.join("Library/Caches")
    } else {
        home.0.clone()
    };
    let cache = base.join("tideminer/tune.json");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    let topology = Topology::detect();
    let threads = topology.cpus.len().min(2);
    let mut saved = Saved {
        fingerprint: tune::fingerprint(&topology),
        limits: Limits {
            layout: Layout::All,
            threads: Some(threads),
            cpus: None,
            lanes: None,
            gpu: None,
        },
        tuned_at: "test profile".into(),
        unix: 1,
        seconds: 60.0,
        baseline_rate: 100.0,
        speed: SavedPick {
            recipe: Recipe {
                threads: 1,
                efficiency_first: false,
                merge_smt: false,
                lanes: LanePolicy::Two,
                gpu_hashes: None,
            },
            label: "one worker, two lanes".into(),
            rate: 120.0,
            hashes_per_joule: None,
        },
        efficiency: None,
        power: None,
    };
    let write = |saved: &Saved, version| {
        std::fs::write(
            &cache,
            serde_json::to_vec(&json!({"version":version,"entries":[saved]})).unwrap(),
        )
        .unwrap()
    };

    assert!(!mine(&home.0, threads, &[]).contains("using saved"));
    write(&saved, 1);
    let output = mine(&home.0, threads, &[]);
    assert!(output.contains("using saved `tideminer tune`"), "{output}");
    assert!(
        output.contains("1 workers") && output.contains("2 hashes"),
        "{output}"
    );
    assert!(mine(&home.0, threads, &["--autotune"]).contains("using saved"));

    // A profile from another miner version must not be selected or rewritten.
    saved.fingerprint = format!(
        "{} | {} | tideminer 0.0.0",
        topology.summary(),
        tideminer::pow::KERNEL
    );
    write(&saved, 1);
    let previous = std::fs::read(&cache).unwrap();
    assert!(!mine(&home.0, threads, &[]).contains("using saved"));
    assert_eq!(std::fs::read(&cache).unwrap(), previous);
    saved.fingerprint = tune::fingerprint(&topology);
    write(&saved, 1);

    // Explicit lane constraints do not match the unrestricted profile.
    let output = mine(&home.0, threads, &["--lanes", "1"]);
    assert!(
        !output.contains("using saved") && !output.contains("2 hashes"),
        "{output}"
    );
    // Even a matching cache entry cannot override an explicit lane constraint.
    saved.limits.lanes = Some(1);
    write(&saved, 1);
    let output = mine(&home.0, threads, &["--lanes", "1"]);
    assert!(
        output.contains("ignoring invalid saved tuning") && !output.contains("2 hashes"),
        "{output}"
    );

    saved.limits.lanes = None;
    saved.speed.recipe.threads = usize::MAX;
    write(&saved, 1);
    assert!(mine(&home.0, threads, &[]).contains("ignoring invalid saved tuning"));
    saved.speed.recipe.threads = 1;
    saved.fingerprint.push_str("different machine");
    write(&saved, 1);
    assert!(!mine(&home.0, threads, &[]).contains("using saved"));
    saved.fingerprint = tune::fingerprint(&topology);
    write(&saved, 2);
    assert!(!mine(&home.0, threads, &[]).contains("using saved"));
    std::fs::write(&cache, "broken json").unwrap();
    assert!(!mine(&home.0, threads, &[]).contains("using saved"));
}
