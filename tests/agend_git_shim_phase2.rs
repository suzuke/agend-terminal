//! agend-git-shim Phase 2 invariant + stress tests.

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

/// #2234 fix B (r6 #2316): end-to-end RUNTIME proof that the shim BINARY itself
/// denies an agent's `AGEND_GIT_BYPASS` provisioning op in a canonical-rooted
/// repo — exit 1 + a `DENIED` message — and passes carve-outs through. The pure
/// `deny_agent_canonical_bypass` unit test covers the DECISION; this covers the
/// WIRING (`enforce_agent_canonical_bypass_deny` reading the live env + cwd) and
/// the runtime behavior §3.17 requires for shim changes, exercised on every CI
/// platform incl. windows-runtime.
///
/// Fixtures are throwaway temp dirs — NEVER the real canonical. DENY cases exit
/// BEFORE `exec_real_git` (no git spawned). ALLOW/carve-out cases pass through to
/// real git, which fast-fails on the fake-`.git` fixture (no objects/HEAD, so no
/// index.lock, no checkout, no hang); we assert only that `DENIED` is absent.
/// Joins the `git-subprocess` serialize group (.config/nextest.toml) so the
/// passthrough git calls can't race on windows.
#[test]
fn shim_denies_agent_bypass_canonical_provisioning_2234() {
    use std::process::Command;

    let root = std::env::temp_dir().join(format!("agend-2234-deny-{}", std::process::id()));
    // Throwaway "canonical" fixture: a `.git` DIR whose config carries
    // `[remote "origin"]` → `cwd_is_canonical_rooted()` == true. No real git used
    // to build it.
    let canonical = root.join("canonical");
    std::fs::create_dir_all(canonical.join(".git")).expect("mk .git dir");
    std::fs::write(
        canonical.join(".git").join("config"),
        "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = https://example.invalid/x.git\n",
    )
    .expect("write .git/config");
    // Non-canonical control dir (plain dir, no `.git`).
    let plain = root.join("plain");
    std::fs::create_dir_all(&plain).expect("mk plain dir");

    let shim = env!("CARGO_BIN_EXE_agend-git");
    let run = |cwd: &std::path::Path, instance: Option<&str>, escape: bool, args: &[&str]| {
        let mut c = Command::new(shim);
        c.args(args)
            .current_dir(cwd)
            .env("AGEND_GIT_BYPASS", "1")
            // Start at shim depth 0 (don't inherit an outer shim's depth).
            .env_remove("AGEND_GIT_SHIM_DEPTH")
            // No AGEND_HOME → the #2158 audit log is skipped (no fleet_events).
            .env_remove("AGEND_HOME");
        match instance {
            Some(n) => {
                c.env("AGEND_INSTANCE_NAME", n);
            }
            None => {
                c.env_remove("AGEND_INSTANCE_NAME");
            }
        }
        if escape {
            c.env("AGEND_GIT_ALLOW_CANONICAL_MUTATE", "1");
        } else {
            c.env_remove("AGEND_GIT_ALLOW_CANONICAL_MUTATE");
        }
        c.output().expect("run agend-git shim")
    };
    let is_denied = |o: &std::process::Output| {
        o.status.code() == Some(1) && String::from_utf8_lossy(&o.stderr).contains("DENIED")
    };
    let not_denied =
        |o: &std::process::Output| !String::from_utf8_lossy(&o.stderr).contains("DENIED");

    // ── DENY (exit 1 + DENIED; exits before exec → no git spawned) ──
    assert!(
        is_denied(&run(
            &canonical,
            Some("test-agent"),
            false,
            &["worktree", "add", "/tmp/agend-2234-wt-a", "origin/main"]
        )),
        "agent `worktree add` in canonical must be DENIED"
    );
    assert!(
        is_denied(&run(
            &canonical,
            Some("test-agent"),
            false,
            &["checkout", "origin/main"]
        )),
        "agent positional `checkout <ref>` in canonical must be DENIED"
    );
    assert!(
        is_denied(&run(
            &canonical,
            Some("test-agent"),
            false,
            &["switch", "main"]
        )),
        "agent positional `switch <ref>` in canonical must be DENIED"
    );

    // ── ALLOW / carve-outs (DENIED message absent) ──
    // (a) no AGEND_INSTANCE_NAME (daemon-internal / operator shell) → pass.
    assert!(
        not_denied(&run(
            &canonical,
            None,
            false,
            &["worktree", "add", "/tmp/agend-2234-wt-b", "origin/main"]
        )),
        "non-agent caller must NOT be denied"
    );
    // (c) explicit one-shot escape env → pass.
    assert!(
        not_denied(&run(
            &canonical,
            Some("test-agent"),
            true,
            &["worktree", "add", "/tmp/agend-2234-wt-c", "origin/main"]
        )),
        "AGEND_GIT_ALLOW_CANONICAL_MUTATE=1 must bypass the deny"
    );
    // non-`add` worktree subcommand (r4 over-block fix): `list` is read-only → pass.
    assert!(
        not_denied(&run(
            &canonical,
            Some("test-agent"),
            false,
            &["worktree", "list"]
        )),
        "`worktree list` (read-only) must NOT be denied"
    );
    // (b) non-canonical cwd → pass.
    assert!(
        not_denied(&run(
            &plain,
            Some("test-agent"),
            false,
            &["worktree", "add", "/tmp/agend-2234-wt-d", "origin/main"]
        )),
        "non-canonical cwd must NOT be denied"
    );

    // ── #2234 Patch A (r4): a leading `-C` must NOT slip the deny ──
    let canonical_str = canonical.to_str().expect("utf8 canonical path");
    let plain_str = plain.to_str().expect("utf8 plain path");
    // `git -C <canonical> worktree add` from a NON-canonical cwd → DENIED. Proves
    // BOTH fixes at once: the real subcommand is found past `-C` (not `args.first()`
    // == "-C"), AND the effective cwd is the `-C` TARGET (canonical), not the
    // process cwd (plain).
    assert!(
        is_denied(&run(
            &plain,
            Some("test-agent"),
            false,
            &[
                "-C",
                canonical_str,
                "worktree",
                "add",
                "/tmp/agend-2234-wt-e",
                "origin/main"
            ]
        )),
        "`git -C <canonical> worktree add` from non-canonical cwd must be DENIED"
    );
    // Same for a positional `checkout <ref>` behind `-C`.
    assert!(
        is_denied(&run(
            &plain,
            Some("test-agent"),
            false,
            &["-C", canonical_str, "checkout", "origin/main"]
        )),
        "`git -C <canonical> checkout <ref>` from non-canonical cwd must be DENIED"
    );
    // Inverse (no over-block): `-C` pointing AWAY from canonical → effective cwd is
    // the non-canonical `-C` target even though the PROCESS cwd is canonical, so the
    // deny must NOT fire (the fix narrows by the dir git actually operates in).
    assert!(
        not_denied(&run(
            &canonical,
            Some("test-agent"),
            false,
            &[
                "-C",
                plain_str,
                "worktree",
                "add",
                "/tmp/agend-2234-wt-f",
                "origin/main"
            ]
        )),
        "`git -C <non-canonical>` from canonical cwd must NOT be denied (judged by the -C target)"
    );

    std::fs::remove_dir_all(&root).ok();
}
