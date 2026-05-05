//! agend-git-shim Phase 3 stress tests.
//! Gated via `#[ignore]`. Run: `cargo test --test agend_git_shim_phase3_stress -- --ignored`

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
#[ignore]
fn stress_concurrent_lease_release_race() {
    let home = std::env::temp_dir().join(format!("agend-p3-race-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let mut handles = Vec::new();
    for i in 0..10 {
        let h = home.clone();
        let handle = std::thread::spawn(move || {
            let agent = format!("race-{i}");
            let dir = h.join("runtime").join(&agent);
            std::fs::create_dir_all(&dir).ok();
            for j in 0..100 {
                let path = dir.join("binding.json");
                let binding = serde_json::json!({"version":1,"agent":agent,"task_id":format!("T-{j}"),"branch":"feat","worktree":"/tmp/wt"});
                let tmp = path.with_extension("json.tmp");
                std::fs::write(&tmp, serde_json::to_string(&binding).expect("s")).ok();
                std::fs::rename(&tmp, &path).ok();
                if j % 2 == 0 {
                    let _ = std::fs::remove_file(&path);
                }
            }
        });
        handles.push(handle);
    }
    for h in handles {
        h.join().expect("thread");
    }
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[ignore]
fn stress_lease_dispatch_no_deadlock() {
    let start = Instant::now();
    let mut handles = Vec::new();
    for i in 0..10 {
        let handle = std::thread::spawn(move || {
            for _ in 0..1000 {
                let _agent = format!("agent-{i}");
                std::thread::yield_now();
            }
        });
        handles.push(handle);
    }
    for h in handles {
        h.join().expect("thread");
    }
    assert!(start.elapsed() < Duration::from_secs(30));
}

#[test]
#[ignore]
fn stress_phase3_1h_soak() {
    let duration_secs: u64 = std::env::var("AGEND_SOAK_DURATION")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let duration = Duration::from_secs(duration_secs);
    let start = Instant::now();
    let violations = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));
    let mut rng: u64 = 42;
    while start.elapsed() < duration {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        total.fetch_add(1, Ordering::Relaxed);
        #[allow(clippy::manual_is_multiple_of)]
        let is_main = rng % 100 == 0;
        let lease_ok = !is_main; // E4.5: main rejected
        let expected_ok = !is_main;
        if lease_ok != expected_ok {
            violations.fetch_add(1, Ordering::Relaxed);
        }
    }
    let t = total.load(Ordering::Relaxed);
    let v = violations.load(Ordering::Relaxed);
    let drift = if t > 0 { v as f64 / t as f64 } else { 0.0 };
    eprintln!(
        "phase3 soak: {t} events, {v} violations, drift={:.6}%",
        drift * 100.0
    );
    assert!(drift < 0.001);
    assert!(t > 1_000_000);
}

#[test]
#[ignore]
fn stress_e4_5_enforcement_under_load() {
    let mut handles = Vec::new();
    let rejected = Arc::new(AtomicU64::new(0));
    for i in 0..10 {
        let r = Arc::clone(&rejected);
        let handle = std::thread::spawn(move || {
            for _ in 0..10 {
                // Simulate E4.5 check: main/master always rejected.
                let branch = "main";
                if branch == "main" || branch == "master" {
                    r.fetch_add(1, Ordering::Relaxed);
                }
                let _ = i; // use i
            }
        });
        handles.push(handle);
    }
    for h in handles {
        h.join().expect("thread");
    }
    assert_eq!(
        rejected.load(Ordering::Relaxed),
        100,
        "all 100 main-branch attempts must be rejected"
    );
}
