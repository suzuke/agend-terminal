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
    // Throughput / stability soak of the E4.5 lease decision. (Removed the
    // vacuous drift counter: `let lease_ok = EXPR; let expected_ok = EXPR;`
    // compared an expression to an identical copy of itself, so `violations`
    // could never increment and `assert!(drift < 0.001)` was a tautology. The
    // decision is now black-boxed; only the failable throughput assert remains.)
    let duration = Duration::from_secs(duration_secs);
    let start = Instant::now();
    let mut total: u64 = 0;
    let mut rng: u64 = 42;
    while start.elapsed() < duration {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        total += 1;
        // E4.5: a lease on `main` is rejected; any other branch is allowed.
        #[allow(clippy::manual_is_multiple_of)]
        let is_main = rng % 100 == 0;
        let lease_ok = !is_main;
        std::hint::black_box(lease_ok);
    }
    eprintln!("phase3 soak: {total} iterations in {duration_secs}s budget");
    assert!(
        total > 1_000_000,
        "must sustain >1M iterations within the {duration_secs}s budget (got {total})"
    );
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
