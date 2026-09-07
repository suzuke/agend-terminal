//! Unit tests for [`super`], re-homed out of `binding.rs` so that file stays
//! under the 2500-LOC anti-monolith ceiling (`tests/src_file_size_invariant.rs`).
//! A PURE MOVE: contents are byte-identical to the former inline `mod tests`,
//! only de-indented one level. Same module path (`binding::tests`), so every
//! test name is unchanged. Mirrors the siblings this file already declares
//! (`catalog_tests`, `reaper_notify_tests`, `review_repro_agent_binding`).

use super::*;

/// §3.9 #1882 (concurrency): the per-branch lease flock SERIALIZES two
/// acquirers of the SAME branch (the second blocks until the first drops) but
/// lets DIFFERENT branches proceed concurrently (no cross-branch contention).
/// This is the atomicity that makes the dispatch `scan → bind_full` a critical
/// section so two agents can't both pass the scan and double-bind. Regression-
/// proof: it's the lock's defining behavior.
#[test]
fn branch_lease_lock_serializes_same_branch_allows_different_1882() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    let home = tmp_home("lease-lock-1882");

    let repo = "/repo/a";
    // Different branches must NOT contend — both acquire while both are held.
    let g_x = acquire_branch_lease_lock(&home, repo, "feat/x").expect("lock x");
    let g_y = acquire_branch_lease_lock(&home, repo, "feat/y").expect("lock y must not block on x");
    drop((g_x, g_y));

    // #2117 P3b: the SAME branch in a DIFFERENT repo is a DIFFERENT lease and
    // must NOT contend (the per-repo independence P3b adds).
    let g_a = acquire_branch_lease_lock(&home, "/repo/a", "feat/shared").expect("lock a");
    let g_b = acquire_branch_lease_lock(&home, "/repo/b", "feat/shared")
        .expect("same branch, different repo must not block");
    drop((g_a, g_b));

    // Same (repo, branch): a second acquirer BLOCKS until the first guard drops.
    let got = Arc::new(AtomicBool::new(false));
    let g1 = acquire_branch_lease_lock(&home, repo, "feat/z").expect("lock z (holder)");
    let home2 = home.clone();
    let got2 = got.clone();
    let t = std::thread::spawn(move || {
        // Blocks here until the holder drops g1.
        let _g2 = acquire_branch_lease_lock(&home2, "/repo/a", "feat/z").expect("lock z (waiter)");
        got2.store(true, Ordering::SeqCst);
    });

    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !got.load(Ordering::SeqCst),
        "#1882: a second same-(repo,branch) lock MUST block while the first is held"
    );
    drop(g1); // release → the waiter proceeds.
    t.join().expect("waiter thread");
    assert!(
        got.load(Ordering::SeqCst),
        "#1882: the second same-(repo,branch) lock must proceed once the first drops"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// Write a `binding.json` for `agent` directly, controlling whether the
/// `source_repo` field is present — to construct a pre-P3b "legacy" binding
/// (field absent) that `bind_full` would only write when non-empty.
fn write_binding_json(home: &Path, agent: &str, branch: &str, source_repo: Option<&str>) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    let mut v = json!({
        "version": BINDING_SCHEMA_VERSION,
        "agent": agent,
        "task_id": "T-test",
        "branch": branch,
        "issued_at": "2026-01-01T00:00:00+00:00",
    });
    if let Some(r) = source_repo {
        v["source_repo"] = json!(r);
    }
    std::fs::write(
        dir.join("binding.json"),
        serde_json::to_string_pretty(&v).unwrap(),
    )
    .unwrap();
}

/// #2117 P3b CORE GATE (reviewer-2): a pre-P3b "legacy" live binding that
/// LACKS the `source_repo` field must STILL exclude a new (repo, branch) bind
/// on the post-P3b rescan — else `None != Some(repo)` would miss it and a
/// second agent double-binds. This is the CI-runnable form of the cross-deploy
/// restart smoke: construct a field-less legacy binding fixture, then rescan as
/// a post-P3b dispatch would (with a real source_repo). The wildcard fallback
/// must match it branch-only. (True end-to-end verification — an operator
/// restart carrying a real legacy binding — is the PR's dogfood caveat.)
#[test]
fn scan_legacy_binding_missing_source_repo_still_excludes_p3b() {
    let home = tmp_home("p3b-legacy-rescan");
    write_binding_json(&home, "legacy-agent", "feat/shared", None); // no source_repo field
    assert_eq!(
        scan_existing_branch_binding(&home, "/repo/new", "feat/shared", "new-agent"),
        Some("legacy-agent".to_string()),
        "legacy binding (missing source_repo) MUST still exclude — no double-bind on P3b rollout"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2117 P3b key semantics: same (repo, branch) conflicts; same branch in a
/// DIFFERENT repo is an independent lease; an empty-source_repo query falls
/// back to branch-only (byte-identical for the pre-P3b wildcard callers).
#[test]
fn scan_p3b_lease_key_repo_scoped() {
    let home = tmp_home("p3b-key");
    write_binding_json(&home, "holder", "feat/x", Some("/repo/a"));
    assert_eq!(
        scan_existing_branch_binding(&home, "/repo/a", "feat/x", "other"),
        Some("holder".to_string()),
        "same (repo, branch) is one lease → conflict"
    );
    assert_eq!(
        scan_existing_branch_binding(&home, "/repo/b", "feat/x", "other"),
        None,
        "same branch in a different repo is a different lease → no conflict (P3b)"
    );
    assert_eq!(
        scan_existing_branch_binding(&home, "", "feat/x", "other"),
        Some("holder".to_string()),
        "empty-source_repo query falls back to branch-only (pre-P3b callers byte-identical)"
    );
    // The querying agent's own binding is always excluded.
    assert_eq!(
        scan_existing_branch_binding(&home, "/repo/a", "feat/x", "holder"),
        None,
        "self-agent binding is excluded"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Reverse of the legacy gate: when BOTH sides carry a non-empty source_repo,
/// a DIFFERENT repo must NOT match (the tightening that makes per-repo leases
/// independent). Pins that the wildcard is scoped to the missing-field case.
#[test]
fn scan_both_present_different_repo_does_not_match_p3b() {
    let home = tmp_home("p3b-both-present");
    write_binding_json(&home, "holder", "feat/x", Some("/repo/a"));
    assert_eq!(
        scan_existing_branch_binding(&home, "/repo/b", "feat/x", "other"),
        None,
        "both source_repos present + different → independent leases, no false conflict"
    );
    std::fs::remove_dir_all(&home).ok();
}

fn tmp_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-binding-test-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

// ─── #2550 W3 Wave1 pins: bound_source_repos (pre-refactor, zero prior
// coverage) — locks current field-extraction/dedup/corruption-tolerance/
// missing-field behavior before the binding_scan_all() extraction. ───────

#[test]
fn bound_source_repos_returns_distinct_source_repos_2550_w3() {
    let home = tmp_home("src-repos-distinct");
    write_binding_json(&home, "alpha", "feat/a", Some("/repo/a"));
    write_binding_json(&home, "beta", "feat/b", Some("/repo/b"));

    let mut repos = bound_source_repos(&home);
    repos.sort();
    assert_eq!(
        repos,
        vec![
            std::path::PathBuf::from("/repo/a"),
            std::path::PathBuf::from("/repo/b")
        ],
        "distinct source_repo paths across agents must all surface: {repos:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bound_source_repos_dedupes_same_source_repo_2550_w3() {
    let home = tmp_home("src-repos-dedup");
    write_binding_json(&home, "alpha", "feat/a", Some("/repo/shared"));
    write_binding_json(&home, "beta", "feat/b", Some("/repo/shared"));

    let repos = bound_source_repos(&home);
    assert_eq!(
        repos,
        vec![std::path::PathBuf::from("/repo/shared")],
        "two agents bound to the SAME source_repo must dedupe to one entry: {repos:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bound_source_repos_skips_missing_source_repo_field_2550_w3() {
    let home = tmp_home("src-repos-missing-field");
    write_binding_json(&home, "legacy", "feat/legacy", None); // no source_repo

    let repos = bound_source_repos(&home);
    assert!(
        repos.is_empty(),
        "a binding with no source_repo field must not contribute an entry: {repos:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bound_source_repos_tolerates_corrupt_binding_json_2550_w3() {
    let home = tmp_home("src-repos-corrupt");
    let corrupt_dir = crate::paths::runtime_dir(&home).join("corrupt-agent");
    std::fs::create_dir_all(&corrupt_dir).unwrap();
    std::fs::write(corrupt_dir.join("binding.json"), b"not valid json").unwrap();
    write_binding_json(&home, "good-agent", "feat/good", Some("/repo/good"));

    let repos = bound_source_repos(&home);
    assert_eq!(
        repos,
        vec![std::path::PathBuf::from("/repo/good")],
        "a corrupt sibling binding must not block finding the valid one: {repos:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2234: install_hooks must write the reference-transaction detach instrument
/// into the ACTIVE hooks dir (`$AGEND_HOME/hooks`) — a hook in `<repo>/.git/hooks`
/// is shadowed by `core.hooksPath` and never fires (Phase 1 root cause). Pins
/// that the artifact is installed (+ executable on unix) and carries the
/// fail-open safety shape, so a refactor can't silently drop it or its safety.
#[test]
fn install_hooks_writes_reference_transaction_2234() {
    let home = tmp_home("reftx-install");
    // The worktree need not be a real git repo: the hook FILES are written
    // before the `git config core.hooksPath` step (which only warns on a
    // non-repo), so the artifact lands regardless.
    install_hooks(&home, &home.join("not-a-repo"));
    let hook = home.join("hooks").join("reference-transaction");
    assert!(
        hook.exists(),
        "reference-transaction must be installed in $AGEND_HOME/hooks (active hooksPath)"
    );
    let body = std::fs::read_to_string(&hook).unwrap();
    assert!(
        body.contains("[ \"$1\" = \"committed\" ] || exit 0"),
        "fail-open gate (act only in the committed phase) must be present"
    );
    assert!(
        body.trim_end().ends_with("exit 0"),
        "hook must unconditionally exit 0 (fail-open — never abort a ref txn)"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&hook).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "hook must be executable");
    }
    std::fs::remove_dir_all(&home).ok();
}

/// #2234: the reference-transaction hook is (1) FAIL-OPEN — always exits 0, even
/// in the `prepared` phase, so it can NEVER abort a ref transaction and wedge the
/// fleet — and (2) SIGNATURE-SCOPED — logs ONLY a HEAD detach to origin/main, NOT
/// routine ref churn (FF pull / branch commits). Drives the REAL installed hook
/// with a stubbed `git` on PATH (hermetic, no real git / no host repo touched).
#[cfg(unix)]
#[test]
fn reference_transaction_hook_fail_open_and_signature_scoped_2234() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let home = tmp_home("reftx-behavior");
    install_hooks(&home, &home.join("not-a-repo"));
    let hook = home.join("hooks").join("reference-transaction");

    // Stub `git` early on PATH: origin/main = OMSHA, toplevel = /fake/canonical.
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let git_stub = bin.join("git");
    std::fs::write(
        &git_stub,
        "#!/bin/sh\n\
         case \"$*\" in\n\
           *refs/remotes/origin/main*) echo OMSHA ;;\n\
           *--show-toplevel*) echo /fake/canonical ;;\n\
           *) : ;;\n\
         esac\n\
         exit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&git_stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let log = home.join("head-detach-culprit.log");

    let run = |state: &str, stdin: &str| -> i32 {
        let mut c = Command::new("sh")
            .arg(&hook)
            .arg(state)
            .env("AGEND_HOME", &home)
            .env("PATH", &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        {
            // Best-effort write: a fail-open hook legitimately exits (and closes
            // stdin) before reading, so a BrokenPipe here is expected, not a bug.
            let mut si = c.stdin.take().unwrap();
            let _ = si.write_all(stdin.as_bytes());
        } // drop stdin → the committed-phase read loop gets EOF
        c.wait().unwrap().code().unwrap_or(-1)
    };

    // (1) FAIL-OPEN: prepared phase with garbage stdin → exit 0, no log.
    assert_eq!(
        run("prepared", "garbage not a ref line\n"),
        0,
        "prepared phase must exit 0 (a non-zero exit there ABORTS the ref txn)"
    );
    assert!(!log.exists(), "must not log outside the committed phase");

    // (2) SIGNATURE MATCH: committed + HEAD landing on origin/main (detach) → log.
    assert_eq!(
        run("committed", "OLDSHA OMSHA HEAD\n"),
        0,
        "committed phase must still exit 0"
    );
    let detach_count = || -> usize {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("HEAD detach -> origin/main"))
            .count()
    };
    assert_eq!(detach_count(), 1, "the detach signature must be logged");

    // (3) NOISE GUARD: a normal FF pull (refs/heads/main → origin/main, ref is NOT
    // HEAD) and a normal commit (HEAD → some other sha) must NOT add a log line.
    assert_eq!(run("committed", "OLDSHA OMSHA refs/heads/main\n"), 0);
    assert_eq!(run("committed", "OLDSHA OTHERSHA HEAD\n"), 0);
    assert_eq!(
        detach_count(),
        1,
        "routine ref churn (FF pull / non-origin-main HEAD move) must NOT be logged"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #1688 (codex): the daemon startup must NOT auto-bless a sidecar-less
/// binding. "No sidecar" cannot distinguish a legit unsigned binding from an
/// attacker that tampered binding.json AND deleted the sidecar — there is no
/// trusted source at startup to tell them apart, so the only safe behaviour is
/// to NOT sign (fail-closed → unbound; legit bindings re-sign on the next
/// dispatch / `bind_self`). This pins that a tampered, sidecar-less binding is
/// NOT made verifiable by the startup pass — RED while the (now-removed) blind
/// `resign_unsigned_bindings` washed it white.
#[test]
fn startup_does_not_wash_white_tampered_sidecarless_binding_1688() {
    let home = tmp_home("washwhite-1688");
    // Shared integrity key present → a sign WOULD produce a verifiable tag.
    std::fs::write(home.join(".config-integrity-key"), [9u8; 32]).unwrap();
    // Attacker blind-writes a self-authorizing branch and removes the sidecar.
    let dir = crate::paths::runtime_dir(&home).join("ag");
    std::fs::create_dir_all(&dir).unwrap();
    let forged = r#"{"version":1,"agent":"ag","task_id":"T-1","branch":"main"}"#;
    std::fs::write(dir.join("binding.json"), forged).unwrap();
    // (no binding.json.sig — the wash-white precondition)

    // Daemon startup binding handling. Post-#1688 this does NOTHING to an
    // unsigned binding (the blind re-sign is gone) — so nothing blesses it.

    // The forged content must NOT be verifiable as trusted (no valid sidecar).
    let sig = std::fs::read_to_string(dir.join("binding.json.sig")).unwrap_or_default();
    assert!(
        !crate::config_integrity::verify(&home, forged.as_bytes(), &sig),
        "#1688: a tampered, sidecar-less binding must NOT be auto-blessed at startup"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bind_creates_binding_json() {
    let home = tmp_home("bind");
    bind(&home, "agent-1", "T-123", "feature-x");
    let binding = read(&home, "agent-1").expect("binding must exist");
    assert_eq!(binding["agent"], "agent-1");
    assert_eq!(binding["task_id"], "T-123");
    assert_eq!(binding["branch"], "feature-x");
    assert_eq!(binding["version"], 1);
    assert!(binding["issued_at"].as_str().is_some());
    std::fs::remove_dir_all(&home).ok();
}

/// #1990: a binding.json a NEWER daemon wrote (version > current) reads as
/// `None` — treated as ABSENT so every consumer (git shim push gate, lease
/// checks) fail-closes, rather than acting on a shape this binary may not
/// fully understand. The missing-signature fail-closed path is unchanged.
#[test]
fn future_version_binding_reads_as_absent() {
    let home = tmp_home("future-binding");
    let dir = crate::paths::runtime_dir(&home).join("ag");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("binding.json"),
        r#"{"version":999,"agent":"ag","task_id":"T-1","branch":"main"}"#,
    )
    .unwrap();
    assert!(
        read(&home, "ag").is_none(),
        "a future-version binding.json must read as absent (fail-closed)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1990 (reviewer-2 P2b): destructive retention must distinguish a
/// future-version binding (present — a newer daemon's live worktree) from a
/// truly absent one. `read` collapses both to None; `present_including_future`
/// must report the future binding as present.
#[test]
fn present_including_future_distinguishes_future_from_absent() {
    let home = tmp_home("present-future");
    let dir = crate::paths::runtime_dir(&home).join("ag");
    std::fs::create_dir_all(&dir).unwrap();
    // Truly absent → false.
    assert!(!present_including_future(&home, "ag"));
    // Future-version binding → read() fail-closes to None, but it is PRESENT.
    std::fs::write(
        dir.join("binding.json"),
        r#"{"version":999,"agent":"ag","task_id":"T-1","branch":"main"}"#,
    )
    .unwrap();
    assert!(
        read(&home, "ag").is_none(),
        "read() fail-closes a future binding"
    );
    assert!(
        present_including_future(&home, "ag"),
        "present_including_future must report a future binding as present (future ≠ absent)"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn unbind_removes_binding_json() {
    let home = tmp_home("unbind");
    bind(&home, "agent-2", "T-456", "fix-bug");
    assert!(read(&home, "agent-2").is_some());
    unbind(&home, "agent-2");
    assert!(read(&home, "agent-2").is_none());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn read_missing_returns_none() {
    let home = tmp_home("read-miss");
    assert!(read(&home, "ghost").is_none());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn marker_check_passes_for_managed_worktree() {
    let home = tmp_home("marker-pass");
    let wt = home.join("worktrees").join("agent-1").join("feat-branch");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join(".agend-managed"), "").unwrap();
    // Write binding pointing to this worktree
    let rt = crate::paths::runtime_dir(&home).join("agent-1");
    std::fs::create_dir_all(&rt).unwrap();
    let binding = serde_json::json!({"worktree": wt.to_str().unwrap(), "branch": "feat-branch"});
    std::fs::write(rt.join("binding.json"), binding.to_string()).unwrap();

    assert!(is_agent_in_managed_worktree(&home, "agent-1"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn marker_check_fails_for_unmanaged() {
    let home = tmp_home("marker-fail");
    let wt = home.join("worktrees").join("agent-2").join("feat-branch");
    std::fs::create_dir_all(&wt).unwrap();
    // No .agend-managed marker
    let rt = crate::paths::runtime_dir(&home).join("agent-2");
    std::fs::create_dir_all(&rt).unwrap();
    let binding = serde_json::json!({"worktree": wt.to_str().unwrap(), "branch": "feat-branch"});
    std::fs::write(rt.join("binding.json"), binding.to_string()).unwrap();

    assert!(!is_agent_in_managed_worktree(&home, "agent-2"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn marker_check_fails_for_no_binding() {
    let home = tmp_home("marker-no-bind");
    assert!(!is_agent_in_managed_worktree(&home, "nobody"));
    std::fs::remove_dir_all(&home).ok();
}

/// #1163: bind_full must propagate acquire_file_lock errors.
/// Pre-fix: lock result was silently ignored, so bind_full would
/// write binding.json without holding the lock — breaking the
/// serialization guarantee under concurrent lease/bind operations.
#[test]
fn bind_full_propagates_lock_error_1163() {
    let home = tmp_home("lock-err");
    let agent = "lock-test";
    let rt = crate::paths::runtime_dir(&home).join(agent);
    std::fs::create_dir_all(&rt).unwrap();
    let lock_path = rt.join(".binding.json.lock");
    // Plant a directory where the lock file should be — open() on a
    // directory fails, so acquire_file_lock returns Err.
    std::fs::create_dir_all(&lock_path).unwrap();
    let result = bind_full(
        &home,
        agent,
        "T-999",
        "branch",
        std::path::Path::new(""),
        std::path::Path::new(""),
        false,
    );
    assert!(
        result.is_err(),
        "#1163: bind_full must fail when lock acquisition fails, got Ok"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("acquire_file_lock"),
        "error must mention lock failure: {err}"
    );
    // binding.json must NOT have been written
    assert!(
        read(&home, agent).is_none(),
        "#1163: binding.json must not be written when lock fails"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bind_wrapper_fail_closed_on_lock_error() {
    let home = tmp_home("bind-failclose");
    let agent = "fc-agent";
    let rt = crate::paths::runtime_dir(&home).join(agent);
    std::fs::create_dir_all(&rt).unwrap();
    let lock_path = rt.join(".binding.json.lock");
    std::fs::create_dir_all(&lock_path).unwrap();

    bind(&home, agent, "T-999", "branch");

    assert!(
        read(&home, agent).is_none(),
        "bind() must not write binding.json when lock acquisition fails (fail-closed)"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn index_serves_read_after_bind() {
    let home = tmp_home("idx-read");
    bind(&home, "idx-agent", "T-IDX", "idx-branch");
    let v = read(&home, "idx-agent").expect("index must serve binding");
    assert_eq!(v["branch"], "idx-branch");
    // Delete file on disk — index should still serve the cached value
    let path = crate::paths::runtime_dir(&home)
        .join("idx-agent")
        .join("binding.json");
    std::fs::remove_file(&path).unwrap();
    let v2 = read(&home, "idx-agent").expect("index must survive disk delete");
    assert_eq!(v2["task_id"], "T-IDX");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn index_invalidated_by_unbind() {
    let home = tmp_home("idx-unbind");
    bind(&home, "idx-ub", "T-UB", "ub-branch");
    assert!(read(&home, "idx-ub").is_some());
    unbind(&home, "idx-ub");
    assert!(
        read(&home, "idx-ub").is_none(),
        "unbind must clear index entry"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn no_stale_resurrection_after_concurrent_unbind() {
    for _ in 0..50 {
        let home = tmp_home("race");
        bind(&home, "race-a", "T-R", "race-b");
        if let Ok(mut map) = binding_index().write() {
            map.remove(&index_key(&home, "race-a"));
        }
        let home2 = home.clone();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let b2 = barrier.clone();
        let t = std::thread::spawn(move || {
            b2.wait();
            unbind(&home2, "race-a");
        });
        barrier.wait();
        let _ = read(&home, "race-a");
        t.join().expect("unbind thread must not panic");
        assert!(
            read(&home, "race-a").is_none(),
            "stale resurrection: read() returned binding after unbind()"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}

// ── #2158 PR2: bind_full guard-b + binding-change audit ──────────────────

fn read_event_log(home: &Path) -> String {
    std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default()
}

/// guard-b: a cross-branch rebind of a LIVE binding (worktree dir exists) with
/// no intervening release is REJECTED and does NOT mutate the binding. RED
/// pre-fix: `bind_full` unconditionally overwrote → branch silently moved.
#[test]
fn guard_b_rejects_cross_branch_rebind_of_live_binding_2158() {
    let home = tmp_home("guardb-reject");
    let wt = home.join("wt-live");
    std::fs::create_dir_all(&wt).unwrap(); // LIVE worktree on disk
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();
    bind_full(&home, "agentA", "", "feat/x", &wt, &src, false).expect("first bind ok");

    let err = bind_full(&home, "agentA", "", "feat/y", &wt, &src, false)
        .expect_err("cross-branch rebind of a live binding must be rejected");
    assert!(
        err.contains("#2158") && err.contains("release_worktree first"),
        "expected fail-closed cross-branch reject: {err}"
    );
    assert_eq!(
        read(&home, "agentA")
            .and_then(|b| b.get("branch").and_then(|v| v.as_str()).map(String::from))
            .as_deref(),
        Some("feat/x"),
        "a rejected rebind must NOT mutate the binding"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// guard-b allows the two legitimate shapes: first-bind (no existing) and
/// same-branch idempotent reuse (#2226).
#[test]
fn guard_b_allows_first_bind_and_same_branch_2158() {
    let home = tmp_home("guardb-allow");
    let wt = home.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();
    bind_full(&home, "ag", "", "feat/x", &wt, &src, false).expect("first-bind allowed");
    bind_full(&home, "ag", "", "feat/x", &wt, &src, false).expect("same-branch reuse allowed");
    std::fs::remove_dir_all(&home).ok();
}

/// guard-b gates on a LIVE worktree: if the prior worktree dir is gone (stale
/// binding), a cross-branch rebind is allowed (no live binding to protect).
#[test]
fn guard_b_allows_rebind_when_prior_worktree_is_dead_2158() {
    let home = tmp_home("guardb-dead");
    let dead = home.join("wt-gone"); // NOT created → not live
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();
    bind_full(&home, "ag", "", "feat/x", &dead, &src, false).expect("first bind ok");
    bind_full(&home, "ag", "", "feat/y", &dead, &src, false)
        .expect("rebind allowed when the prior worktree is dead (stale binding)");
    assert_eq!(
        read(&home, "ag")
            .and_then(|b| b.get("branch").and_then(|v| v.as_str()).map(String::from))
            .as_deref(),
        Some("feat/y"),
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #2496: same-agent metadata-catchup exception to guard-b ──────────────

/// A real git-repo dir standing in for a worktree (plain repo — every
/// check the #2496 exception performs, `is_git_repo`/`has_uncommitted_changes`/
/// `branch --show-current`/`switch`, works identically on a plain repo or an
/// actual `git worktree add` checkout).
fn tmp_git_repo(tag: &str) -> std::path::PathBuf {
    let dir = tmp_home(tag);
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&dir)
        .output()
        .ok();
    // Every REAL source repo gitignores `.agend-managed` (this repo's own
    // .gitignore does — see `worktree.rs::commit_marker_gitignore`'s test
    // precedent) so the lease marker never makes a clean worktree look
    // dirty. Commit it here so `write_managed_marker` doesn't trip
    // `has_uncommitted_changes` in these tests.
    std::fs::write(dir.join(".gitignore"), ".agend-managed\n").unwrap();
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["add", ".gitignore"])
        .current_dir(&dir)
        .output()
        .ok();
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@test",
            "commit",
            "-q",
            "-m",
            "init",
        ])
        .current_dir(&dir)
        .output()
        .ok();
    dir
}

fn git_switch(repo: &Path, branch: &str) {
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["switch", "-c", branch])
        .current_dir(repo)
        .output()
        .ok();
}

fn write_managed_marker(worktree: &Path, agent: &str) {
    std::fs::write(
        worktree.join(crate::worktree_pool::MANAGED_MARKER),
        format!("agent={agent}\nbranch=irrelevant\n"),
    )
    .unwrap();
}

/// #2496 (consensus test 1 + 8): a worktree that was cleanly `git
/// checkout`'d to the requested branch out-of-band (bind/release bypassed
/// entirely) — daemon-managed, owned by this agent, clean — gets its
/// STALE binding metadata caught up instead of rejected. No worktree
/// mutation happens (it was already on the right branch).
#[test]
fn guard_b_allows_metadata_catchup_when_worktree_already_on_requested_branch_2496() {
    let home = tmp_home("2496-catchup-allow");
    let wt = tmp_git_repo("2496-catchup-allow-wt");
    write_managed_marker(&wt, "agentA");
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();
    bind_full(&home, "agentA", "", "feat/x", &wt, &src, false).expect("first bind ok");

    // Out-of-band: the worktree is switched to feat/y directly (bypassing
    // bind/release) — binding.json still says feat/x.
    git_switch(&wt, "feat/y");

    bind_full(&home, "agentA", "", "feat/y", &wt, &src, false).expect(
        "same-agent metadata catchup must be allowed: worktree is already on feat/y, clean, managed",
    );
    assert_eq!(
        read(&home, "agentA")
            .and_then(|b| b.get("branch").and_then(|v| v.as_str()).map(String::from))
            .as_deref(),
        Some("feat/y"),
        "binding.json must catch up to the worktree's actual branch"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&wt).ok();
}

/// #2496 (consensus test 2): same scenario, but the worktree is DIRTY —
/// guard-b's original reject stands; metadata is untouched.
#[test]
fn guard_b_rejects_metadata_catchup_when_worktree_dirty_2496() {
    let home = tmp_home("2496-catchup-dirty");
    let wt = tmp_git_repo("2496-catchup-dirty-wt");
    write_managed_marker(&wt, "agentA");
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();
    bind_full(&home, "agentA", "", "feat/x", &wt, &src, false).expect("first bind ok");
    git_switch(&wt, "feat/y");
    std::fs::write(wt.join("dirty.txt"), "uncommitted").unwrap();

    let err = bind_full(&home, "agentA", "", "feat/y", &wt, &src, false)
        .expect_err("dirty worktree must NOT get the metadata-catchup exception");
    assert!(
        err.contains("#2158"),
        "must be the original guard-b reject: {err}"
    );
    assert_eq!(
        read(&home, "agentA")
            .and_then(|b| b.get("branch").and_then(|v| v.as_str()).map(String::from))
            .as_deref(),
        Some("feat/x"),
        "rejected catchup must not mutate the binding"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&wt).ok();
}

/// #2496 (consensus test 3): missing `.agend-managed` marker, and a marker
/// present but recording a DIFFERENT agent, both reject.
#[test]
fn guard_b_rejects_metadata_catchup_when_marker_missing_or_mismatched_2496() {
    let home = tmp_home("2496-catchup-marker");
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();

    // Sub-case: marker missing entirely.
    let wt1 = tmp_git_repo("2496-catchup-marker-missing");
    bind_full(&home, "agentA", "", "feat/x", &wt1, &src, false).expect("first bind ok");
    git_switch(&wt1, "feat/y");
    assert!(
        bind_full(&home, "agentA", "", "feat/y", &wt1, &src, false).is_err(),
        "no .agend-managed marker → not daemon-managed → reject"
    );
    std::fs::remove_dir_all(&wt1).ok();

    // Sub-case: marker present but for a DIFFERENT agent.
    let wt2 = tmp_git_repo("2496-catchup-marker-mismatch");
    write_managed_marker(&wt2, "someone-else");
    bind_full(&home, "agentB", "", "feat/x", &wt2, &src, false).expect("first bind ok");
    git_switch(&wt2, "feat/y");
    assert!(
        bind_full(&home, "agentB", "", "feat/y", &wt2, &src, false).is_err(),
        "marker agent mismatch → reject, not treated as this agent's own stale metadata"
    );
    std::fs::remove_dir_all(&wt2).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// #2496 (consensus test 4): the requested branch is already held by a
/// DIFFERENT agent — reject even though this agent's own worktree is
/// clean and genuinely on that branch (two worktrees can't both claim the
/// same (source_repo, branch)).
#[test]
fn guard_b_rejects_metadata_catchup_when_branch_held_by_another_agent_2496() {
    let home = tmp_home("2496-catchup-other-agent");
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let other_wt = home.join("other-wt");
    std::fs::create_dir_all(&other_wt).unwrap();
    bind_full(&home, "other-agent", "", "feat/y", &other_wt, &src, false)
        .expect("other agent holds feat/y");

    let wt = tmp_git_repo("2496-catchup-other-agent-wt");
    write_managed_marker(&wt, "agentA");
    bind_full(&home, "agentA", "", "feat/x", &wt, &src, false).expect("first bind ok");
    git_switch(&wt, "feat/y");

    let err = bind_full(&home, "agentA", "", "feat/y", &wt, &src, false)
        .expect_err("feat/y is held by another agent on the same source_repo — must reject");
    assert!(err.contains("#2158"), "{err}");
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&wt).ok();
}

/// #2496 (consensus test 5): the stale branch (`feat/x`, being abandoned)
/// has an active CI watch for this agent — must not silently orphan it.
#[test]
fn guard_b_rejects_metadata_catchup_when_stale_branch_has_active_ci_watch_2496() {
    let home = tmp_home("2496-catchup-ci-watch");
    let wt = tmp_git_repo("2496-catchup-ci-watch-wt");
    write_managed_marker(&wt, "agentA");
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();
    bind_full(&home, "agentA", "", "feat/x", &wt, &src, false).expect("first bind ok");
    git_switch(&wt, "feat/y");

    let ci_dir = home.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).unwrap();
    std::fs::write(
        ci_dir.join("w.json"),
        serde_json::to_string(&serde_json::json!({
            "repo": "o/r",
            "branch": "feat/x",
            "subscribers": [{"instance": "agentA"}],
        }))
        .unwrap(),
    )
    .unwrap();

    let err = bind_full(&home, "agentA", "", "feat/y", &wt, &src, false)
        .expect_err("active CI watch on the abandoned stale branch must block catchup");
    assert!(err.contains("#2158"), "{err}");
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&wt).ok();
}

/// #2496 (consensus test 6): the stale branch has an active
/// branch-linked task — must not silently orphan it.
#[test]
fn guard_b_rejects_metadata_catchup_when_stale_branch_has_active_task_2496() {
    let home = tmp_home("2496-catchup-task");
    let wt = tmp_git_repo("2496-catchup-task-wt");
    write_managed_marker(&wt, "agentA");
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();
    bind_full(&home, "agentA", "", "feat/x", &wt, &src, false).expect("first bind ok");
    git_switch(&wt, "feat/y");

    crate::tasks::handle(
        &home,
        "agentA",
        &serde_json::json!({
            "action": "create",
            "title": "work on feat/x",
            "assignee": "agentA",
            "branch": "feat/x",
        }),
    );

    let err = bind_full(&home, "agentA", "", "feat/y", &wt, &src, false)
        .expect_err("active task linked to the abandoned stale branch must block catchup");
    assert!(err.contains("#2158"), "{err}");
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&wt).ok();
}

/// (ii) binding-change audit: a bind emits `binding_changed` and an unbind emits
/// `binding_released`, both carrying caller process context (`pid=`).
#[test]
fn binding_change_and_release_are_audited_2158() {
    let home = tmp_home("audit");
    let wt = home.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();
    bind_full(&home, "ag", "", "feat/x", &wt, &src, false).expect("bind ok");
    unbind(&home, "ag");
    let log = read_event_log(&home);
    assert!(
        log.contains("binding_changed"),
        "bind must audit a binding_changed event: {log}"
    );
    assert!(
        log.contains("binding_released"),
        "unbind must audit a binding_released event: {log}"
    );
    assert!(
        log.contains("pid="),
        "audit lines must carry caller process context: {log}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #2158 GR1: agent self-claim bind → operator-visible, fire-once ──────

/// An AGENT SELF-CLAIM bind (`is_self_claim=true`) surfaces a distinct
/// `binding_out_of_dispatch` marker and DEDUPS per (agent, branch): a repeated
/// CHANGE on the SAME branch (here a new worktree path) must NOT re-surface
/// (fire-once, no flood).
#[test]
fn self_claim_bind_surfaces_once_per_branch_2158_gr1() {
    let home = tmp_home("gr1-once");
    let wt = home.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    let wt2 = home.join("wt2");
    std::fs::create_dir_all(&wt2).unwrap();
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();

    // first self-claim bind → surfaced.
    bind_full(&home, "ag", "", "feat/x", &wt, &src, true).expect("first self-claim bind");
    // SAME branch, different worktree → changed=true again, but dedup suppresses.
    bind_full(&home, "ag", "", "feat/x", &wt2, &src, true).expect("same-branch rebind allowed");

    let log = read_event_log(&home);
    assert_eq!(
        log.matches("binding_out_of_dispatch").count(),
        1,
        "self-claim bind must surface exactly once per (agent, branch): {log}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// bug-audit Rank6: the out-of-dispatch notify latch is per-bind-CYCLE, not
/// forever. `unbind` clears the `.out_of_dispatch_notified` sidecar, so after a
/// release a real re-claim of the SAME branch re-surfaces (pre-fix it was
/// swallowed as "already notified"). Intra-cycle dedup stays intact.
#[test]
fn unbind_resets_out_of_dispatch_latch_so_reclaim_resurfaces_rank6() {
    let home = tmp_home("rank6-relatch");
    let wt = home.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();

    // Cycle 1: self-claim feat/x surfaces once; a same-branch rebind in the
    // SAME cycle does NOT re-surface (intra-cycle dedup preserved).
    bind_full(&home, "ag", "", "feat/x", &wt, &src, true).expect("cycle1 bind");
    bind_full(&home, "ag", "", "feat/x", &wt, &src, true).expect("cycle1 same-branch rebind");
    assert_eq!(
        read_event_log(&home)
            .matches("binding_out_of_dispatch")
            .count(),
        1,
        "intra-cycle: a self-claim surfaces exactly once per (agent, branch)"
    );

    // Release clears the latch.
    unbind(&home, "ag");

    // Cycle 2: re-claim the SAME branch (the hijack scenario) MUST re-surface.
    bind_full(&home, "ag", "", "feat/x", &wt, &src, true).expect("cycle2 reclaim bind");
    assert_eq!(
        read_event_log(&home)
            .matches("binding_out_of_dispatch")
            .count(),
        2,
        "post-release re-claim of the same branch must re-surface (latch reset on unbind)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2158 GR1 (r2's catch): a DISPATCH bind must NOT surface even when its task_id
/// is EMPTY — a single-target `send kind=task` is auto-create-exempt and binds with
/// task_id="" (#1050 assigns the id AFTER the bind). Keying notify on
/// `is_self_claim=false` (the dispatch intent), NOT task_id, avoids the false
/// operator alert the old task_id heuristic + non-empty-task_id test masked.
#[test]
fn empty_task_id_dispatch_does_not_surface_2158_gr1() {
    let home = tmp_home("gr1-dispatch");
    let wt = home.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();
    // EMPTY task_id but a real dispatch (is_self_claim=false) — the masked case.
    bind_full(&home, "ag", "", "feat/x", &wt, &src, false).expect("dispatch bind ok");
    let log = read_event_log(&home);
    assert!(
        log.contains("binding_changed"),
        "a dispatch bind is still audited: {log}"
    );
    assert!(
        !log.contains("binding_out_of_dispatch"),
        "#2158 GR1: an EMPTY-task_id dispatch (is_self_claim=false) must NOT false-notify: {log}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #2533: task_id-carrying self-claim is in-dispatch (no warning) ───────

/// A self-claim bind (`bind_self` / `repo checkout bind:true`) that CARRIES a
/// task_id — e.g. a dev's `bind_self(task_id=...)` workaround for #2525, or a
/// reviewer's `repo checkout bind:true task_id=...` — is legitimately
/// attributed to a task and must be treated as IN-DISPATCH: no
/// `binding_out_of_dispatch` marker/notify. An EMPTY task_id self-claim
/// (`self_claim_bind_surfaces_once_per_branch_2158_gr1` above) still warns —
/// unchanged.
#[test]
fn self_claim_bind_with_task_id_does_not_surface_2533() {
    let home = tmp_home("2533-task-id-no-warn");
    let wt = home.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();

    bind_full(&home, "ag", "T-123", "feat/x", &wt, &src, true)
        .expect("self-claim bind with task_id ok");

    let log = read_event_log(&home);
    assert!(
        !log.contains("binding_out_of_dispatch"),
        "#2533: a self-claim bind CARRYING a task_id must be treated as in-dispatch — no \
         warning: {log}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2533: `notify_operator_out_of_dispatch_bind`'s body text must NOT
/// hardcode the `[system:binding_out_of_dispatch]` tag — `NotifySource::System`
/// already renders it once at the delivery-layer wrapper
/// (`format_notification_for_inject`). A hardcoded prefix doubles it:
/// `[system:binding_out_of_dispatch] [system:binding_out_of_dispatch] ...`.
#[test]
fn out_of_dispatch_notification_renders_tag_exactly_once_2533() {
    let text = out_of_dispatch_notify_body("ag", "feat/x", None);
    let rendered = crate::inbox::format_notification_for_inject(
        false,
        &crate::inbox::NotifySource::System("binding_out_of_dispatch"),
        &text,
        &[],
    );
    assert_eq!(
        rendered.matches("[system:binding_out_of_dispatch]").count(),
        1,
        "#2533: rendered notification must carry the tag exactly once (double-render bug): \
         {rendered}"
    );
}

// ── #2347: route the binding_out_of_dispatch DELIVERY to the team ────────
// orchestrator. `out_of_dispatch_notify_recipient` is the pure routing
// decision, unit-tested directly (the live delivery goes through
// `notify_agent`'s compose-aware inject, which is gated on pane/draft state
// and so is NOT observable via `inbox::drain` in a no-pane test). The
// event-log marker stays unconditional (see the #2158 GR1 tests above and
// the forensics-guard integration test below) so GR1 detection is
// unaffected. NOT a legit-vs-stolen discrimination (structurally impossible,
// GR1 premise) — only WHO receives the live notice changes.

/// #2347 ①: an operator-directed team-LEAD self-claim (lead == its own
/// team's orchestrator) resolves to NO recipient — a self-notify is pure
/// noise, so neither `general` nor the lead itself is live-notified.
#[test]
fn out_of_dispatch_notify_recipient_skips_self_for_team_lead_2347() {
    let home = tmp_home("2347-lead-skip");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "teams:\n  ops:\n    members: [lead]\n    orchestrator: lead\n",
    )
    .unwrap();
    assert_eq!(
        out_of_dispatch_notify_recipient(&home, "lead"),
        None,
        "#2347: a self-claiming team-lead (orchestrator == self) must not be live-notified"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2347 ②: a NON-lead member's self-claim routes the live notice to its
/// team's orchestrator (a DIFFERENT instance), NOT to `general`.
#[test]
fn out_of_dispatch_notify_recipient_routes_to_orchestrator_2347() {
    let home = tmp_home("2347-member-orch");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "teams:\n  ops:\n    members: [dev, lead]\n    orchestrator: lead\n",
    )
    .unwrap();
    assert_eq!(
        out_of_dispatch_notify_recipient(&home, "dev").as_deref(),
        Some("lead"),
        "#2347: a member's out-of-dispatch notice routes to its team orchestrator"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2347 ③: a TEAMLESS agent (no team lists it) falls back to the `general`
/// operator inbox — preserves operator visibility for an instance no lead owns.
#[test]
fn out_of_dispatch_notify_recipient_falls_back_to_general_when_teamless_2347() {
    let home = tmp_home("2347-teamless");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "teams:\n  ops:\n    members: [someoneelse]\n    orchestrator: someoneelse\n",
    )
    .unwrap();
    assert_eq!(
        out_of_dispatch_notify_recipient(&home, "loner").as_deref(),
        Some("general"),
        "#2347: a teamless agent's notice falls back to `general`"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2347 ④ (forensics guard): even when the live notify is SKIPPED (a
/// self-claiming lead), the unconditional `binding_out_of_dispatch`
/// event-log marker MUST still fire exactly once — the skip must never gate
/// the #2158 GR1 detection signal.
#[test]
fn self_claiming_lead_still_logs_marker_despite_skipped_notify_2347() {
    let home = tmp_home("2347-lead-marker");
    let wt = home.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    let src = home.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "teams:\n  ops:\n    members: [lead]\n    orchestrator: lead\n",
    )
    .unwrap();

    bind_full(&home, "lead", "", "feat/x", &wt, &src, true).expect("self-claim bind ok");

    let log = read_event_log(&home);
    assert_eq!(
        log.matches("binding_out_of_dispatch").count(),
        1,
        "#2347: the GR1 event-log marker must fire once even when live notify is skipped: {log}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #restart-freeze: reconcile_hooks marker-walk (correctness) ──────────
/// RED→GREEN: a SLASH-branch daemon worktree (`<home>/worktrees/<agent>/fix/x`)
/// must get its prepare-commit-msg hook wired (`core.hooksPath` set on the
/// LEAF). The old fixed 2-level descent hit the intermediate `<agent>/fix`
/// (not a git worktree) → `git config` failed there → the leaf's hook was
/// silently never installed. The marker-walk targets the real leaf.
#[test]
fn reconcile_hooks_installs_into_slash_branch_worktree_restart_freeze() {
    use std::process::Command;
    let home = tmp_home("reconcile-slash");
    let repo = tmp_home("reconcile-slash-repo");
    let git = |args: &[&str], dir: &std::path::Path| {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git")
    };
    // Source repo with one commit so `worktree add` has a base.
    assert!(git(&["init", "-b", "main"], &repo).status.success());
    std::fs::write(repo.join("README.md"), "x\n").unwrap();
    assert!(git(&["add", "README.md"], &repo).status.success());
    assert!(git(&["commit", "-m", "init"], &repo).status.success());

    // A SLASH-branch daemon worktree at <home>/worktrees/dev/fix/x.
    let wt = home.join("worktrees").join("dev").join("fix").join("x");
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    assert!(
        git(
            &["worktree", "add", "-b", "fix/x", &wt.display().to_string()],
            &repo
        )
        .status
        .success(),
        "git worktree add (slash branch) must succeed"
    );
    // Daemon-managed marker (as lease() writes it).
    std::fs::write(wt.join(crate::worktree_pool::MANAGED_MARKER), "").unwrap();

    reconcile_hooks(&home);

    // GREEN: the LEAF worktree's core.hooksPath points at <home>/hooks.
    // RED (fixed-depth): the leaf was never visited → core.hooksPath unset.
    let out = git(&["config", "--get", "core.hooksPath"], &wt);
    let cfg = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(
        cfg,
        home.join("hooks").display().to_string(),
        "slash-branch worktree must have core.hooksPath set (hook installed)"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

// ── #t-92758-7: project-scoped DCO Signed-off-by in prepare-commit-msg ──
/// Run the EMBEDDED prepare-commit-msg hook in a hermetic harness: a stub `git`
/// on PATH answers `rev-parse --show-toplevel` + `config user.{name,email}`, so
/// the project-scope gate is exercised without a real repo. `dco_present`
/// toggles `<root>/.github/workflows/dco.yml`. No `AGEND_INSTANCE_NAME` → the
/// Agend-trailer block is skipped, isolating the DCO sign-off path. Returns the
/// resulting commit message.
#[cfg(unix)]
fn run_prepare_commit_msg(
    tag: &str,
    dco_present: bool,
    name: &str,
    email: &str,
    initial: &str,
) -> String {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let home = tmp_home(tag);
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let repo_root = home.join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();

    let git_stub = bin.join("git");
    std::fs::write(
        &git_stub,
        format!(
            "#!/bin/sh\n\
             case \"$*\" in\n\
               *'rev-parse --show-toplevel'*) echo '{root}' ;;\n\
               *'config user.name'*) printf '%s' '{name}' ;;\n\
               *'config user.email'*) printf '%s' '{email}' ;;\n\
               *) : ;;\n\
             esac\n\
             exit 0\n",
            root = repo_root.display(),
            name = name,
            email = email,
        ),
    )
    .unwrap();
    std::fs::set_permissions(&git_stub, std::fs::Permissions::from_mode(0o755)).unwrap();

    if dco_present {
        let wf = repo_root.join(".github").join("workflows");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(wf.join("dco.yml"), "name: DCO\n").unwrap();
    }

    let hook = home.join("prepare-commit-msg");
    std::fs::write(&hook, include_str!("../../assets/hooks/prepare-commit-msg")).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let msg = home.join("COMMIT_EDITMSG");
    std::fs::write(&msg, initial).unwrap();

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let status = Command::new("sh")
        .arg(&hook)
        .arg(&msg)
        .arg("message")
        .env("PATH", &path)
        .env_remove("AGEND_INSTANCE_NAME")
        .status()
        .unwrap();
    assert!(status.success(), "hook must always exit 0");
    let out = std::fs::read_to_string(&msg).unwrap();
    std::fs::remove_dir_all(&home).ok();
    out
}

/// (a) A DCO repo (ships dco.yml) gets a sign-off in dco.yml's required
/// `Name <email>` shape.
#[cfg(unix)]
#[test]
fn dco_repo_gets_signoff_passing_regex_92758() {
    let out = run_prepare_commit_msg("dco-a", true, "Test User", "test@example.com", "subject\n");
    assert!(
        out.contains("Signed-off-by: Test User <test@example.com>"),
        "a DCO repo must get a sign-off: {out}"
    );
    // Mirrors dco.yml's gate: `^\s*Signed-off-by:\s+.+ <[^>]+@[^>]+>\s*$`.
    let matches_dco = out.lines().any(|l| {
        let l = l.trim();
        l.starts_with("Signed-off-by:") && l.contains(" <") && l.contains('@') && l.ends_with('>')
    });
    assert!(matches_dco, "sign-off must satisfy dco.yml's format: {out}");
}

/// (b) THE core safety property — a repo WITHOUT dco.yml is NEVER signed, so the
/// trailer cannot overflow to other projects an operator runs Agend on.
#[cfg(unix)]
#[test]
fn non_dco_repo_gets_no_signoff_92758() {
    let out = run_prepare_commit_msg("dco-b", false, "Test User", "test@example.com", "subject\n");
    assert!(
        !out.contains("Signed-off-by"),
        "a repo without .github/workflows/dco.yml must NEVER be signed: {out}"
    );
}

/// (c) Idempotent — an already-signed message is not double-signed.
#[cfg(unix)]
#[test]
fn already_signed_message_not_duplicated_92758() {
    let initial = "subject\n\nSigned-off-by: Test User <test@example.com>\n";
    let out = run_prepare_commit_msg("dco-c", true, "Test User", "test@example.com", initial);
    assert_eq!(
        out.matches("Signed-off-by:").count(),
        1,
        "must not duplicate an existing sign-off: {out}"
    );
}

/// (d) Identity gap (empty email) → SKIP rather than emit a malformed trailer
/// that would fail the DCO regex.
#[cfg(unix)]
#[test]
fn missing_identity_skips_signoff_92758() {
    let out = run_prepare_commit_msg("dco-d", true, "Test User", "", "subject\n");
    assert!(
        !out.contains("Signed-off-by"),
        "empty email must skip the sign-off (no malformed trailer): {out}"
    );
}

/// (e, r2) A MALFORMED email (no `@`) must skip the sign-off — dco.yml requires
/// `<...@...>`, so emitting it would fail the very check.
#[cfg(unix)]
#[test]
fn malformed_email_no_at_skips_signoff_92758() {
    let out = run_prepare_commit_msg("dco-noat", true, "Test User", "not-an-email", "subject\n");
    assert!(
        !out.contains("Signed-off-by"),
        "an email without '@' must skip the sign-off: {out}"
    );
}

// ── #t-92758-7: REAL-git resolution e2e (r6) — locks the no-overflow property
//    against actual `git rev-parse --show-toplevel`, not a stub. ──
#[cfg(unix)]
fn run_git(args: &[&str], dir: &std::path::Path) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env_remove("AGEND_INSTANCE_NAME")
        .output()
        .expect("git")
}

#[cfg(unix)]
fn init_repo_with_identity(repo: &std::path::Path) {
    assert!(run_git(&["init", "-b", "main"], repo).status.success());
    run_git(&["config", "user.name", "Test User"], repo);
    run_git(&["config", "user.email", "test@example.com"], repo);
}

#[cfg(unix)]
fn write_dco_yml(root: &std::path::Path) {
    let wf = root.join(".github").join("workflows");
    std::fs::create_dir_all(&wf).unwrap();
    std::fs::write(wf.join("dco.yml"), "name: DCO\n").unwrap();
}

#[cfg(unix)]
fn head_message(dir: &std::path::Path) -> String {
    let log = run_git(&["log", "-1", "--format=%B"], dir);
    String::from_utf8_lossy(&log.stdout).to_string()
}

/// (f) Real git: a commit from a SUBDIR of a DCO repo is signed (the gate
/// resolves the repo root via `git rev-parse --show-toplevel`).
#[cfg(unix)]
#[test]
fn real_git_dco_repo_subdir_commit_signs_92758() {
    let home = tmp_home("dco-e2e-subdir");
    let repo = tmp_home("dco-e2e-subdir-repo");
    init_repo_with_identity(&repo);
    write_dco_yml(&repo);
    let sub = repo.join("src");
    std::fs::create_dir_all(&sub).unwrap();
    install_hooks(&home, &repo);
    assert!(
        run_git(&["commit", "--allow-empty", "-m", "subj"], &sub)
            .status
            .success(),
        "commit from subdir must succeed"
    );
    let msg = head_message(&repo);
    assert!(
        msg.contains("Signed-off-by: Test User <test@example.com>"),
        "subdir commit in a DCO repo must be signed: {msg}"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// (g) #2234 core, real git: a commit in a LINKED WORKTREE of a DCO repo is
/// signed (the gate resolves the worktree root, where the tracked dco.yml is
/// checked out — the cwd/worktree dual-truth case).
#[cfg(unix)]
#[test]
fn real_git_dco_repo_linked_worktree_commit_signs_92758() {
    let home = tmp_home("dco-e2e-wt");
    let repo = tmp_home("dco-e2e-wt-repo");
    init_repo_with_identity(&repo);
    write_dco_yml(&repo);
    run_git(&["add", "."], &repo);
    assert!(
        run_git(&["commit", "-m", "init dco.yml"], &repo)
            .status
            .success(),
        "seed commit must succeed"
    );
    let wt = home.join("wt");
    assert!(
        run_git(
            &["worktree", "add", "-b", "feat/x", &wt.display().to_string()],
            &repo
        )
        .status
        .success(),
        "worktree add must succeed"
    );
    install_hooks(&home, &wt);
    assert!(
        run_git(&["commit", "--allow-empty", "-m", "subj"], &wt)
            .status
            .success(),
        "worktree commit must succeed"
    );
    let msg = head_message(&wt);
    assert!(
        msg.contains("Signed-off-by: Test User <test@example.com>"),
        "commit in a linked worktree of a DCO repo must be signed (#2234): {msg}"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// (h) THE no-overflow regression (operator-core, real git): a NON-DCO repo is
/// NEVER signed, even when a SIBLING directory ships a dco.yml — the gate must
/// key on the committed repo's OWN root, not a neighbour.
#[cfg(unix)]
#[test]
fn real_git_non_dco_repo_with_dco_sibling_not_signed_92758() {
    let home = tmp_home("dco-e2e-nondco");
    let base = tmp_home("dco-e2e-nondco-base");
    // Sibling that DOES opt into DCO — must not leak into the repo below.
    write_dco_yml(&base.join("sibling-dco"));
    let repo = base.join("plain-repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo_with_identity(&repo);
    install_hooks(&home, &repo);
    assert!(
        run_git(&["commit", "--allow-empty", "-m", "subj"], &repo)
            .status
            .success(),
        "commit must succeed"
    );
    let msg = head_message(&repo);
    assert!(
        !msg.contains("Signed-off-by"),
        "a non-DCO repo must NEVER be signed even with a DCO sibling (no overflow): {msg}"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&base).ok();
}

/// Lifecycle #1: after the last binding for a repo is released, the repo is
/// still discoverable via the durable managed-repo registry — closes the
/// last-binding repo-discovery hole. RED without register_managed_repo.
#[test]
fn last_binding_gone_repo_still_discovered() {
    let home = tmp_home("last-binding-discovery");
    let wt = home.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    let src = home.join("canonical-repo");
    std::fs::create_dir_all(&src).unwrap();

    bind_full(&home, "agentA", "", "feat/x", &wt, &src, false).expect("bind ok");
    // The repo is seeded into the registry on bind.
    assert!(
        all_managed_repos(&home).contains(&src),
        "repo must be discoverable while bound"
    );

    // Simulate last-binding release: remove the binding.json.
    let binding_path = crate::paths::runtime_dir(&home)
        .join("agentA")
        .join("binding.json");
    std::fs::remove_file(&binding_path).ok();

    // Live bindings now empty, but the registry keeps the repo discoverable.
    assert!(
        bound_source_repos(&home).is_empty(),
        "no live bindings after release"
    );
    assert!(
        all_managed_repos(&home).contains(&src),
        "repo must STILL be discovered after the last binding is released"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Lifecycle #2: registry dedups — binding the same repo twice records it once.
#[test]
fn managed_repo_registry_dedups() {
    let home = tmp_home("registry-dedup");
    let wt = home.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    let src = home.join("repo");
    std::fs::create_dir_all(&src).unwrap();

    bind_full(&home, "agentA", "", "feat/x", &wt, &src, false).expect("bind1");
    bind_full(&home, "agentB", "", "feat/y", &wt, &src, false).expect("bind2");

    let registry = read_managed_repo_registry(&home);
    let count = registry.iter().filter(|p| **p == src).count();
    assert_eq!(count, 1, "repo recorded exactly once in registry");
    std::fs::remove_dir_all(&home).ok();
}
