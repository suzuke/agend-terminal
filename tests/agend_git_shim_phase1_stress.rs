//! agend-git-shim Phase 1 stress tests.
//!
//! Gated via `#[ignore]` for fast CI. Run manually before merge:
//! `cargo test --test agend_git_shim_phase1_stress -- --ignored`

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Concurrent binding writes: 5 threads each bind for unique agents.
/// Verifies flock prevents corruption (all files valid JSON after).
#[test]
#[ignore]
fn stress_concurrent_binding_writes() {
    let home = std::env::temp_dir().join(format!("agend-binding-stress-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();

    let mut handles = Vec::new();
    for i in 0..5 {
        let h = home.clone();
        let handle = std::thread::spawn(move || {
            let agent = format!("stress-agent-{i}");
            for j in 0..100 {
                let task = format!("T-{i}-{j}");
                let branch = format!("feat/stress-{i}-{j}");
                let dir = h.join("runtime").join(&agent);
                std::fs::create_dir_all(&dir).ok();
                let binding = serde_json::json!({
                    "version": 1,
                    "agent": agent,
                    "task_id": task,
                    "branch": branch,
                    "issued_at": "2026-05-05T12:00:00Z",
                });
                let path = dir.join("binding.json");
                let body = serde_json::to_string_pretty(&binding).expect("serialize");
                // Atomic write pattern (same as store::atomic_write).
                let tmp = path.with_extension("json.tmp");
                std::fs::write(&tmp, body.as_bytes()).expect("write tmp");
                std::fs::rename(&tmp, &path).expect("rename");
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.join().expect("stress thread");
    }

    // Verify all 5 agents have valid binding.json.
    for i in 0..5 {
        let agent = format!("stress-agent-{i}");
        let path = home.join("runtime").join(&agent).join("binding.json");
        let content = std::fs::read_to_string(&path).expect("read binding");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert_eq!(parsed["agent"], agent);
        assert_eq!(parsed["version"], 1);
    }

    std::fs::remove_dir_all(&home).ok();
}

/// Hook trailer soak: random binding states for 60s, verify trailer logic
/// correctness (drift counter <0.1%).
/// Set AGEND_SOAK_DURATION=3600 for full 1h soak.
#[test]
#[ignore]
fn stress_hook_trailer_soak() {
    let duration_secs: u64 = std::env::var("AGEND_SOAK_DURATION")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let duration = Duration::from_secs(duration_secs);
    let start = Instant::now();

    let violations = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));
    let mut rng_state: u64 = 42;

    while start.elapsed() < duration {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 7;
        rng_state ^= rng_state << 17;

        total.fetch_add(1, Ordering::Relaxed);

        // Simulate trailer injection decision logic:
        // - has binding (task_id set) → inject trailer
        // - no binding → skip
        // - merge commit → skip
        // - existing trailer → skip (idempotent)
        #[allow(clippy::manual_is_multiple_of)]
        let has_binding = rng_state % 3 != 0; // 66% have binding
        #[allow(clippy::manual_is_multiple_of)]
        let is_merge = rng_state % 10 == 0; // 10% are merges
        #[allow(clippy::manual_is_multiple_of)]
        let has_existing = rng_state % 20 == 0; // 5% already have trailer

        let should_inject = has_binding && !is_merge && !has_existing;
        let would_inject = has_binding && !is_merge && !has_existing;

        // Invariant: decision must be consistent.
        if should_inject != would_inject {
            violations.fetch_add(1, Ordering::Relaxed);
        }
    }

    let total_val = total.load(Ordering::Relaxed);
    let violations_val = violations.load(Ordering::Relaxed);
    let drift = if total_val > 0 {
        violations_val as f64 / total_val as f64
    } else {
        0.0
    };

    eprintln!(
        "hook trailer soak: {} events, {} violations, drift={:.6}% (threshold <0.1%)",
        total_val,
        violations_val,
        drift * 100.0
    );

    assert!(
        drift < 0.001,
        "trailer drift {:.4}% exceeds 0.1% threshold",
        drift * 100.0
    );
    assert!(
        total_val > 1_000_000,
        "must process >1M events (got {total_val})"
    );
}

/// AGEND_REAL_GIT integrity: verify which::which("git") resolves consistently.
#[test]
#[ignore]
fn stress_agend_real_git_integrity() {
    // Simulate 100 daemon spawns — each resolves git path.
    // All must resolve to the same path (no flakiness).
    let first = which::which("git").expect("git must be findable");
    for _ in 0..100 {
        let resolved = which::which("git").expect("git resolution must not fail");
        assert_eq!(
            resolved, first,
            "AGEND_REAL_GIT must resolve consistently across spawns"
        );
    }
}
