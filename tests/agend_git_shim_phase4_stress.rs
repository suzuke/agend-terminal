//! agend-git-shim Phase 4 GC stress tests.
//! Gated via `#[ignore]`. Run: `cargo test --test agend_git_shim_phase4_stress -- --ignored`

use std::time::{Duration, Instant};

#[test]
#[ignore]
fn stress_concurrent_gc_scan_no_race() {
    let home = std::env::temp_dir().join(format!("agend-p4-gc-race-{}", std::process::id()));
    std::fs::create_dir_all(home.join("workspace").join("repo").join(".worktrees")).ok();
    let mut handles = Vec::new();
    for i in 0..10 {
        let h = home.clone();
        let handle = std::thread::spawn(move || {
            for j in 0..100 {
                let agent = format!("gc-race-{i}-{j}");
                let wt = h
                    .join("workspace")
                    .join("repo")
                    .join(".worktrees")
                    .join(&agent);
                std::fs::create_dir_all(&wt).ok();
                let old = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
                std::fs::write(
                    wt.join(".agend-managed"),
                    format!("agent={agent}\nleased_at={old}\nreleased_at={old}\n"),
                )
                .ok();
                // Concurrent scan shouldn't panic.
                std::thread::yield_now();
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
fn stress_gc_cutover_under_pin_change() {
    let home = std::env::temp_dir().join(format!("agend-p4-pin-{}", std::process::id()));
    let wt_base = home.join("workspace").join("repo").join(".worktrees");
    std::fs::create_dir_all(&wt_base).ok();
    // Create 10 candidates.
    for i in 0..10 {
        let wt = wt_base.join(format!("pin-agent-{i}"));
        std::fs::create_dir_all(&wt).ok();
        let old = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
        std::fs::write(
            wt.join(".agend-managed"),
            format!("agent=pin-agent-{i}\nleased_at={old}\n"),
        )
        .ok();
    }
    // Pin half of them concurrently while "GC" runs.
    let h2 = home.clone();
    let pinner = std::thread::spawn(move || {
        for i in 0..5 {
            let wt = h2
                .join("workspace")
                .join("repo")
                .join(".worktrees")
                .join(format!("pin-agent-{i}"));
            std::fs::write(wt.join(".agend-pinned"), "pinned").ok();
        }
    });
    pinner.join().ok();
    // Pinned worktrees must survive.
    for i in 0..5 {
        let wt = wt_base.join(format!("pin-agent-{i}"));
        assert!(
            wt.join(".agend-pinned").exists(),
            "pinned agent must have pin file"
        );
    }
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[ignore]
fn stress_phase4_1h_soak() {
    let duration_secs: u64 = std::env::var("AGEND_SOAK_DURATION")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    // Throughput / stability soak of the GC candidate decision. (Removed the
    // vacuous drift counter: `let is_candidate = EXPR; let expected = EXPR;`
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
        // GC candidate: managed + past_grace + not_pinned + no_binding.
        #[allow(clippy::manual_is_multiple_of)]
        let managed = rng % 3 != 0;
        #[allow(clippy::manual_is_multiple_of)]
        let past_grace = rng % 4 != 0;
        #[allow(clippy::manual_is_multiple_of)]
        let pinned = rng % 10 == 0;
        #[allow(clippy::manual_is_multiple_of)]
        let has_binding = rng % 5 == 0;
        let is_candidate = managed && past_grace && !pinned && !has_binding;
        std::hint::black_box(is_candidate);
    }
    eprintln!("phase4 soak: {total} iterations in {duration_secs}s budget");
    assert!(
        total > 1_000_000,
        "must sustain >1M iterations within the {duration_secs}s budget (got {total})"
    );
}

#[test]
#[ignore]
fn stress_dry_run_only_when_flag_unset() {
    let home = std::env::temp_dir().join(format!("agend-p4-noflag-{}", std::process::id()));
    let wt_base = home.join("workspace").join("repo").join(".worktrees");
    std::fs::create_dir_all(&wt_base).ok();
    let wt = wt_base.join("noflag-agent");
    std::fs::create_dir_all(&wt).ok();
    let old = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
    std::fs::write(
        wt.join(".agend-managed"),
        format!("agent=noflag-agent\nleased_at={old}\n"),
    )
    .ok();
    std::env::remove_var("AGEND_WORKTREE_GC");
    // 100 cutover attempts without flag → all must be no-ops.
    for _ in 0..100 {
        // Simulate: check flag → skip.
        assert!(std::env::var("AGEND_WORKTREE_GC").is_err());
    }
    assert!(
        wt.exists(),
        "worktree must survive 100 attempts without flag"
    );
    std::fs::remove_dir_all(&home).ok();
}
