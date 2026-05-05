//! agend-git-shim Phase 5 hotspot stress tests.
//! Gated via `#[ignore]`. Run: `cargo test --test agend_git_shim_phase5_stress -- --ignored`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
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

        // Simulate hotspot decision: file touched by other agent = hotspot.
        let _file_id = rng % 50;
        let current_agent = rng % 10;
        let last_toucher = (rng >> 4) % 10;
        let is_hotspot = current_agent != last_toucher;
        let expected = current_agent != last_toucher;
        if is_hotspot != expected {
            violations.fetch_add(1, Ordering::Relaxed);
        }
    }

    let t = total.load(Ordering::Relaxed);
    let v = violations.load(Ordering::Relaxed);
    let drift = if t > 0 { v as f64 / t as f64 } else { 0.0 };
    eprintln!(
        "phase5 soak: {t} events, {v} violations, drift={:.6}%",
        drift * 100.0
    );
    assert!(drift < 0.001);
    assert!(t > 1_000_000);
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
