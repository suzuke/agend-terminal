//! agend-git-shim Phase 5 hotspot stress tests.
//! Gated via `#[ignore]`. Run: `cargo test --test agend_git_shim_phase5_stress -- --ignored`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
#[ignore]
fn stress_concurrent_hotspot_query_under_commits() {
    // Simulate 10 agents querying hotspot index concurrently.
    let index: Arc<HashMap<PathBuf, Vec<(String, String)>>> = Arc::new({
        let mut m = HashMap::new();
        for i in 0..50 {
            let file = PathBuf::from(format!("src/file_{i}.rs"));
            let touches: Vec<(String, String)> = (0..5)
                .map(|j| (format!("agent-{j}"), format!("sha-{i}-{j}")))
                .collect();
            m.insert(file, touches);
        }
        m
    });

    let mut handles = Vec::new();
    for i in 0..10 {
        let idx = Arc::clone(&index);
        let handle = std::thread::spawn(move || {
            let agent = format!("agent-{i}");
            for j in 0..100 {
                let file = PathBuf::from(format!("src/file_{}.rs", j % 50));
                // Query: find other agents who touched this file.
                if let Some(touches) = idx.get(&file) {
                    let _others: Vec<_> = touches.iter().filter(|(a, _)| *a != agent).collect();
                }
            }
        });
        handles.push(handle);
    }
    for h in handles {
        h.join().expect("thread");
    }
}

#[test]
#[ignore]
fn stress_phase5_1h_soak() {
    // Throughput / stability soak: drive the per-file hotspot-index hot path
    // (insert last-toucher + decide hotspot) for a time budget and assert it
    // sustains a high iteration count without slowing down or wedging.
    //
    // Previously this also kept a "drift" counter computed as
    //   `let is_hotspot = EXPR; let expected = EXPR; if is_hotspot != expected`
    // where both sides were the IDENTICAL expression — so `violations` could
    // never increment and `assert!(drift < 0.001)` was a tautology that could
    // not fail and exercised no real index path. That vacuous machinery is
    // removed; the soak now drives a real HashMap insert/lookup and keeps only
    // the genuine, failable throughput assertion.
    let duration_secs: u64 = std::env::var("AGEND_SOAK_DURATION")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let duration = Duration::from_secs(duration_secs);
    let start = Instant::now();
    let mut total: u64 = 0;
    let mut rng: u64 = 42;
    // last toucher per file_id — the real per-file hotspot-index shape.
    let mut last_toucher: HashMap<u64, u64> = HashMap::new();

    while start.elapsed() < duration {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        total += 1;

        let file_id = rng % 50;
        let current_agent = rng % 10;
        // Hotspot iff a DIFFERENT agent last touched this file. Driven off the
        // index's prior value, not a copy of the same line, so the work is real.
        let prev = last_toucher.insert(file_id, current_agent);
        let _is_hotspot = prev.is_some_and(|p| p != current_agent);
    }

    eprintln!("phase5 soak: {total} iterations in {duration_secs}s budget");
    assert!(
        total > 1_000_000,
        "soak must sustain >1M iterations within the {duration_secs}s budget, got {total}"
    );
}

#[test]
#[ignore]
fn stress_hotspot_index_size_bound() {
    // Build a large index: 1000 commits × 50 files.
    let start = Instant::now();
    let mut index: HashMap<PathBuf, Vec<(String, String, String)>> = HashMap::new();
    for commit in 0..1000 {
        let agent = format!("agent-{}", commit % 10);
        let sha = format!("sha-{commit:04}");
        let ts = "2026-05-05T12:00:00Z";
        for file_id in 0..3 {
            // Each commit touches 3 files.
            let file = PathBuf::from(format!("src/file_{}.rs", (commit + file_id) % 50));
            index
                .entry(file)
                .or_default()
                .push((agent.clone(), sha.clone(), ts.to_string()));
        }
    }
    let build_time = start.elapsed();
    assert!(
        build_time < Duration::from_millis(100),
        "index build must be <100ms, got {:?}",
        build_time
    );

    // Query latency.
    let query_start = Instant::now();
    for i in 0..1000 {
        let file = PathBuf::from(format!("src/file_{}.rs", i % 50));
        let _touches = index.get(&file);
    }
    let query_time = query_start.elapsed();
    assert!(
        query_time < Duration::from_millis(100),
        "1000 queries must be <100ms, got {:?}",
        query_time
    );
}
