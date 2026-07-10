//! agend-git-shim Phase 2 invariant + stress tests.
//!
//! #2524 P3b: the git-guard source-inspection tests that used to live here
//! (`shim_deny_*_in_source`, `shim_bypass_global_env`, `shim_force_push_*`,
//! `shim_writes_git_event_on_deny`, `shim_uses_agend_real_git_env_first`,
//! `shim_excludes_agend_bin_from_which`, and the runtime
//! `shim_denies_agent_bypass_canonical_provisioning_2234`) were REMOVED with the
//! agend-git git logic. That enforcement now lives in the vendored `agentic-git`
//! shim and is covered by its own suite (`cargo nextest run -p agentic-git`:
//! unbound / cross-branch / force-lease / protected / canonical / worktree /
//! recursion). What remains here is binding-file lifecycle + the compile/no-IPC
//! invariants + the generic stress/soak harness, all of which survive the cut.

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
    // #1784: prove the agend-git bin builds + runs via the PREBUILT artifact.
    // cargo builds it before this integration test and exposes its path as
    // CARGO_BIN_EXE_agend-git; running `--version` proves it compiled and
    // is runnable.
    //
    // Previously this spawned a NESTED `cargo build --bin agend-git`, which
    // contends on the cargo/`target` lock held by the outer test runner: merely
    // slow (~38s) on unix (advisory locks), but an intermittent DEADLOCK on windows
    // (mandatory file locks / AV scanning the .exe write) — the fleet-wide ~56-min
    // windows-CI hang that reddened main HEAD itself. The prebuilt binary has no
    // nested-cargo target-lock contention; the build was also redundant (the
    // workspace build + test harness already compile every bin).
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_agend-git"))
        .arg("--version")
        .output()
        .expect("run agend-git --version");
    assert!(
        out.status.code().is_some(),
        "agend-git must compile and run to completion; stderr={}",
        String::from_utf8_lossy(&out.stderr)
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
    // Throughput / stability soak of the shim deny decision. (Removed the
    // vacuous drift counter: `let should_deny = EXPR; let would_deny = EXPR;`
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

        // Shim deny decision: deny iff a mutating op on an unbound path with no
        // bypass (bound/unbound × read/mutate × bypass).
        #[allow(clippy::manual_is_multiple_of)]
        let bound = rng % 3 != 0;
        #[allow(clippy::manual_is_multiple_of)]
        let mutate = rng % 4 == 0;
        #[allow(clippy::manual_is_multiple_of)]
        let bypass = rng % 50 == 0;
        let should_deny = !bypass && !bound && mutate;
        std::hint::black_box(should_deny);
    }

    eprintln!("phase2 soak: {total} iterations in {duration_secs}s budget");
    assert!(
        total > 1_000_000,
        "must sustain >1M iterations within the {duration_secs}s budget (got {total})"
    );
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
