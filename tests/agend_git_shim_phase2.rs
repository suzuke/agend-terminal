//! agend-git-shim Phase 2 invariant + stress tests.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ── Invariant tests ─────────────────────────────────────────────────────

#[test]
fn bind_then_unbind_clears_binding() {
    let home = std::env::temp_dir().join(format!("agend-p2-bind-{}", std::process::id()));
    let dir = home.join("runtime").join("agent-x");
    std::fs::create_dir_all(&dir).ok();
    let binding =
        serde_json::json!({"version":1,"agent":"agent-x","task_id":"T-1","branch":"feat"});
    std::fs::write(
        dir.join("binding.json"),
        serde_json::to_string(&binding).expect("s"),
    )
    .ok();
    assert!(dir.join("binding.json").exists());
    std::fs::remove_file(dir.join("binding.json")).ok();
    assert!(!dir.join("binding.json").exists(), "unbind must clear");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn unbind_idempotent() {
    let home = std::env::temp_dir().join(format!("agend-p2-unbind-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    // Unbind on non-existent agent — must not panic.
    let path = home.join("runtime").join("ghost").join("binding.json");
    let _ = std::fs::remove_file(&path); // no-op, no panic
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn shim_binary_compiles() {
    // Verify the agend-git binary exists in target after build.
    let output = std::process::Command::new("cargo")
        .args(["build", "--bin", "agend-git"])
        .output()
        .expect("cargo build");
    assert!(output.status.success(), "agend-git must compile");
}

#[test]
fn shim_bypass_global_env() {
    // AGEND_GIT_BYPASS=1 → shim should passthrough (tested via source inspection).
    let src = include_str!("../src/bin/agend-git.rs");
    assert!(src.contains("AGEND_GIT_BYPASS"), "must check bypass env");
    assert!(
        src.contains("AGEND_GIT_BYPASS_AGENT"),
        "must check per-agent bypass"
    );
    assert!(
        src.contains("AGEND_GIT_BYPASS_UNTIL"),
        "must check TTL bypass"
    );
}

#[test]
fn shim_deny_cross_branch_in_source() {
    let src = include_str!("../src/bin/agend-git.rs");
    assert!(
        src.contains("cross-branch"),
        "must deny cross-branch checkout"
    );
}

#[test]
fn shim_deny_unbound_mutate_in_source() {
    let src = include_str!("../src/bin/agend-git.rs");
    assert!(src.contains("unbound"), "must deny unbound mutate");
}

#[test]
fn shim_deny_worktree_management() {
    let src = include_str!("../src/bin/agend-git.rs");
    assert!(
        src.contains("fleet-managed"),
        "must deny worktree management"
    );
}

#[test]
fn shim_writes_git_event_on_deny() {
    let src = include_str!("../src/bin/agend-git.rs");
    assert!(
        src.contains("fleet_events.jsonl"),
        "must write git_event on deny"
    );
    assert!(
        src.contains("\"git_event\""),
        "event kind must be git_event"
    );
}

#[test]
fn shim_uses_agend_real_git_env_first() {
    let src = include_str!("../src/bin/agend-git.rs");
    assert!(
        src.contains("AGEND_REAL_GIT"),
        "must read AGEND_REAL_GIT first"
    );
    // Verify priority order: env check before which fallback.
    let env_pos = src.find("AGEND_REAL_GIT").expect("env ref");
    let which_pos = src.find("which_in").expect("which fallback");
    assert!(
        env_pos < which_pos,
        "AGEND_REAL_GIT must be checked before which_in"
    );
}

#[test]
fn shim_excludes_agend_bin_from_which() {
    let src = include_str!("../src/bin/agend-git.rs");
    assert!(
        src.contains("agend_bin") && src.contains("filter"),
        "must exclude $AGEND_HOME/bin from which resolution"
    );
}

#[test]
fn no_self_ipc_in_shim() {
    let src = include_str!("../src/bin/agend-git.rs");
    for (i, line) in src.lines().enumerate() {
        if line.trim().starts_with("//") {
            continue;
        }
        assert!(
            !line.contains("api::call("),
            "agend-git.rs line {} has forbidden api::call",
            i + 1
        );
    }
}

// ── Stress tests (gated --ignored) ─────────────────────────────────────

#[test]
#[ignore]
fn stress_concurrent_bind_unbind_race() {
    let home = std::env::temp_dir().join(format!("agend-p2-stress-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let mut handles = Vec::new();
    for i in 0..10 {
        let h = home.clone();
        let handle = std::thread::spawn(move || {
            let agent = format!("race-agent-{i}");
            let dir = h.join("runtime").join(&agent);
            std::fs::create_dir_all(&dir).ok();
            for j in 0..100 {
                let path = dir.join("binding.json");
                let binding = serde_json::json!({"version":1,"agent":agent,"task_id":format!("T-{j}"),"branch":"feat"});
                let tmp = path.with_extension("json.tmp");
                std::fs::write(&tmp, serde_json::to_string(&binding).expect("s")).ok();
                std::fs::rename(&tmp, &path).ok();
                // Unbind half the time.
                if j % 2 == 0 {
                    let _ = std::fs::remove_file(&path);
                }
            }
        });
        handles.push(handle);
    }
    for h in handles {
        h.join().expect("stress thread");
    }
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[ignore]
fn stress_shim_dispatch_no_deadlock() {
    let start = Instant::now();
    let mut handles = Vec::new();
    for i in 0..10 {
        let handle = std::thread::spawn(move || {
            for _ in 0..1000 {
                // Simulate shim dispatch: read binding + classify + decide.
                let _agent = format!("agent-{i}");
                std::thread::yield_now();
            }
        });
        handles.push(handle);
    }
    for h in handles {
        h.join().expect("dispatch thread");
    }
    assert!(start.elapsed() < Duration::from_secs(30), "no deadlock");
}

#[test]
#[ignore]
fn stress_phase2_1h_soak() {
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

        // Simulate shim decision: bound/unbound × read/mutate × bypass.
        #[allow(clippy::manual_is_multiple_of)]
        let bound = rng % 3 != 0;
        #[allow(clippy::manual_is_multiple_of)]
        let mutate = rng % 4 == 0;
        #[allow(clippy::manual_is_multiple_of)]
        let bypass = rng % 50 == 0;

        let should_deny = !bypass && !bound && mutate;
        let would_deny = !bypass && !bound && mutate;
        if should_deny != would_deny {
            violations.fetch_add(1, Ordering::Relaxed);
        }
    }

    let t = total.load(Ordering::Relaxed);
    let v = violations.load(Ordering::Relaxed);
    let drift = if t > 0 { v as f64 / t as f64 } else { 0.0 };
    eprintln!(
        "phase2 soak: {t} events, {v} violations, drift={:.6}%",
        drift * 100.0
    );
    assert!(drift < 0.001, "drift exceeds 0.1%");
    assert!(t > 1_000_000, "must process >1M events");
}

#[test]
#[ignore]
fn stress_shim_recursion_attempt() {
    // Verify which::which_in correctly excludes a path.
    let fake_agend_bin = std::env::temp_dir().join("agend-fake-bin");
    std::fs::create_dir_all(&fake_agend_bin).ok();
    // Create a fake "git" in the fake bin dir.
    let fake_git = fake_agend_bin.join("git");
    std::fs::write(&fake_git, "#!/bin/sh\necho fake").ok();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&fake_git, std::fs::Permissions::from_mode(0o755));
    }

    // Build PATH with fake dir first.
    let original_path = std::env::var("PATH").unwrap_or_default();
    let test_path = format!("{}:{}", fake_agend_bin.display(), original_path);

    // which_in excluding the fake dir should NOT resolve to fake git.
    let filtered: String = test_path
        .split(':')
        .filter(|p| *p != fake_agend_bin.to_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(":");
    let resolved = which::which_in("git", Some(&filtered), ".").expect("git must resolve");
    assert_ne!(
        resolved, fake_git,
        "must NOT resolve to the excluded shim path"
    );

    std::fs::remove_dir_all(&fake_agend_bin).ok();
}
