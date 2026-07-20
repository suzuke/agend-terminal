use super::*;

#[test]
fn release_repo_rejects_root_path() {
    let result = handle_release_repo(
        std::path::Path::new("/tmp"),
        &serde_json::json!({"path": "/"}),
        "",
    );
    assert!(result["error"].as_str().is_some(), "root must be rejected");
}

#[test]
fn release_repo_rejects_system_path() {
    let result = super::validate_release_path("/etc");
    assert!(result.is_err(), "/etc must be rejected: {:?}", result);
}

#[test]
fn release_repo_rejects_empty_path() {
    let result = handle_release_repo(
        std::path::Path::new("/tmp"),
        &serde_json::json!({"path": ""}),
        "",
    );
    assert!(result["error"].as_str().is_some(), "empty must be rejected");
}

#[test]
fn validate_release_path_rejects_relative_dotdot() {
    let result = super::validate_release_path("../../etc");
    // Either fails canonicalize (doesn't exist) or rejects as system path.
    assert!(result.is_err(), "relative dotdot must be rejected");
}

#[test]
fn validate_release_path_rejects_relative_no_root() {
    let result = super::validate_release_path("a/b/c");
    // Relative path that doesn't exist → canonicalize fails.
    assert!(result.is_err(), "relative path must be rejected");
}

#[test]
#[cfg(unix)]
fn validate_release_path_rejects_shallow() {
    // /tmp canonicalizes to /private/tmp on macOS → system prefix match.
    let result = super::validate_release_path("/tmp");
    assert!(result.is_err(), "/tmp must be rejected: {:?}", result);
}

#[test]
#[cfg(unix)]
fn validate_release_path_refuses_non_worktree_deep_dir_83936() {
    // #t-…83936-6: SEMANTIC TIGHTENING (was `..._accepts_deep_existing`, which
    // asserted a plain deep dir is ACCEPTED — that permissive accept is exactly
    // what let a canonical repo through to the `remove_dir_all` fallback). Under
    // the whitelist, validate now accepts ONLY a real linked worktree; a deep
    // non-worktree dir is refused. The positive control (a legit linked worktree
    // IS accepted) is `handle_release_repo_still_removes_linked_worktree_83936`.
    let home = std::env::var("HOME").expect("HOME must be set");
    let dir =
        std::path::PathBuf::from(home).join(format!(".agend-release-test-{}", std::process::id()));
    let deep = dir.join("sub");
    std::fs::create_dir_all(&deep).ok();
    let result = super::validate_release_path(deep.to_str().expect("valid UTF-8"));
    assert!(
        result.is_err(),
        "a non-worktree deep dir must now be REFUSED (whitelist), got {result:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── #t-…83936-6 P0: NEVER release a primary/main working tree ───────────────
// The 2026-07-06 canonical-deletion incident: `repo release path=<canonical>`
// → validate passed → `git worktree remove` refused the main tree → the
// `remove_dir_all` fallback deleted the ENTIRE repo. Fixtures live under $HOME
// (a plain path like the real incident) NOT temp_dir — `/var`→`/private` is
// already system-prefix-rejected and would MASK the guard under test.

#[cfg(unix)]
fn release_guard_tmp(tag: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    let d = std::path::PathBuf::from(home).join(format!(
        ".agend-release-guard-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&d).ok();
    d
}

#[cfg(unix)]
fn release_guard_git_init(dir: &Path) {
    std::fs::create_dir_all(dir).ok();
    for args in [
        &["init", "-b", "main"][..],
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ][..],
    ] {
        let _ = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output();
    }
}

#[test]
#[cfg(unix)]
fn validate_release_path_refuses_primary_working_tree_83936() {
    let base = release_guard_tmp("validate-main");
    let repo = base.join("source-repo");
    release_guard_git_init(&repo);
    assert!(
        repo.join(".git").is_dir(),
        "a main repo's .git must be a dir"
    );
    let result = super::validate_release_path(repo.to_str().unwrap());
    assert!(
        result
            .as_ref()
            .is_err_and(|e| e.contains("linked worktree")),
        "a primary/main working tree must be refused, got {result:?}"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// reviewer5's bypass (#2668 r0): a BARE repo has NO `.git` child (it IS a git
/// dir), so the earlier `.git.is_dir()` blacklist let it through and the fallback
/// `remove_dir_all`'d it. The whitelist (`.git` must be a gitlink FILE) refuses
/// it. RED against the blacklist code, GREEN after.
#[test]
#[cfg(unix)]
fn handle_release_repo_never_deletes_bare_repo_83936() {
    let base = release_guard_tmp("no-delete-bare");
    let bare = base.join("bare-repo.git");
    std::fs::create_dir_all(&bare).ok();
    let _ = std::process::Command::new("git")
        .args(["init", "--bare"])
        .current_dir(&bare)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    assert!(bare.join("HEAD").exists(), "a bare repo must have HEAD");
    assert!(!bare.join(".git").exists(), "a bare repo has NO .git child");

    let _ = handle_release_repo(
        std::path::Path::new("/tmp"),
        &serde_json::json!({"path": bare.to_str().unwrap()}),
        "",
    );

    assert!(
        bare.exists() && bare.join("HEAD").exists(),
        "a bare source repo MUST survive a release call (reviewer5 bare-repo bypass)"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// reviewer4's bypass (#2668 r0'): a `git init --separate-git-dir` MAIN tree has
/// a gitlink `.git` FILE, so a filesystem `.git`-is-file whitelist would wrongly
/// DELETE it. The git-based check (a main tree's git-dir == common-dir) refuses
/// it. RED against the filesystem whitelist, GREEN with the git check.
#[test]
#[cfg(unix)]
fn handle_release_repo_never_deletes_separate_git_dir_main_83936() {
    let base = release_guard_tmp("no-delete-sepgit");
    let worktree = base.join("main-worktree");
    let gitdir = base.join("main-gitdir");
    let _ = std::process::Command::new("git")
        .args([
            "init",
            "--separate-git-dir",
            gitdir.to_str().unwrap(),
            worktree.to_str().unwrap(),
        ])
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    assert!(
        worktree.join(".git").is_file(),
        "a --separate-git-dir main's .git must be a gitlink FILE"
    );
    let keep = worktree.join("KEEP.txt");
    std::fs::write(&keep, "main content").unwrap();

    let _ = handle_release_repo(
        std::path::Path::new("/tmp"),
        &serde_json::json!({"path": worktree.to_str().unwrap()}),
        "",
    );

    assert!(
        worktree.exists() && keep.exists(),
        "a --separate-git-dir MAIN tree MUST survive (reviewer4 gitlink-file bypass)"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// Fail-safe (⑤): a non-repo dir git cannot classify must be REFUSED, not
/// deleted (prefer a false reject over data loss).
#[test]
#[cfg(unix)]
fn handle_release_repo_refuses_non_repo_dir_83936() {
    let base = release_guard_tmp("non-repo");
    let dir = base.join("just-a-dir");
    std::fs::create_dir_all(&dir).ok();
    std::fs::write(dir.join("file.txt"), "data").unwrap();

    let _ = handle_release_repo(
        std::path::Path::new("/tmp"),
        &serde_json::json!({"path": dir.to_str().unwrap()}),
        "",
    );

    assert!(
        dir.exists(),
        "a non-repo dir must be refused (git can't classify → fail-safe)"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// Incident repro (RED against pre-fix code, which `remove_dir_all`s the repo):
/// a `repo release` pointed at a main repo must leave the repo fully intact.
#[test]
#[cfg(unix)]
fn handle_release_repo_never_deletes_main_repo_83936() {
    let base = release_guard_tmp("no-delete-main");
    let repo = base.join("source-repo");
    release_guard_git_init(&repo);
    let keep = repo.join("KEEP.txt");
    std::fs::write(&keep, "canonical content").unwrap();

    let _ = handle_release_repo(
        std::path::Path::new("/tmp"),
        &serde_json::json!({"path": repo.to_str().unwrap()}),
        "",
    );

    assert!(
        repo.exists(),
        "the main repo dir MUST survive a release call (canonical incident)"
    );
    assert!(repo.join(".git").is_dir(), ".git MUST survive");
    assert!(keep.exists(), "repo contents MUST survive");
    std::fs::remove_dir_all(&base).ok();
}

/// Counter-example: a genuine LINKED worktree (`.git` is a gitlink file) is
/// still releasable — the guard must not over-block legitimate releases.
#[test]
#[cfg(unix)]
fn handle_release_repo_still_removes_linked_worktree_83936() {
    let base = release_guard_tmp("linked-ok");
    let repo = base.join("source-repo");
    release_guard_git_init(&repo);
    let _ = std::process::Command::new("git")
        .args(["branch", "feat", "main"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    let wt = base.join("linked-wt");
    let add = std::process::Command::new("git")
        .args(["worktree", "add", wt.to_str().unwrap(), "feat"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    assert!(
        wt.join(".git").is_file(),
        "a linked worktree's .git must be a gitlink FILE; add stderr: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    assert!(
        super::validate_release_path(wt.to_str().unwrap()).is_ok(),
        "a linked worktree must validate OK"
    );
    let _ = handle_release_repo(
        std::path::Path::new("/tmp"),
        &serde_json::json!({"path": wt.to_str().unwrap()}),
        "",
    );
    assert!(
        !wt.exists(),
        "a linked worktree must still be releasable/removed"
    );
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn dispatch_with_branch_and_repo_auto_invokes_watch_ci() {
    let home = std::env::temp_dir().join(format!("agend-auto-watch-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repository": "owner/repo", "branch": "feat/test"});
    handle_watch_ci(&home, &args, "test-agent");
    let filename = crate::daemon::ci_watch::watch_filename("owner/repo", "feat/test");
    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(&filename);
    assert!(watch_path.exists(), "watch file must be created");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn dispatch_idempotent_double_watch_safe() {
    let home = std::env::temp_dir().join(format!("agend-auto-watch-idem-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repository": "owner/repo", "branch": "feat/idem"});
    handle_watch_ci(&home, &args, "agent-1");
    handle_watch_ci(&home, &args, "agent-1"); // second call — idempotent
    let filename = crate::daemon::ci_watch::watch_filename("owner/repo", "feat/idem");
    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(&filename);
    assert!(watch_path.exists());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn dispatch_without_repo_no_auto_watch() {
    // If no repo field, auto-watch should not fire.
    // This tests the comms.rs logic: args["repo"].as_str() returns None.
    let home = std::env::temp_dir().join(format!("agend-no-watch-{}", std::process::id()));
    std::fs::create_dir_all(crate::daemon::ci_watch::ci_watches_dir(&home)).ok();
    // No watch file should exist for a branch without repo.
    let filename = crate::daemon::ci_watch::watch_filename("", "feat/no-repo");
    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(&filename);
    assert!(!watch_path.exists(), "no watch without repo");
    std::fs::remove_dir_all(&home).ok();
}

// -----------------------------------------------------------------
// Sprint 54 P0-1 — multi-subscriber contract invariants. Each test
// pins one of the six hard-contract guarantees from the lead's
// dispatch (see m-20260507000244357650-11). The fan-out test in
// src/daemon/ci_watch.rs (`subscriber_fan_out_notifies_every_member`)
// is the empirical regression-proof anchor; these are the watch-file
// schema invariants that proof relies on.
// -----------------------------------------------------------------

fn watch_path_for(home: &Path, repo: &str, branch: &str) -> std::path::PathBuf {
    let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
    crate::daemon::ci_watch::ci_watches_dir(home).join(filename)
}

fn read_watch(path: &Path) -> serde_json::Value {
    let s = std::fs::read_to_string(path).expect("watch file must exist");
    serde_json::from_str(&s).expect("watch must be valid JSON")
}

#[test]
fn ci_watch_appends_subscriber_idempotent_distinct_callers() {
    // Hard contract item 4: `ci watch` MCP action APPENDS to subscribers
    // if not present (idempotent), does NOT overwrite. Last-write-wins
    // was the Sprint 53 multi-caller bug.
    let home = std::env::temp_dir().join(format!("agend-watch-append-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repository": "owner/repo", "branch": "feat-test"});

    handle_watch_ci(&home, &args, "lead");
    handle_watch_ci(&home, &args, "dev");

    let watch = read_watch(&watch_path_for(&home, "owner/repo", "feat-test"));
    let subs: Vec<&str> = watch["subscribers"]
        .as_array()
        .expect("subscribers array")
        .iter()
        .map(|s| s["instance"].as_str().unwrap())
        .collect();
    assert_eq!(
        subs,
        vec!["lead", "dev"],
        "both callers must be present, not last-write-wins"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ci_watch_double_subscribe_same_caller_is_idempotent() {
    // Same caller calling twice must not duplicate. Idempotent in
    // the strict mathematical sense — `f(f(x)) == f(x)`.
    let home = std::env::temp_dir().join(format!("agend-watch-dup-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repository": "owner/repo", "branch": "feat-test"});

    handle_watch_ci(&home, &args, "lead");
    handle_watch_ci(&home, &args, "lead");
    handle_watch_ci(&home, &args, "lead");

    let watch = read_watch(&watch_path_for(&home, "owner/repo", "feat-test"));
    let subs = watch["subscribers"].as_array().unwrap();
    assert_eq!(subs.len(), 1, "duplicate subscribe must collapse");
    assert_eq!(subs[0]["instance"].as_str(), Some("lead"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ci_watch_preserves_poll_state_on_resubscribe() {
    // Re-subscribing must NOT reset last_run_id / last_polled_at —
    // otherwise the next poll re-emits the last terminal run as a
    // duplicate notification.
    let home = std::env::temp_dir().join(format!("agend-watch-state-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repository": "owner/repo", "branch": "feat-test"});

    handle_watch_ci(&home, &args, "lead");

    // Simulate the daemon's poll-loop having stamped state.
    let path = watch_path_for(&home, "owner/repo", "feat-test");
    let mut watch = read_watch(&path);
    watch["last_run_id"] = serde_json::json!(42_u64);
    watch["last_polled_at"] = serde_json::json!(1_700_000_000_000_i64);
    watch["last_notified_head_sha"] = serde_json::json!("abc1234");
    std::fs::write(&path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

    // dev subscribes.
    handle_watch_ci(&home, &args, "dev");

    let watch = read_watch(&path);
    assert_eq!(
        watch["last_run_id"].as_u64(),
        Some(42),
        "poll state must survive append"
    );
    assert_eq!(
        watch["last_polled_at"].as_i64(),
        Some(1_700_000_000_000_i64)
    );
    assert_eq!(watch["last_notified_head_sha"].as_str(), Some("abc1234"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ci_watch_legacy_instance_field_migrates_on_resubscribe() {
    // Hard contract item 3: legacy `instance: "X"` files migrate to
    // `subscribers: [{instance: X, ...}]` on the next write. The
    // legacy field is preserved as a deprecated alias so a rollback
    // to a pre-r0 daemon binary can still read SOMEONE.
    let home = std::env::temp_dir().join(format!("agend-watch-migrate-{}", std::process::id()));
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
    std::fs::create_dir_all(&ci_dir).ok();
    let path = watch_path_for(&home, "owner/repo", "feat-test");

    // Hand-craft a legacy watch file (no subscribers array).
    let legacy = serde_json::json!({
        "repo": "owner/repo",
        "branch": "feat-test",
        "interval_secs": 60,
        "instance": "lead",
        "last_run_id": 100,
        "head_sha": "abc",
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

    // Trigger migration via a fresh subscribe.
    handle_watch_ci(
        &home,
        &serde_json::json!({"repository": "owner/repo", "branch": "feat-test"}),
        "dev",
    );

    let watch = read_watch(&path);
    let subs: Vec<&str> = watch["subscribers"]
        .as_array()
        .expect("subscribers must exist post-migration")
        .iter()
        .map(|s| s["instance"].as_str().unwrap())
        .collect();
    assert_eq!(
        subs,
        vec!["lead", "dev"],
        "legacy lead retained, dev appended"
    );
    // Legacy field preserved as deprecated alias = first subscriber.
    assert_eq!(watch["instance"].as_str(), Some("lead"));
    // Poll state survived.
    assert_eq!(watch["last_run_id"].as_u64(), Some(100));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ci_unwatch_removes_caller_only_when_others_remain() {
    // Hard contract item 5 (a): `ci unwatch` removes the caller
    // and writes the file back. Watch file is NOT deleted.
    let home = std::env::temp_dir().join(format!("agend-unwatch-keep-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repository": "owner/repo", "branch": "feat-test"});
    handle_watch_ci(&home, &args, "lead");
    handle_watch_ci(&home, &args, "dev");

    let path = watch_path_for(&home, "owner/repo", "feat-test");
    assert!(path.exists());

    let unwatch_args = serde_json::json!({
        "repository": "owner/repo",
        "branch": "feat-test",
        "instance": "lead",
    });
    let resp = handle_unwatch_ci(&home, &unwatch_args, "lead");

    assert_eq!(
        resp["watching"].as_bool(),
        Some(true),
        "still watched by dev"
    );
    assert!(path.exists(), "file must remain while subscribers > 0");

    let watch = read_watch(&path);
    let subs: Vec<&str> = watch["subscribers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["instance"].as_str().unwrap())
        .collect();
    assert_eq!(subs, vec!["dev"]);
    // Legacy alias also rolls forward.
    assert_eq!(watch["instance"].as_str(), Some("dev"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ci_unwatch_last_subscriber_leaves_tombstone_not_delete() {
    // Hard contract item 5 (b), REVISED by #1991: the LAST unwatch used to
    // DELETE the file — but PR-3 auto-arm re-arms any open PR whose watch
    // file is absent, so the delete re-subscribed the just-unwatched agent
    // ~60s later. The last unwatch now leaves a subscriber-less TOMBSTONE:
    // never polled (prepare_poll_context skips empty-subscriber watches, so
    // the rate-limit budget is still protected), never re-armed, and reaped
    // by gc only at PR-terminal or the unwatched_at age-cap (P6: it survives
    // the TTL/inactivity reaps — unwatch is an explicit decision).
    let home = std::env::temp_dir().join(format!("agend-unwatch-delete-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repository": "owner/repo", "branch": "feat-test"});
    handle_watch_ci(&home, &args, "lead");

    let path = watch_path_for(&home, "owner/repo", "feat-test");
    assert!(path.exists());

    let unwatch_args = serde_json::json!({
        "repository": "owner/repo",
        "branch": "feat-test",
        "instance": "lead",
    });
    let resp = handle_unwatch_ci(&home, &unwatch_args, "lead");

    assert_eq!(resp["watching"].as_bool(), Some(false));
    assert!(
        path.exists(),
        "#1991: last unwatch leaves a tombstone (deletion re-armed via PR-3)"
    );
    let v = read_watch(&path);
    assert_eq!(v["auto_arm_optout"], true);
    assert!(crate::daemon::ci_watch::parse_subscribers(&v).is_empty());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ci_unwatch_unknown_caller_is_noop_keeps_watch() {
    // Defensive: unwatch from an instance that never subscribed
    // must not silently delete the watch (would have been a quiet
    // way to clobber lead's watch via dev's typo).
    let home = std::env::temp_dir().join(format!("agend-unwatch-noop-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repository": "owner/repo", "branch": "feat-test"});
    handle_watch_ci(&home, &args, "lead");

    let path = watch_path_for(&home, "owner/repo", "feat-test");
    let unwatch_args = serde_json::json!({
        "repository": "owner/repo",
        "branch": "feat-test",
        "instance": "stranger",
    });
    handle_unwatch_ci(&home, &unwatch_args, "stranger");

    assert!(
        path.exists(),
        "lead's watch must survive stranger's unwatch"
    );
    let watch = read_watch(&path);
    let subs: Vec<&str> = watch["subscribers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["instance"].as_str().unwrap())
        .collect();
    assert_eq!(subs, vec!["lead"]);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ci_unwatch_explicit_empty_instance_keeps_other_subscribers() {
    // H4 hardening (CR-2026-06-14), now reinforced by
    // t-20260705161926295621-30532-2 ②: an agent-supplied `instance` arg must
    // not reach the empty-caller `subscribers.clear()` branch. Since the
    // `instance` override was removed (caller is ALWAYS the validated sender),
    // the arg is ignored entirely — caller resolves to instance_name ("lead"),
    // removing only "lead". Guards the clear-all edge stays closed.
    let home = std::env::temp_dir().join(format!("agend-unwatch-empty-arg-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repository": "owner/repo", "branch": "feat-test"});
    handle_watch_ci(&home, &args, "lead");
    handle_watch_ci(&home, &args, "dev");

    let path = watch_path_for(&home, "owner/repo", "feat-test");
    // lead unwatches with an EXPLICIT empty instance arg; validated caller = "lead".
    let unwatch_args = serde_json::json!({
        "repository": "owner/repo",
        "branch": "feat-test",
        "instance": "",
    });
    handle_unwatch_ci(&home, &unwatch_args, "lead");

    let subs = crate::daemon::ci_watch::parse_subscribers(&read_watch(&path));
    assert!(
        subs.iter().any(|s| s == "dev"),
        "dev must remain subscribed; an empty instance arg must not clear-all. got {subs:?}"
    );
    assert!(
        !subs.iter().any(|s| s == "lead"),
        "lead (the validated caller) should be removed. got {subs:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ci_unwatch_instance_arg_cannot_drop_another_agents_subscription_30532() {
    // #2622-followup t-20260705161926295621-30532-2 ② (decision
    // d-20260705165815268234-1): the former `args["instance"]` override let
    // agent A pass `instance="agent-B"` to silently drop B's CI-watch
    // subscription (and resolve B's ci-handoff obligation track) — an
    // unauthenticated cross-agent state change, the #2622 obligation-loss
    // class. The override is REMOVED: caller is ALWAYS the validated sender, so
    // a forged `instance` arg naming another agent is ignored and B stays
    // subscribed. Pins the new self-only semantics against regression.
    let home = std::env::temp_dir().join(format!("agend-unwatch-xagent-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repository": "owner/repo", "branch": "feat-xagent"});
    // B subscribes to the watch.
    handle_watch_ci(&home, &args, "agent-b");

    // A (validated sender) tries to unwatch B by naming it in the instance arg.
    let unwatch_args = serde_json::json!({
        "repository": "owner/repo",
        "branch": "feat-xagent",
        "instance": "agent-b",
    });
    handle_unwatch_ci(&home, &unwatch_args, "agent-a");

    let path = watch_path_for(&home, "owner/repo", "feat-xagent");
    let subs = crate::daemon::ci_watch::parse_subscribers(&read_watch(&path));
    assert!(
        subs.iter().any(|s| s == "agent-b"),
        "A must NOT be able to drop B's subscription via a forged `instance` arg \
         (caller is the validated sender); got {subs:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------
// Sprint 54 P0-5 — Agent-visible CI health surface. Tests cover all
// three sub-scopes from dispatch m-20260507045729197032-16:
//
//   A. handle_watch_ci response enrichment (rate_limit_active +
//      rate_limit_until + next_poll_eta)
//   B. consecutive_skips tracking + stalled/resumed inbox events live
//      in the daemon-side `bump_consecutive_skips_and_maybe_notify` /
//      `clear_stall_and_maybe_notify_resumed` helpers; their tests
//      live in `src/daemon/ci_watch.rs` (this handler doesn't drive
//      the tick-loop schema).
//   C. ci status MCP action — caller-scoped + filter semantics +
//      empty-state shape.
// ---------------------------------------------------------------------

#[test]
fn watch_ci_response_includes_health_fields_when_state_populated() {
    // Sub-scope A gate 1: response carries the new diagnostic fields
    // even on the first watch. Fresh watches have null poll state, so
    // `next_poll_eta` is null and `rate_limit_active` is false — but
    // the FIELDS must exist so agents can pattern-match without
    // optional-field ladders.
    let home = std::env::temp_dir().join(format!("agend-p05-A1-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repository": "owner/repo", "branch": "feat-test"});
    let resp = handle_watch_ci(&home, &args, "lead");

    assert_eq!(resp["watching"].as_bool(), Some(true));
    assert!(
        resp.get("rate_limit_active").is_some(),
        "rate_limit_active must always be present"
    );
    assert_eq!(resp["rate_limit_active"].as_bool(), Some(false));
    assert!(resp["rate_limit_until"].is_null());
    assert!(resp["next_poll_eta"].is_null());

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn watch_ci_rate_limit_active_when_until_in_future() {
    // Sub-scope A gate 2: rate_limit_until > now ⇒ rate_limit_active
    // surfaces true. Hand-craft state to simulate a tick loop having
    // just stamped rate_limit_until.
    let home = std::env::temp_dir().join(format!("agend-p05-A2-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repository": "owner/repo", "branch": "feat-test"});
    handle_watch_ci(&home, &args, "lead");

    let path = watch_path_for(&home, "owner/repo", "feat-test");
    let mut watch = read_watch(&path);
    let future_secs = chrono::Utc::now().timestamp() + 3600;
    watch["rate_limit_until"] = serde_json::json!(future_secs);
    watch["last_polled_at"] = serde_json::json!(chrono::Utc::now().timestamp_millis());
    watch["effective_interval_secs"] = serde_json::json!(120);
    std::fs::write(&path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

    // Re-call watch_ci (idempotent re-subscribe) — response should
    // reflect the new state.
    let resp = handle_watch_ci(&home, &args, "lead");
    assert_eq!(resp["rate_limit_active"].as_bool(), Some(true));
    assert_eq!(resp["rate_limit_until"].as_i64(), Some(future_secs));
    assert!(resp["next_poll_eta"].as_i64().is_some());

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn watch_ci_next_poll_eta_null_for_fresh_watch() {
    // Sub-scope A gate 3: a fresh watch (no last_polled_at yet) has
    // null next_poll_eta — agents shouldn't be lied to about "when's
    // the next poll" when no poll has happened yet.
    let home = std::env::temp_dir().join(format!("agend-p05-A3-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repository": "owner/repo", "branch": "feat-test"});
    let resp = handle_watch_ci(&home, &args, "lead");
    assert!(resp["next_poll_eta"].is_null());
    std::fs::remove_dir_all(&home).ok();
}

// ---- Sub-scope C: ci status MCP tool ----

#[test]
fn ci_status_returns_caller_subscribed_watches() {
    // Sub-scope C gate 1: caller scoping — only watches that include
    // the caller in `subscribers` come back. A second watch the
    // caller didn't subscribe to is filtered out.
    let home = std::env::temp_dir().join(format!("agend-p05-C1-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    handle_watch_ci(
        &home,
        &serde_json::json!({"repository": "o/r1", "branch": "feat-test"}),
        "lead",
    );
    handle_watch_ci(
        &home,
        &serde_json::json!({"repository": "o/r2", "branch": "feat-test"}),
        "dev",
    );

    let resp = handle_status_ci(&home, &serde_json::json!({}), "lead");
    let watches = resp["watches"].as_array().unwrap();
    assert_eq!(watches.len(), 1, "lead sees only their watch");
    assert_eq!(watches[0]["repo"].as_str(), Some("o/r1"));
    assert!(watches[0]
        .get("rate_limit_active")
        .and_then(|v| v.as_bool())
        .is_some());

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ci_status_filter_by_repo_returns_subset() {
    // Sub-scope C gate 2: optional repo filter narrows to a single
    // watch even when caller has multiple subscriptions.
    //
    // EMPIRICAL REGRESSION-PROOF ANCHOR: if `handle_status_ci`
    // accidentally drops the `filter_repo` check (e.g. early-return
    // before the comparison), this test fails because both
    // subscribed watches surface. PR description captures the FAIL
    // signature.
    let home = std::env::temp_dir().join(format!("agend-p05-C2-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    handle_watch_ci(
        &home,
        &serde_json::json!({"repository": "o/alpha", "branch": "feat-test"}),
        "lead",
    );
    handle_watch_ci(
        &home,
        &serde_json::json!({"repository": "o/beta", "branch": "feat-test"}),
        "lead",
    );

    let resp = handle_status_ci(&home, &serde_json::json!({"repository": "o/alpha"}), "lead");
    let watches = resp["watches"].as_array().unwrap();
    assert_eq!(watches.len(), 1, "filter must narrow to o/alpha only");
    assert_eq!(watches[0]["repo"].as_str(), Some("o/alpha"));

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ci_status_no_watches_returns_empty_array_not_error() {
    // Sub-scope C gate 3: empty-state shape is `{"watches": []}`,
    // never an error. Agents that pattern-match on `watches.length`
    // shouldn't have to handle a separate not-found code.
    let home = std::env::temp_dir().join(format!("agend-p05-C3-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let resp = handle_status_ci(&home, &serde_json::json!({}), "lead");
    assert!(resp.get("error").is_none());
    let watches = resp["watches"].as_array().expect("watches array");
    assert!(watches.is_empty());
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------
// Sprint 57 Wave 2 Track B (#546 Item 3) — handle_watch_ci E4.5 gate.
// ---------------------------------------------------------------------

fn watch_test_home(tag: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("agend-watch-e45-{}-{}", std::process::id(), tag));
    std::fs::create_dir_all(&home).ok();
    home
}

#[test]
fn handle_watch_ci_rejects_protected_refs() {
    // Both `main` and `master` must surface the canonical
    // `e4_5_protected_branch` error code so callers can branch on
    // it the same way `bind_self` callers do.
    for branch in &["main", "master"] {
        let home = watch_test_home(&format!("reject-{branch}"));
        let resp = super::handle_watch_ci(
            &home,
            &serde_json::json!({"repository": "owner/repo", "branch": branch}),
            "dev",
        );
        assert!(
            resp["error"].as_str().is_some(),
            "branch={branch} must error, got {resp}"
        );
        assert_eq!(
            resp["code"].as_str(),
            Some("e4_5_protected_branch"),
            "branch={branch} error code must be e4_5_protected_branch, got {resp}"
        );

        // No side-effect on rejection: ci-watches dir must not gain
        // a new file for the protected branch.
        let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
        if let Ok(rd) = std::fs::read_dir(&ci_dir) {
            let n: usize = rd.count();
            assert_eq!(
                n, 0,
                "rejected ci_watch must not write a watch file (branch={branch})"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }
}

#[test]
fn handle_watch_ci_default_branch_does_not_silently_set_main() {
    // The handler defaults `branch` to "main" when the caller omits
    // it. After the Item 3 gate, that default-then-create flow must
    // be rejected with the same E4.5 error rather than silently
    // creating a watch on main. Pin against re-introduction of the
    // silent-default behavior.
    let home = watch_test_home("default-no-silent-main");
    let resp = super::handle_watch_ci(
        &home,
        // NO branch field — exercises the `unwrap_or("main")`.
        &serde_json::json!({"repository": "owner/repo"}),
        "dev",
    );
    assert_eq!(
        resp["code"].as_str(),
        Some("e4_5_protected_branch"),
        "default-branch path must hit the E4.5 gate, got {resp}"
    );

    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
    if let Ok(rd) = std::fs::read_dir(&ci_dir) {
        assert_eq!(rd.count(), 0, "no watch file must be created on rejection");
    }
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn handle_watch_ci_accepts_non_protected_branch() {
    // Defensive bonus pin: a feature branch must still be accepted
    // post-gate (the gate must be a CHECK not a refusal-of-all).
    let home = watch_test_home("accept-feature");
    let resp = super::handle_watch_ci(
        &home,
        &serde_json::json!({
            "repository": "owner/repo",
            "branch": "feat/sprint57-wave2-track-b",
            "interval_secs": 60_u64,
        }),
        "dev",
    );
    // Either Ok shape OR a different error — but NOT
    // e4_5_protected_branch.
    let code = resp["code"].as_str().unwrap_or("");
    assert_ne!(
        code, "e4_5_protected_branch",
        "non-protected branch must NOT trip E4.5 gate, got {resp}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn handle_watch_ci_accepts_multi_next_after_ci_targets_2502() {
    let home = watch_test_home("multi-next-after-ci-2502");
    let resp = super::handle_watch_ci(
        &home,
        &serde_json::json!({
            "repository": "owner/repo",
            "branch": "feat/multi-next-after-ci",
            "next_after_ci": ["reviewer-b", "reviewer-a", "reviewer-a", ""],
        }),
        "dev",
    );
    assert!(
        resp["watching"].as_bool().unwrap_or(false),
        "watching must succeed for array next_after_ci: {resp}"
    );

    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("owner/repo", "feat/multi-next-after-ci"),
    );
    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
    assert_eq!(
        watch["next_after_ci"],
        serde_json::json!(["reviewer-a", "reviewer-b"]),
        "next_after_ci array must persist all unique handoff targets"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ----------------------------------------------------------------------
// #778 Option 1: `repo action=checkout bind:true` — atomic provision +
// bind. Closes the chicken-and-egg surfaced by validation canary
// 2026-05-14 (dossier /tmp/val-workflow-2026-05-14.md). Empirical
// anchor: comment out the `bind` block in handle_checkout_repo →
// `checkout_bind_true_writes_binding_marker_and_arms_watch` fails
// because binding.json never gets written.
//
// Happy-path tests spawn real git subprocesses (init/commit/remote/
// branch/worktree-add) and are `#[cfg(unix)]` — Windows CI runner's
// git-subprocess concurrency was observed to cause unrelated
// `worktree_pool::tests::*` regressions when these tests ran in
// parallel. The daemon code itself is cross-platform (Windows path
// mangling now collapses `\` and `:` alongside `/`); only the
// integration-style happy-path tests are unix-gated. The E4.5 +
// anonymous-caller error-path tests below stay cross-platform.
// ----------------------------------------------------------------------

fn p778_tmp_home(suffix: &str) -> std::path::PathBuf {
    let h = std::env::temp_dir().join(format!(
        "agend-p778-bind-{}-{}-{}",
        std::process::id(),
        suffix,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&h).ok();
    h
}

/// Fixture: a real git source repo with `origin` remote pointing at a
/// GitHub-style URL so `derive_repo_from_remote_pub` resolves to
/// `owner/repo` and the test exercises the auto-watch_ci arm. One
/// initial commit on `main`, plus a feature branch named `branch`
/// pre-created so `git worktree add <path> <branch>` succeeds.
#[cfg(unix)]
fn p778_setup_source_repo(parent: &Path, branch: &str) -> std::path::PathBuf {
    let repo = parent.join("source-repo");
    std::fs::create_dir_all(&repo).ok();
    let bypass = ("AGEND_GIT_BYPASS", "1");
    let _ = std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    let _ = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    let _ = std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    let _ = std::process::Command::new("git")
        .args(["branch", branch, "main"])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    repo
}

// #2158 GR1: `checkout_bind_true_no_longer_auto_derives_next_after_ci_pr2` was
// DELETED here — it asserted a self-claim `repo checkout bind:true` armed a watch
// (without next_after_ci). GR1 removes that silent self-claim auto-arm entirely, so
// the test's whole premise (a self-claim watch sidecar) no longer exists; the new
// "no longer arms" behavior is pinned by the test just below.

#[test]
#[cfg(unix)]
fn checkout_bind_true_writes_binding_marker_and_no_longer_arms_watch_2158_gr1() {
    // Empirical regression-proof anchor for #778 Option 1 (bind / marker / HEAD),
    // now ALSO pinning #2158 GR1: the dev-self-claim path NO LONGER auto-arms a
    // ci_watch. The daemon DISPATCH path still arms (dispatch_hook tests).
    let home = p778_tmp_home("ok");
    let parent = p778_tmp_home("ok-src-parent");
    let source = p778_setup_source_repo(&parent, "feat/p778");
    let agent = "p778-agent-ok";

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/p778",
            "bind": true,
        }),
        agent,
    );

    assert!(resp.get("error").is_none(), "checkout must succeed: {resp}");
    assert_eq!(
        resp["bound"].as_bool(),
        Some(true),
        "bind=true must surface bound flag: {resp}"
    );

    let wt_path = std::path::PathBuf::from(resp["path"].as_str().expect("path"));
    assert!(wt_path.exists(), "worktree dir must exist: {resp}");
    assert!(
        wt_path.join(crate::worktree_pool::MANAGED_MARKER).exists(),
        ".agend-managed marker must be written"
    );

    let binding = crate::paths::runtime_dir(&home)
        .join(agent)
        .join("binding.json");
    assert!(binding.exists(), "binding.json must be written");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&binding).unwrap()).unwrap();
    assert_eq!(v["branch"].as_str(), Some("feat/p778"));
    assert_eq!(
        v["task_id"].as_str(),
        Some(""),
        "atomic bind must record empty task_id (no sentinel)"
    );

    // #2158 GR1: the dev-self-claim `repo checkout bind:true` must NO LONGER
    // auto-arm ci_watch — no sidecar for the derived repo+branch.
    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("owner/repo", "feat/p778"),
    );
    assert!(
        !watch_path.exists(),
        "#2158 GR1: self-claim checkout bind:true must NOT auto-arm ci_watch"
    );
    assert_eq!(
        resp["ci_watch_armed"].as_bool(),
        Some(false),
        "self-claim checkout must report ci_watch_armed:false: {resp}"
    );
    // #2158 GR1: a self-claim `repo checkout bind:true` surfaces the
    // `binding_out_of_dispatch` MARKER (it passes is_self_claim=true to
    // bind_full). #2347: the live DELIVERY now routes via the bound agent's team
    // orchestrator (fallback `general` — this teamless test agent → general); the
    // unconditional event-log marker is the GR1-forensics invariant asserted here
    // (live delivery goes through the compose-aware inject, not a drainable inbox,
    // so the recipient is verified by the `out_of_dispatch_notify_recipient` unit
    // tests in `binding.rs`, not here).
    assert!(
        std::fs::read_to_string(home.join("event-log.jsonl"))
            .unwrap_or_default()
            .contains("binding_out_of_dispatch"),
        "#2158 GR1 marker (unconditional; #2347 delivery routes via team orchestrator): \
         self-claim checkout bind:true must surface binding_out_of_dispatch"
    );

    // HEAD must be on the named branch (NOT detached). Verifies the
    // `--detach` omission for bind:true so subsequent commits land
    // on the right ref.
    let head_ref = std::fs::read_to_string(wt_path.join(".git"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("gitdir:").map(str::trim))
                .map(std::path::PathBuf::from)
        })
        .and_then(|d| std::fs::read_to_string(d.join("HEAD")).ok())
        .unwrap_or_default();
    assert!(
        head_ref.starts_with("ref: refs/heads/feat/p778"),
        "HEAD must point at refs/heads/feat/p778 (no --detach), got: {head_ref:?}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

/// #2703: `repo action=checkout bind:true` WITHOUT an explicit `from_ref` must
/// default the new branch's base to the repo's DEFAULT branch (`origin/HEAD`),
/// not a hard-coded `origin/main` (checkout.rs's `unwrap_or("origin/main")`).
/// Same root as the dispatch-path fix (dispatch_hook/mod.rs:488). RED before the
/// `default_branch()` swap (pre-fix the created branch tips at origin/main).
#[test]
#[cfg(unix)]
fn checkout_bind_true_defaults_base_to_repo_default_branch_2703() {
    let home = p778_tmp_home("2703-devdefault");
    let parent = p778_tmp_home("2703-devdefault-src");
    let origin = parent.join("o.git");
    let source = parent.join("source-repo");

    let git = |args: &[&str], dir: &std::path::Path| -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git spawn");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // Bare origin whose DEFAULT branch is `dev`, with main != dev tips.
    std::fs::create_dir_all(&origin).ok();
    git(&["init", "--bare", "-b", "main"], &origin);
    std::fs::create_dir_all(&source).ok();
    git(&["init", "-b", "main"], &source);
    git(
        &["remote", "add", "origin", origin.to_str().unwrap()],
        &source,
    );
    git(
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "MAIN",
        ],
        &source,
    );
    let main_sha = git(&["rev-parse", "HEAD"], &source);
    git(&["push", "-q", "origin", "main"], &source);
    git(&["checkout", "-b", "dev"], &source);
    git(
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "DEV",
        ],
        &source,
    );
    let dev_sha = git(&["rev-parse", "HEAD"], &source);
    git(&["push", "-q", "origin", "dev"], &source);
    git(&["symbolic-ref", "HEAD", "refs/heads/dev"], &origin);
    git(&["fetch", "origin", "--quiet"], &source);
    git(&["remote", "set-head", "origin", "dev"], &source);
    assert_ne!(main_sha, dev_sha);

    // No `from_ref` supplied → must default to the repo default (origin/dev).
    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/x-2703",
            "bind": true,
        }),
        "checkout-2703-agent",
    );
    assert!(resp.get("error").is_none(), "checkout must succeed: {resp}");

    let created = git(&["rev-parse", "refs/heads/feat/x-2703"], &source);
    assert_eq!(
        created, dev_sha,
        "#2703: checkout bind:true default base must be repo default (origin/dev), \
         got {created} (origin/main={main_sha})"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

/// #2533: `repo action=checkout bind:true task_id=...` (the reviewer-workflow
/// self-claim path, §3.19.1) must thread the caller-supplied `task_id` into
/// `binding.json` — pre-fix, `handle_checkout_repo_inner` hardcoded `""`
/// regardless of the `task_id` arg — AND the task_id linkage must suppress the
/// `binding_out_of_dispatch` warning (the checkout is now attributable to a
/// task, not a rogue bind).
#[test]
#[cfg(unix)]
fn checkout_bind_true_with_task_id_records_task_id_and_suppresses_warning_2533() {
    let home = p778_tmp_home("task-id");
    let parent = p778_tmp_home("task-id-src-parent");
    let source = p778_setup_source_repo(&parent, "feat/p2533");
    let agent = "p2533-agent";

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/p2533",
            "bind": true,
            "task_id": "T-2533",
        }),
        agent,
    );
    assert!(resp.get("error").is_none(), "checkout must succeed: {resp}");

    let binding = crate::paths::runtime_dir(&home)
        .join(agent)
        .join("binding.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&binding).unwrap()).unwrap();
    assert_eq!(
        v["task_id"].as_str(),
        Some("T-2533"),
        "#2533: checkout bind:true must record the caller-supplied task_id, not a hardcoded \
         empty sentinel: {v}"
    );

    assert!(
        !std::fs::read_to_string(home.join("event-log.jsonl"))
            .unwrap_or_default()
            .contains("binding_out_of_dispatch"),
        "#2533: a checkout bind:true CARRYING a task_id must be treated as in-dispatch — no \
         out-of-dispatch warning"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

/// §3.9 #1882 (reviewer-2 re-verify): the repo-checkout bind path is the third
/// production bind site. An end-to-end conflict — agent A checks out + binds a
/// branch, then agent B repo-checkouts the SAME branch — must end with EXACTLY
/// one binding: B is rejected (`cross_agent_conflict`) under the shared per-branch
/// lease lock + scan, NOT double-bound. Regression-proof: pre-fix B had no scan,
/// so it reached `git worktree add` and failed with a raw "already checked out"
/// error (different code) or, on a different worktree scheme, double-bound.
#[test]
#[cfg(unix)]
fn checkout_bind_rejects_cross_agent_branch_conflict_1882() {
    let home = p778_tmp_home("xagent");
    let parent = p778_tmp_home("xagent-src-parent");
    let source = p778_setup_source_repo(&parent, "feat/shared");

    // Agent A checks out + binds feat/shared.
    let a = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/shared",
            "bind": true,
        }),
        "agent-a",
    );
    assert!(
        a.get("error").is_none(),
        "agent A checkout must succeed: {a}"
    );
    assert_eq!(a["bound"].as_bool(), Some(true));

    // Agent B tries the SAME branch → rejected (cross-agent), not double-bound.
    let b = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/shared",
            "bind": true,
        }),
        "agent-b",
    );
    assert_eq!(
        b["code"].as_str(),
        Some("cross_agent_conflict"),
        "#1882: a second agent on the same branch must be rejected: {b}"
    );
    assert!(
        !crate::paths::runtime_dir(&home)
            .join("agent-b")
            .join("binding.json")
            .exists(),
        "#1882: the rejected agent must NOT be bound"
    );
    // Exactly one binding holds feat/shared (it is agent-a's).
    assert_eq!(
        crate::binding::scan_existing_branch_binding(&home, "", "feat/shared", ""),
        Some("agent-a".to_string()),
        "#1882: exactly one agent must hold the branch after the conflict"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn checkout_bind_true_idempotent_when_already_bound_1494() {
    // #1494 RED→GREEN: the dispatch pre-build hook leases the agent's
    // worktree at the canonical `<agent>/<branch>` path and writes
    // binding.json before the agent claims. When the agent THEN runs
    // `repo action=checkout bind:true` on the same branch, this handler's
    // DIFFERENT `<agent>-<source>` path scheme means the direct
    // `git worktree add <agent>-<source> <branch>` fails with "is already
    // checked out at <agent>/<branch>" — the release-dance pain. The
    // handler must detect the existing same-branch binding and return that
    // worktree path idempotently (same spirit as #1465 idempotent-release).
    //
    // REGRESSION-PROOF: delete the `if bind { if let Some(existing) = ...`
    // short-circuit in handle_checkout_repo → the claim re-runs
    // `git worktree add` on an already-checked-out branch, this returns
    // `{error, code:"worktree_add_failed"}`, and the assertions below fail.
    let home = p778_tmp_home("1494-idem");
    let parent = p778_tmp_home("1494-idem-src");
    let source = p778_setup_source_repo(&parent, "feat/1494");
    let agent = "idem-dev";

    // Simulate the dispatch pre-build: lease the worktree at the canonical
    // `<agent>/<branch>` path + write binding.json — exactly what the
    // dispatch hook does before the agent claims.
    let lease = crate::worktree_pool::lease(&home, &source, agent, "feat/1494")
        .expect("dispatch pre-build lease must succeed");
    // lease no longer binds (finding D+H); the authoritative caller binds —
    // simulate dispatch's pre-build bind here so the #1494 idempotent re-bind
    // short-circuit finds the existing same-branch binding.
    crate::binding::bind_full(&home, agent, "", "feat/1494", &lease.path, &source, false)
        .expect("dispatch pre-build bind_full");
    let dispatch_path = lease.path.clone();
    assert!(dispatch_path.exists(), "pre-built worktree must exist");

    // Agent claims via `repo checkout bind:true` on the SAME branch.
    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/1494",
            "bind": true,
        }),
        agent,
    );

    assert!(
        resp.get("error").is_none(),
        "#1494: re-bind to already-bound branch must be idempotent, not error: {resp}"
    );
    assert_eq!(
        resp["bound"].as_bool(),
        Some(true),
        "must still report bound: {resp}"
    );
    assert_eq!(
        resp["idempotent"].as_bool(),
        Some(true),
        "#1494: short-circuit must flag the idempotent reuse: {resp}"
    );
    assert_eq!(
        resp["path"].as_str(),
        Some(dispatch_path.display().to_string().as_str()),
        "#1494: must return the EXISTING dispatch worktree path, not a fresh one: {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn checkout_bind_true_rejects_stale_dispatch_binding_2499() {
    let home = p778_tmp_home("2499-stale");
    let parent = p778_tmp_home("2499-stale-src");
    let source = p778_setup_source_repo(&parent, "feat/2499");
    let agent = "stale-dev";
    let task_id = "t-2499";

    let lease = crate::worktree_pool::lease(&home, &source, agent, "feat/2499")
        .expect("dispatch pre-build lease must succeed");
    crate::binding::bind_full(
        &home,
        agent,
        task_id,
        "feat/2499",
        &lease.path,
        &source,
        false,
    )
    .expect("dispatch pre-build bind_full");

    std::fs::remove_dir_all(&lease.path).expect("simulate missing dispatch worktree");

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/2499",
            "bind": true,
        }),
        agent,
    );

    assert_eq!(
        resp["code"].as_str(),
        Some("stale_binding"),
        "#2499: stale dispatch binding must fail closed instead of rebinding with empty task_id: {resp}"
    );
    let binding = crate::binding::read(&home, agent).expect("binding remains for explicit release");
    assert_eq!(
        binding["task_id"].as_str(),
        Some(task_id),
        "#2499: failed checkout must not overwrite the dispatch task_id"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn checkout_writes_event_log_success_and_error_1466() {
    // #1466: every checkout outcome — success AND error — must leave a
    // `worktree_checkout` event-log trace (observability for silent
    // bootstrap failures: src/ present but no .git). Reuses event_log
    // (event-log.jsonl), no new schema.
    let home = p778_tmp_home("evlog");
    let parent = p778_tmp_home("evlog-src-parent");
    let source = p778_setup_source_repo(&parent, "feat/evlog");
    let agent = "evlog-agent";

    // Error path: missing repository_path → must still be logged (ok=false).
    let err =
        super::handle_checkout_repo(&home, &serde_json::json!({ "branch": "feat/evlog" }), agent);
    assert!(err.get("error").is_some(), "missing repo must error: {err}");

    // Success path.
    let ok = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/evlog",
        }),
        agent,
    );
    assert!(ok.get("error").is_none(), "checkout must succeed: {ok}");

    let log = std::fs::read_to_string(home.join("event-log.jsonl")).expect("event-log written");
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("worktree_checkout"))
        .collect();
    assert!(
        lines.len() >= 2,
        "both checkout outcomes must be logged: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("ok=false") && l.contains("err=")),
        "error outcome must log ok=false + err: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("ok=true") && l.contains("branch=feat/evlog")),
        "success outcome must log ok=true + branch: {lines:?}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn checkout_bind_false_default_preserves_detached_no_binding() {
    // Back-compat: existing callers (review pool, operator triage) pass
    // no `bind` arg → behavior identical to pre-#778 — detached HEAD,
    // no binding.json, no marker, no auto-watch.
    let home = p778_tmp_home("bc");
    let parent = p778_tmp_home("bc-src-parent");
    let source = p778_setup_source_repo(&parent, "feat/p778-bc");
    let agent = "p778-agent-bc";

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/p778-bc",
        }),
        agent,
    );

    assert!(resp.get("error").is_none(), "checkout must succeed: {resp}");
    assert!(
        resp.get("bound").is_none(),
        "default checkout must NOT surface bound: {resp}"
    );

    let wt_path = std::path::PathBuf::from(resp["path"].as_str().expect("path"));
    assert!(
        wt_path.join(crate::worktree_pool::MANAGED_MARKER).exists(),
        ".agend-managed marker must be written even without bind:true (#1275)"
    );

    let binding = crate::paths::runtime_dir(&home)
        .join(agent)
        .join("binding.json");
    assert!(
        !binding.exists(),
        "binding.json must NOT be written without bind:true"
    );

    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("owner/repo", "feat/p778-bc"),
    );
    assert!(
        !watch_path.exists(),
        "watch_ci must NOT be armed without bind:true"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
fn checkout_bind_true_rejects_protected_branch_e45() {
    // E4.5 invariant: bind:true must reject `main`/`master` since it
    // grants write authority. Mirrors bind_self's protected-ref gate.
    let home = p778_tmp_home("e45");

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": "/tmp",  // never reached — E4.5 fires first
            "branch": "main",
            "bind": true,
        }),
        "p778-agent-e45",
    );

    assert!(resp.get("error").is_some(), "main must be rejected: {resp}");
    assert_eq!(
        resp["code"].as_str(),
        Some("e4_5_protected_branch"),
        "code must mark E4.5 class: {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn checkout_bind_true_rejects_anonymous_caller() {
    // bind:true is a write-side operation that must be attributed to a
    // named agent. Anonymous (empty instance_name) callers cannot
    // claim a worktree — surface as `needs_identity` so the caller
    // knows to set AGEND_INSTANCE_NAME.
    let home = p778_tmp_home("anon");

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": "/tmp",
            "branch": "feat/p778",
            "bind": true,
        }),
        "",
    );

    assert!(resp.get("error").is_some(), "anon must be rejected: {resp}");
    assert_eq!(
        resp["code"].as_str(),
        Some("needs_identity"),
        "code must demand identity: {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ----------------------------------------------------------------------
// #780 `from_ref` auto-create branch — lazy fetch-on-missing-ref. Closes
// the single-step bypass-free workflow gap discovered post-#779: when
// caller wants `bind:true` on a brand-new branch, `git worktree add
// <path> <branch>` (no `-b`) fails with `fatal: invalid reference`. The
// new design auto-creates `<branch>` from `from_ref` (default
// `origin/main`) inside `handle_checkout_repo` so no manual `git fetch
// && git branch` pre-step is required.
//
// Source of truth: decision d-20260514102305998399-0
//
// Empirical anchor (per §3.10): comment out the new auto-create block in
// `handle_checkout_repo` → the first test below
// (`checkout_bind_true_auto_creates_branch_from_origin_main_when_missing`)
// fails because the worktree add hits `fatal: invalid reference`.
//
// All happy-path tests are `#[cfg(unix)]` (matching #778 fixture
// convention — Windows subprocess concurrency unstable in CI). Cross-
// platform stance is `unverified cross-backend claim` per §3.7; Windows
// CI smoke test tracked separately (Backlog C).
// ----------------------------------------------------------------------

/// Fixture: like `p778_setup_source_repo` but does NOT pre-create the
/// feature branch — that is the precondition that exercises the new
/// auto-create path. `refs/remotes/origin/main` is populated via
/// `git update-ref` so the default `from_ref="origin/main"` resolves
/// without a network fetch (fixture simulates a previously-fetched
/// canonical clone).
#[cfg(unix)]
fn p780_setup_source_no_feature_branch(parent: &Path) -> std::path::PathBuf {
    let repo = parent.join("source-repo");
    std::fs::create_dir_all(&repo).ok();
    let bypass = ("AGEND_GIT_BYPASS", "1");
    let _ = std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    let _ = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    let _ = std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    // Simulate fetched remote-tracking ref so `origin/main` resolves
    // locally without a network round-trip.
    let main_sha = std::process::Command::new("git")
        .args(["rev-parse", "main"])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .expect("rev-parse main");
    let _ = std::process::Command::new("git")
        .args(["update-ref", "refs/remotes/origin/main", &main_sha])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    repo
}

#[test]
#[cfg(unix)]
fn checkout_bind_true_auto_creates_branch_from_origin_main_when_missing() {
    // ANCHOR (red→green). Pre-impl: `git worktree add <path> feat/p780-new`
    // fails because the branch does not exist locally. Post-impl: handler
    // auto-creates the branch from `from_ref` (default `origin/main`)
    // before the worktree add, observable via `auto_created_branch=true`
    // and `fetch_attempted=false` (simulated origin/main was already
    // present locally — no fetch needed).
    let home = p778_tmp_home("780-auto");
    let parent = p778_tmp_home("780-auto-src");
    let source = p780_setup_source_no_feature_branch(&parent);
    let agent = "p780-agent-auto";

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/p780-new",
            "bind": true,
        }),
        agent,
    );

    assert!(
        resp.get("error").is_none(),
        "auto-create must succeed when branch missing + origin/main present: {resp}"
    );
    assert_eq!(
        resp["bound"].as_bool(),
        Some(true),
        "bind=true must surface: {resp}"
    );
    assert_eq!(
        resp["auto_created_branch"].as_bool(),
        Some(true),
        "auto_created_branch must signal the new-branch path: {resp}"
    );
    assert_eq!(
        resp["fetch_attempted"].as_bool(),
        Some(false),
        "fetch must NOT fire when origin/main is already a valid local ref: {resp}"
    );

    // HEAD must land on the named branch (not detached) so subsequent
    // commits write to the right ref. Same invariant as #778's
    // checkout_bind_true_writes_binding_marker_and_arms_watch.
    let wt_path = std::path::PathBuf::from(resp["path"].as_str().expect("path"));
    let head_ref = std::fs::read_to_string(wt_path.join(".git"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("gitdir:").map(str::trim))
                .map(std::path::PathBuf::from)
        })
        .and_then(|d| std::fs::read_to_string(d.join("HEAD")).ok())
        .unwrap_or_default();
    assert!(
        head_ref.starts_with("ref: refs/heads/feat/p780-new"),
        "HEAD must be on refs/heads/feat/p780-new, got: {head_ref:?}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn checkout_bind_true_existing_branch_ignores_from_ref() {
    // Back-compat pin: when the branch already exists in the source
    // repo, the auto-create path is skipped entirely. `from_ref` is
    // irrelevant — the caller's value (here a typo `origin/maine`) MUST
    // NOT cause a fetch or any error. `auto_created_branch=false`
    // distinguishes "branch existed" from "we authored it" so callers
    // can audit which branches the handler newly created.
    let home = p778_tmp_home("780-existing");
    let parent = p778_tmp_home("780-existing-src");
    // Use #778's fixture which DOES pre-create the feature branch.
    let source = p778_setup_source_repo(&parent, "feat/p780-existing");
    let agent = "p780-agent-existing";

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/p780-existing",
            "bind": true,
            // intentional typo — must not be consulted when branch exists.
            "from_ref": "origin/maine",
        }),
        agent,
    );

    assert!(
        resp.get("error").is_none(),
        "existing branch must succeed regardless of from_ref: {resp}"
    );
    assert_eq!(
        resp["auto_created_branch"].as_bool(),
        Some(false),
        "auto_created_branch must be false for pre-existing branch: {resp}"
    );
    assert_eq!(
        resp["fetch_attempted"].as_bool(),
        Some(false),
        "fetch must NOT fire when branch exists: {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

/// Fixture: source repo whose origin remote points at a file:// URL that
/// does not exist on disk so `git fetch origin` fails fast (no network
/// round-trip, no DNS, no hang). Used by tests that exercise the
/// fetch-failure error surface.
#[cfg(unix)]
fn p780_setup_source_broken_origin(parent: &Path) -> std::path::PathBuf {
    let repo = parent.join("source-repo-broken");
    std::fs::create_dir_all(&repo).ok();
    let bypass = ("AGEND_GIT_BYPASS", "1");
    let _ = std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    let _ = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    // file:// URL pointing at a non-existent path — `git fetch origin`
    // exits non-zero immediately with `fatal: '/...' does not appear to
    // be a git repository`.
    let broken_url = format!("file:///tmp/agend-p780-nonexistent-{}", std::process::id());
    let _ = std::process::Command::new("git")
        .args(["remote", "add", "origin", &broken_url])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output();
    // #t-83936-5: a real checkout source is a clone, so it always has a
    // refs/remotes/origin/* view. Stage one so the create-path data-loss guard —
    // which fail-closes ONLY when there is no origin view at all — proceeds to the
    // from_ref resolution this test pins, instead of refusing up front.
    let head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo)
        .env(bypass.0, bypass.1)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if !head.is_empty() {
        let _ = std::process::Command::new("git")
            .args(["update-ref", "refs/remotes/origin/main", &head])
            .current_dir(&repo)
            .env(bypass.0, bypass.1)
            .output();
    }
    repo
}

#[cfg(unix)]
fn p780_checkout_target(home: &Path, agent: &str, source: &Path) -> std::path::PathBuf {
    home.join("worktrees").join(format!(
        "{}-{}",
        agent,
        source
            .display()
            .to_string()
            .replace(['/', '\\', ':'], "_")
            .replace('~', "")
    ))
}

#[cfg(unix)]
fn p780_branch_exists(source: &Path, branch: &str) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
        .current_dir(source)
        .env("AGEND_GIT_BYPASS", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git rev-parse branch")
        .success()
}

/// Arch14 residue: a checkout that auto-created its branch must delete only that
/// branch when the fixed worktree target is already occupied and `git worktree add`
/// fails. The occupied target is deliberately preserved as out-of-scope state.
#[test]
#[cfg(unix)]
fn checkout_bind_true_worktree_add_failure_rolls_back_auto_created_branch_arch14() {
    let home = p778_tmp_home("arch14-branch-rollback-new");
    let parent = p778_tmp_home("arch14-branch-rollback-new-src");
    let source = p780_setup_source_broken_origin(&parent);
    let agent = "arch14-rollback-new-agent";
    let branch = "feat/arch14-rollback-new";
    let target = p780_checkout_target(&home, agent, &source);
    std::fs::create_dir_all(&target).expect("occupied checkout target");
    let keep = target.join("KEEP.txt");
    std::fs::write(&keep, "legacy dirty worktree state").expect("preserved target state");

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": branch,
            "bind": true,
        }),
        agent,
    );

    assert_eq!(resp["code"].as_str(), Some("worktree_add_failed"), "{resp}");
    assert_eq!(resp["auto_created_branch"].as_bool(), Some(true), "{resp}");
    assert!(
        !p780_branch_exists(&source, branch),
        "a branch created by this failed checkout must be rolled back: {resp}"
    );
    assert!(
        keep.exists(),
        "the pre-existing occupied target and its dirty state must be preserved"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

/// Arch14 guard: the same worktree-add failure must never delete a branch that
/// existed before this checkout transaction began.
#[test]
#[cfg(unix)]
fn checkout_bind_true_worktree_add_failure_preserves_preexisting_branch_arch14() {
    let home = p778_tmp_home("arch14-branch-rollback-existing");
    let parent = p778_tmp_home("arch14-branch-rollback-existing-src");
    let source = p780_setup_source_broken_origin(&parent);
    let agent = "arch14-rollback-existing-agent";
    let branch = "feat/arch14-rollback-existing";
    let create = std::process::Command::new("git")
        .args(["branch", branch, "main"])
        .current_dir(&source)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("create pre-existing branch");
    assert!(create.status.success(), "create branch: {:?}", create);
    let target = p780_checkout_target(&home, agent, &source);
    std::fs::create_dir_all(&target).expect("occupied checkout target");
    let keep = target.join("KEEP.txt");
    std::fs::write(&keep, "legacy dirty worktree state").expect("preserved target state");

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": branch,
            "bind": true,
        }),
        agent,
    );

    assert_eq!(resp["code"].as_str(), Some("worktree_add_failed"), "{resp}");
    assert_eq!(resp["auto_created_branch"].as_bool(), Some(false), "{resp}");
    assert!(
        p780_branch_exists(&source, branch),
        "a pre-existing branch must survive a failed checkout: {resp}"
    );
    assert!(keep.exists(), "the occupied target must be preserved");

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn checkout_bind_true_invalid_from_ref_returns_structured_error_with_stage() {
    // Error surface pin: when `from_ref` is unresolvable BOTH locally
    // and after a fetch, the response must carry the canonical code
    // enum + stage + fetch_attempted + raw fields per decision
    // d-20260514102305998399-0. The fixture's broken origin URL
    // guarantees `git fetch origin` fails fast so the test doesn't hit
    // the network.
    let home = p778_tmp_home("780-bad-ref");
    let parent = p778_tmp_home("780-bad-ref-src");
    let source = p780_setup_source_broken_origin(&parent);
    let agent = "p780-agent-bad-ref";

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/p780-bad",
            "bind": true,
            // Unresolvable: this remote ref does not exist locally and
            // the broken origin URL prevents fetch from populating it.
            "from_ref": "origin/totally-bogus-ref-name",
        }),
        agent,
    );

    assert!(
        resp.get("error").is_some(),
        "unresolvable from_ref must error: {resp}"
    );
    // Stage must be one of the auto-create pipeline stages — either
    // `fetch` (fetch itself failed) or `retry_create` (fetch succeeded
    // but ref still missing). Both are valid endpoints; the broken
    // origin URL deterministically lands on `fetch` for this fixture.
    let stage = resp["stage"].as_str().unwrap_or_default();
    assert!(
        stage == "fetch" || stage == "retry_create",
        "stage must be fetch or retry_create, got: {resp}"
    );
    let code = resp["code"].as_str().unwrap_or_default();
    assert!(
        code == "fetch_failed" || code == "invalid_from_ref",
        "code must be fetch_failed or invalid_from_ref, got: {resp}"
    );
    assert_eq!(
        resp["fetch_attempted"].as_bool(),
        Some(true),
        "fetch_attempted must be true after fallback path entered: {resp}"
    );
    assert!(
        resp["raw"].as_str().is_some() && !resp["raw"].as_str().unwrap().is_empty(),
        "raw stderr must be surfaced for debug: {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn checkout_bind_true_concurrent_branch_create_race_idempotent() {
    // Race semantic pin: two concurrent callers on the SAME source repo
    // + SAME branch must not both error out at the `git branch` stage.
    // The winner sees `auto_created_branch=true`. The loser hits the
    // `already exists` stderr and falls through idempotently to the
    // worktree-add stage — where it will fail with
    // `code=worktree_add_failed` (different `instance_name` → different
    // worktree path, but same branch ref → git refuses second
    // checkout). The fall-through invariant we pin: NEITHER caller
    // returns `code=branch_create_failed`. Barrier(2) makes the race
    // deterministic without timing-dependent sleeps.
    let home = p778_tmp_home("780-race");
    let parent = p778_tmp_home("780-race-src");
    let source = p780_setup_source_no_feature_branch(&parent);

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for i in 0..2 {
        let barrier = std::sync::Arc::clone(&barrier);
        let home_c = home.clone();
        let source_c = source.clone();
        // fire-and-forget: test-only race harness; JoinHandle stored
        // in `handles` and explicitly joined below — not a long-lived
        // spawn site requiring supervisor wiring.
        handles.push(std::thread::spawn(move || {
            let agent = format!("p780-agent-race-{i}");
            barrier.wait();
            super::handle_checkout_repo(
                &home_c,
                &serde_json::json!({
                    "repository_path": source_c.display().to_string(),
                    "branch": "feat/p780-race",
                    "bind": true,
                }),
                &agent,
            )
        }));
    }
    let results: Vec<serde_json::Value> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Neither caller may surface `branch_create_failed` — the race must
    // be absorbed by the idempotent `already exists` fall-through.
    for r in &results {
        let code = r["code"].as_str().unwrap_or_default();
        assert_ne!(
            code, "branch_create_failed",
            "race must fall through, never error at branch create: {r}"
        );
    }
    // #1897: the robust idempotency invariant is "the branch is created exactly
    // once + no double-bind", NOT "exactly one caller's auto_created_branch flag
    // is true". The prior `winners == 1` was RACY (~47% flaky under real system
    // git, on baseline — NOT introduced by the #1897 git-timeout change): the
    // caller that CREATES the branch can then LOSE the bind-lease race and return
    // `cross_agent_conflict` (auto_created_branch not surfaced in a conflict
    // result), while the other caller sees the branch already exists and binds
    // with auto_created_branch=false → a perfectly idempotent outcome with ZERO
    // `auto_created_branch` flags. So:
    //  (1) the branch must exist afterward (created exactly once, not lost), and
    //  (2) at most one caller may hold the bind (no double-bind corruption).
    let branch_exists = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .current_dir(&source)
        .args(["rev-parse", "--verify", "refs/heads/feat/p780-race"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        branch_exists,
        "the race must create the branch exactly once: {results:?}"
    );
    let bound = results
        .iter()
        .filter(|r| r["bound"].as_bool() == Some(true))
        .count();
    assert!(
        bound <= 1,
        "concurrent bind must not double-bind the same branch: {results:?}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn checkout_bind_true_stress_50_iter_branch_create_no_flaky_parse() {
    // Stress pin: 50 sequential fresh-repo iterations exercising the
    // auto-create path. Catches:
    //   1. Flaky stderr-matching from git version / locale variation
    //      (we match on substring "not a valid object name" /
    //      "already exists" — if a future git rewords these the test
    //      surfaces the parse drift here, not in production).
    //   2. Resource / fd leaks from repeated subprocess spawns.
    //   3. Timing-dependent ordering issues in
    //      rev-parse → branch → worktree-add.
    // Each iter rebuilds the source repo from scratch, so the auto-
    // create path is deterministically exercised. Runtime expectation:
    // ~50ms × 50 ≈ 2.5s on a typical dev machine.
    let parent = p778_tmp_home("780-stress-src");
    for i in 0..50 {
        let home = p778_tmp_home(&format!("780-stress-{i}"));
        let source = p780_setup_source_no_feature_branch(&parent.join(format!("iter-{i}")));
        let agent = format!("p780-agent-stress-{i}");

        let resp = super::handle_checkout_repo(
            &home,
            &serde_json::json!({
                "repository_path": source.display().to_string(),
                "branch": format!("feat/p780-stress-{i}"),
                "bind": true,
            }),
            &agent,
        );

        assert!(
            resp.get("error").is_none(),
            "iter {i}: auto-create must succeed every time, got: {resp}"
        );
        assert_eq!(
            resp["auto_created_branch"].as_bool(),
            Some(true),
            "iter {i}: every iter creates a fresh branch: {resp}"
        );
        assert_eq!(
            resp["fetch_attempted"].as_bool(),
            Some(false),
            "iter {i}: fixture has origin/main locally — no fetch should fire: {resp}"
        );

        std::fs::remove_dir_all(&home).ok();
    }
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn checkout_bind_true_auto_create_path_preserves_779_tail_ops() {
    // Regression guard: when the auto-create path entered, ALL of
    // #779's tail-ops (marker write, binding.json, ci_watches arming)
    // must still fire. This is the property
    // `checkout_bind_true_writes_binding_marker_and_arms_watch` pinned
    // for the pre-existing-branch case; #780 introduces a new code path
    // that easily regresses tail-ops if the auto-create logic
    // accidentally short-circuits the post-worktree-add block.
    let home = p778_tmp_home("780-tail");
    let parent = p778_tmp_home("780-tail-src");
    let source = p780_setup_source_no_feature_branch(&parent);
    let agent = "p780-agent-tail";

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/p780-tail",
            "bind": true,
        }),
        agent,
    );

    assert!(resp.get("error").is_none(), "checkout must succeed: {resp}");
    assert_eq!(resp["auto_created_branch"].as_bool(), Some(true));

    let wt_path = std::path::PathBuf::from(resp["path"].as_str().expect("path"));
    assert!(
        wt_path.join(crate::worktree_pool::MANAGED_MARKER).exists(),
        ".agend-managed marker must be written on auto-create path"
    );

    let binding = crate::paths::runtime_dir(&home)
        .join(agent)
        .join("binding.json");
    assert!(
        binding.exists(),
        "binding.json must be written on auto-create path"
    );
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&binding).unwrap()).unwrap();
    assert_eq!(v["branch"].as_str(), Some("feat/p780-tail"));
    assert_eq!(v["task_id"].as_str(), Some(""));

    // #2158 GR1: the self-claim path (including the auto-create branch case) no
    // longer auto-arms ci_watch — no sidecar for the derived repo+branch.
    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("owner/repo", "feat/p780-tail"),
    );
    assert!(
        !watch_path.exists(),
        "#2158 GR1: self-claim auto-create path must NOT auto-arm ci_watch"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn checkout_bind_false_does_not_auto_create() {
    // Scope invariant pin: #780 auto-create is gated on `bind:true`.
    // The `bind:false` review-pool / operator-triage path must NOT
    // auto-create a missing branch — preserves the existing
    // fail-loud-on-missing-ref semantics for inspection-only callers.
    let home = p778_tmp_home("780-bind-false");
    let parent = p778_tmp_home("780-bind-false-src");
    let source = p780_setup_source_no_feature_branch(&parent);
    let agent = "p780-agent-bind-false";

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/p780-bind-false",
            // bind defaulting to false — explicit for test clarity.
            "bind": false,
        }),
        agent,
    );

    assert!(
        resp.get("error").is_some(),
        "bind:false missing branch must surface error (no auto-create): {resp}"
    );
    // No auto-create response fields on the bind:false path.
    assert!(
        resp.get("auto_created_branch").is_none(),
        "bind:false must NOT expose auto_created_branch: {resp}"
    );
    // Confirm the branch was NOT actually created in the source repo.
    let probe = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "refs/heads/feat/p780-bind-false"])
        .current_dir(&source)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git rev-parse");
    assert!(
        !probe.status.success(),
        "bind:false must not write any ref into source repo"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

// ----------------------------------------------------------------------
// #779 P2 (Option B) — partial-failure surfacing for handle_checkout_repo
// bind:true tail-ops + handle_watch_ci self-error surface.
//
// Source of truth: decision d-20260514142300613621-0.
//
// Empirical anchor (§3.10): comment out the warnings-collection block
// in handle_checkout_repo OR revert the handle_watch_ci hardening at
// site A / B → both anchor tests below fail. C2 commits these tests
// red; C3 makes them green.
//
// Cross-platform: all happy-path tests `#[cfg(unix)]` per §3.7.
// ----------------------------------------------------------------------

#[test]
#[cfg(unix)]
fn checkout_bind_true_bind_full_failure_surfaces_warning() {
    // ANCHOR (§3.10 red→green) — C2 red, C3 green.
    //
    // Injection: pre-create `<home>/runtime/<agent>` as a regular FILE
    // (not directory). `bind_full`'s `std::fs::create_dir_all(&dir)`
    // fails with "not a directory" → `Err("create_dir_all ...")`.
    // handle_checkout_repo's warning-collection logic (added in C3)
    // captures the Err and pushes "bind_full: ..." onto the warnings
    // vec. `bound: true` must still hold because lease succeeded —
    // tail-op degradation does not poison main success.
    let home = p778_tmp_home("779p2-bind-fail");
    let parent = p778_tmp_home("779p2-bind-fail-src");
    let source = p778_setup_source_repo(&parent, "feat/p779p2-bind");
    let agent = "p779p2-agent-bind";

    // Block bind_full by pre-creating runtime/<agent> as a regular file.
    let runtime = crate::paths::runtime_dir(&home);
    std::fs::create_dir_all(&runtime).ok();
    std::fs::write(runtime.join(agent), "blocking file (not a dir)").unwrap();

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/p779p2-bind",
            "bind": true,
        }),
        agent,
    );

    // #1310: bind_full failure now triggers worktree rollback — response
    // should be an error with code "bind_rollback", not a success with warnings.
    assert!(
        resp["error"].as_str().is_some(),
        "bind_full failure must return error after rollback: {resp}"
    );
    assert_eq!(
        resp["code"].as_str(),
        Some("bind_rollback"),
        "error code must be bind_rollback: {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn handle_watch_ci_atomic_write_failure_returns_error_field() {
    // ANCHOR (§3.10 red→green) — C2 red, C3 green.
    //
    // Direct test of handle_watch_ci's NEW error surface (Piece 3
    // hardening sites A + B). Independent of handle_checkout_repo
    // wrapper — pins the contract that ci_watches dir-create / atomic-
    // write failures become observable in the Value response as
    // `error` + `code`, not silently swallowed.
    //
    // Injection: pre-create `<home>/ci-watches` as a regular FILE.
    // handle_watch_ci's `std::fs::create_dir_all(&ci_dir)` (site A)
    // fails because the path is already a file. Post-C3 returns
    // `{error, code: "ci_watches_dir_create_failed"}`. Pre-C3 swallows
    // the error and returns success-shape — test FAILS.
    let home = p778_tmp_home("779p2-watch-fail");
    std::fs::create_dir_all(&home).ok();

    // Block ci-watches dir create by pre-creating the path as a file.
    let ci_watches = crate::daemon::ci_watch::ci_watches_dir(&home);
    if let Some(parent) = ci_watches.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&ci_watches, "blocking file (not a dir)").unwrap();

    let resp = super::handle_watch_ci(
        &home,
        &serde_json::json!({"repository": "owner/repo", "branch": "feat/p779p2-watch"}),
        "p779p2-agent-watch",
    );

    assert!(
        resp.get("error").is_some(),
        "handle_watch_ci must surface error when ci-watches dir create fails: {resp}"
    );
    let code = resp["code"].as_str().unwrap_or_default();
    assert!(
        code == "ci_watches_dir_create_failed" || code == "watch_write_failed",
        "code must be one of the canonical Piece-3 hardening codes, got '{code}': {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
}

// #2158 GR1: `checkout_bind_true_watch_ci_failure_surfaces_warning` was DELETED
// here — it injected a ci-watches-dir failure and asserted the self-claim checkout
// surfaced a `watch_ci:` warning. GR1 removes the self-claim watch auto-arm entirely,
// so there is no watch_ci call to fail and no such warning. (The dispatch path's
// watch-arm error handling is covered by dispatch_hook's `auto_watch_arm_error` test.)

#[test]
#[cfg(unix)]
fn checkout_bind_true_no_failures_no_warnings_field() {
    // Test 4 (back-compat invariant): clean fixture, no injection.
    // All tail-ops succeed → `warnings` field MUST be absent (omitted)
    // from the response. Pre-#779-P2 callers checking only `bound`/
    // `error` keys see no payload change.
    let home = p778_tmp_home("779p2-clean");
    let parent = p778_tmp_home("779p2-clean-src");
    let source = p778_setup_source_repo(&parent, "feat/p779p2-clean");
    let agent = "p779p2-agent-clean";

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/p779p2-clean",
            "bind": true,
        }),
        agent,
    );

    assert!(
        resp.get("error").is_none(),
        "clean path must not error: {resp}"
    );
    assert_eq!(resp["bound"].as_bool(), Some(true));
    assert!(
        resp.get("warnings").is_none(),
        "no failures → no `warnings` field (back-compat invariant): {resp}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

// ----------------------------------------------------------------------
// #789 — `repo action=cleanup_init_commits` MCP tool + signature contract
// tests. Pairs with the §3.10 anchor in `tasks::tests` which verifies
// `task action=done` triggers cleanup.
//
// Source of truth: decision d-20260514172825962581-5.
// Cross-platform per reviewer C6 — pure git subprocess + fixture I/O;
// happy-path tests `#[cfg(unix)]` to match #780/#781 fixture convention
// for git-subprocess concurrency on CI.
// ----------------------------------------------------------------------

#[cfg(unix)]
fn p789_setup_worktree_with_empty_inits(
    home: &std::path::Path,
    agent: &str,
    n_empty: usize,
) -> std::path::PathBuf {
    let worktree = home.join("worktree");
    std::fs::create_dir_all(&worktree).ok();
    let bypass = ("AGEND_GIT_BYPASS", "1");
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&worktree)
        .env(bypass.0, bypass.1)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ])
        .current_dir(&worktree)
        .env(bypass.0, bypass.1)
        .output()
        .unwrap();
    let head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&worktree)
        .env(bypass.0, bypass.1)
        .output()
        .unwrap();
    let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
    std::process::Command::new("git")
        .args(["update-ref", "refs/remotes/origin/main", &sha])
        .current_dir(&worktree)
        .env(bypass.0, bypass.1)
        .output()
        .unwrap();
    for _ in 0..n_empty {
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(&worktree)
            .env(bypass.0, bypass.1)
            .output()
            .unwrap();
    }
    let runtime = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&runtime).ok();
    std::fs::write(
        runtime.join("binding.json"),
        serde_json::to_string(&serde_json::json!({
            "version": 1,
            "agent": agent,
            "task_id": "T-1",
            "branch": "feat/p789",
            "worktree": worktree.display().to_string(),
            "source_repo": worktree.display().to_string(),
            "issued_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap(),
    )
    .unwrap();
    worktree
}

#[cfg(unix)]
fn p789_count_commits_origin_main_head(worktree: &std::path::Path) -> usize {
    let out = std::process::Command::new("git")
        .args(["log", "origin/main..HEAD", "--format=%H"])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().count()
}

#[test]
#[cfg(unix)]
fn cleanup_init_commits_mcp_removes_empty_inits_created_after_bind() {
    // Test 2: explicit MCP entry. 3 empty inits → MCP cleans → 0.
    let home = p778_tmp_home("789-mcp-removes");
    let worktree = p789_setup_worktree_with_empty_inits(&home, "dev", 3);
    assert_eq!(p789_count_commits_origin_main_head(&worktree), 3);

    let resp = super::handle_cleanup_init_commits(
        &home,
        &serde_json::json!({"instance": "dev"}),
        "operator",
    );
    assert!(resp.get("error").is_none(), "must succeed: {resp}");
    assert_eq!(
        resp["cleaned_count"].as_u64(),
        Some(3),
        "must report 3 cleaned: {resp}"
    );
    assert_eq!(
        p789_count_commits_origin_main_head(&worktree),
        0,
        "post-MCP, no commits between origin/main..HEAD"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[cfg(unix)]
fn cleanup_preserves_non_empty_commits_with_msg_init() {
    // Test 3 (back-compat invariant): `init`-named commit with actual
    // file changes (real impl work, just badly named) MUST NOT be
    // touched. Defense-in-depth against unusual commit conventions.
    let home = p778_tmp_home("789-preserves-nonempty");
    let worktree = p789_setup_worktree_with_empty_inits(&home, "dev", 0);
    let bypass = ("AGEND_GIT_BYPASS", "1");
    std::fs::write(worktree.join("README.md"), "hello").unwrap();
    std::process::Command::new("git")
        .args(["add", "README.md"])
        .current_dir(&worktree)
        .env(bypass.0, bypass.1)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "init",
        ])
        .current_dir(&worktree)
        .env(bypass.0, bypass.1)
        .output()
        .unwrap();
    assert_eq!(p789_count_commits_origin_main_head(&worktree), 1);

    let resp = super::handle_cleanup_init_commits(
        &home,
        &serde_json::json!({"instance": "dev"}),
        "operator",
    );
    assert_eq!(
        resp["cleaned_count"].as_u64(),
        Some(0),
        "non-empty must NOT be cleaned: {resp}"
    );
    assert_eq!(
        p789_count_commits_origin_main_head(&worktree),
        1,
        "non-empty `init` commit survives cleanup (back-compat invariant)"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[cfg(unix)]
fn cleanup_handles_17_burst_pattern_from_pr_781() {
    // Test 4 (stress, reviewer constraint 3): exact PR #781 scenario
    // — 17 contiguous empty `init` commits → single soft-reset cleanly
    // removes all 17.
    let home = p778_tmp_home("789-17-burst");
    let worktree = p789_setup_worktree_with_empty_inits(&home, "dev", 17);
    assert_eq!(p789_count_commits_origin_main_head(&worktree), 17);

    let resp = super::handle_cleanup_init_commits(
        &home,
        &serde_json::json!({"instance": "dev"}),
        "operator",
    );
    assert_eq!(
        resp["cleaned_count"].as_u64(),
        Some(17),
        "all 17 cleaned in single invocation: {resp}"
    );
    assert_eq!(p789_count_commits_origin_main_head(&worktree), 0);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn cleanup_with_invalid_worktree_returns_error_not_silent() {
    // Test 5 (observable failure, reviewer constraint 4): when binding
    // points to a non-existent worktree path, the helper's git log
    // fails. The MCP response surfaces `error` + `code=cleanup_failed`
    // rather than silently returning cleaned_count=0.
    let home = std::env::temp_dir().join(format!("agend-p789-invalid-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let runtime = crate::paths::runtime_dir(&home).join("dev");
    std::fs::create_dir_all(&runtime).ok();
    std::fs::write(
        runtime.join("binding.json"),
        serde_json::to_string(&serde_json::json!({
            "version": 1,
            "agent": "dev",
            "task_id": "T-1",
            "branch": "feat/x",
            "worktree": "/var/folders/non-existent-p789-worktree-path",
            "source_repo": "/var/folders/non-existent-p789-worktree-path",
            "issued_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap(),
    )
    .unwrap();

    let resp = super::handle_cleanup_init_commits(
        &home,
        &serde_json::json!({"instance": "dev"}),
        "operator",
    );
    assert!(
        resp.get("error").is_some(),
        "invalid worktree path must surface error (NOT silent noop): {resp}"
    );
    assert_eq!(
        resp["code"].as_str(),
        Some("cleanup_failed"),
        "code must mark cleanup_failed class: {resp}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn cleanup_on_clean_worktree_is_noop_count_zero() {
    // Test 6 (idempotent): no binding → MCP returns cleaned_count=0
    // with explicit skipped_reason — distinguishes the no-binding case
    // from "successfully cleaned 0 commits".
    let home = std::env::temp_dir().join(format!("agend-p789-noop-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let resp = super::handle_cleanup_init_commits(
        &home,
        &serde_json::json!({"instance": "ghost-agent"}),
        "operator",
    );
    assert_eq!(resp["cleaned_count"].as_u64(), Some(0));
    let reason = resp["skipped_reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("no binding"),
        "no-binding skip reason must be explicit: {resp}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #942 explicit watch + auto-bind no-split end-to-end ──
//
// Pre-#942 bug: caller could pass `repo: "owner/repo.git"` to
// `handle_watch_ci` and the auto-derived form (from
// `derive_repo_from_remote`) would produce `owner/repo` → two
// distinct watch files (different hashes), fragmented subscribers,
// duplicate notifications.
//
// Post-fix: both paths converge on canonical `owner/repo` so the same
// ci_watches file holds both subscribers.

#[test]
fn handle_watch_ci_canonicalizes_caller_supplied_repo_with_git_suffix() {
    let home = p778_tmp_home("942-git-suffix");
    let r = handle_watch_ci(
        &home,
        &serde_json::json!({"repository": "owner/repo.git", "branch": "feat/x"}),
        "dev",
    );
    assert!(
        r["watching"].as_bool().unwrap_or(false),
        "watching must succeed for `.git` form: {r}"
    );

    // File must be at canonical sha256 path.
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
    let canonical_filename = crate::daemon::ci_watch::watch_filename("owner/repo", "feat/x");
    let canonical_path = ci_dir.join(&canonical_filename);
    assert!(
        canonical_path.exists(),
        "watch file must land at canonical sha256 path"
    );

    // Body's `repo` field is canonical (not `.git`-suffixed).
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&canonical_path).unwrap()).unwrap();
    assert_eq!(v["repo"].as_str(), Some("owner/repo"));
    assert_eq!(v["branch"].as_str(), Some("feat/x"));
}

#[test]
fn handle_watch_ci_two_callers_with_different_forms_share_one_watch_file() {
    let home = p778_tmp_home("942-converge");
    let _ = handle_watch_ci(
        &home,
        &serde_json::json!({"repository": "owner/repo.git", "branch": "feat/x"}),
        "agent-a",
    );
    let _ = handle_watch_ci(
        &home,
        &serde_json::json!({"repository": "Owner/Repo", "branch": "feat/x"}),
        "agent-b",
    );

    // Single canonical file post-canonicalization.
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
    let files: Vec<_> = std::fs::read_dir(&ci_dir)
        .unwrap()
        .flatten()
        .filter(|e| {
            e.path().extension().and_then(|s| s.to_str()) == Some("json")
                && e.path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|stem| stem.len() == 64)
        })
        .collect();
    assert_eq!(
        files.len(),
        1,
        "two callers with different repo forms must converge to ONE canonical file: {:?}",
        files.iter().map(|f| f.path()).collect::<Vec<_>>()
    );

    // Both agents in subscribers.
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(files[0].path()).unwrap()).unwrap();
    let subs = crate::daemon::ci_watch::parse_subscribers(&v);
    assert!(subs.contains(&"agent-a".to_string()), "agent-a subscribed");
    assert!(subs.contains(&"agent-b".to_string()), "agent-b subscribed");
}

#[test]
fn handle_watch_ci_rejects_invalid_repo_format() {
    let home = p778_tmp_home("942-invalid");
    // Non-GitHub URL — canonicalize_repo_slug returns None
    let r = handle_watch_ci(
        &home,
        &serde_json::json!({"repository": "https://gitlab.com/owner/repo", "branch": "feat/x"}),
        "dev",
    );
    assert_eq!(
        r["code"].as_str(),
        Some("invalid_repo_format"),
        "GitLab URL must be rejected as invalid_repo_format: {r}"
    );
}

// ----- #1244 PR-B: merge preflight CI gate -----

/// Minimal ScmProvider stub yielding a stable (head, base) so the P0 exact-head
/// acquisition passes, letting the force-audit tests below reach the audit path
/// they exercise. The force path returns at the audit-write failure BEFORE any
/// merge/recheck, so only `pr_view` is ever called here. #merge-exact-head r1:
/// the OIDs MUST be valid FULL 40-hex (`is_full_commit_sha`) — the pre-r1 stub
/// used non-hex `"h".repeat(40)`, which the tightened identity invariant now
/// (correctly) fails closed on.
struct MergeHeadBaseStub;
impl crate::scm::ScmProvider for MergeHeadBaseStub {
    fn pr_view(&self, _r: &str, _p: u64, _f: &[&str]) -> anyhow::Result<crate::scm::PrSummary> {
        Ok(crate::scm::PrSummary {
            head_ref_oid: Some("a".repeat(40)),
            base_ref_oid: Some("b".repeat(40)),
            ..Default::default()
        })
    }
    fn pr_checks(&self, _r: &str, _p: u64) -> anyhow::Result<Vec<crate::scm::CheckState>> {
        unimplemented!("force path fails at audit before pr_checks")
    }
    fn pr_list(
        &self,
        _r: &str,
        _f: &crate::scm::ListFilter,
        _fl: &[&str],
        _c: Option<&std::path::Path>,
    ) -> anyhow::Result<Vec<crate::scm::PrSummary>> {
        unimplemented!()
    }
    fn pr_merge(
        &self,
        _r: &str,
        _p: u64,
        _o: &crate::scm::MergeOpts,
    ) -> anyhow::Result<crate::scm::MergeOutcome> {
        unimplemented!("force path fails at audit before pr_merge")
    }
    fn issue_view(
        &self,
        _r: &str,
        _n: u64,
        _f: &[&str],
    ) -> anyhow::Result<crate::scm::IssueSummary> {
        unimplemented!()
    }
    fn compare(&self, _r: &str, _b: &str, _h: &str) -> anyhow::Result<crate::scm::CompareResult> {
        unimplemented!()
    }
}

#[test]
fn merge_missing_pr_returns_error() {
    let home = std::env::temp_dir().join(format!("agend-merge-test-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let result = super::handle_merge_repo(&home, &json!({}), "dev");
    assert!(result["error"].as_str().unwrap().contains("pr"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn merge_force_without_reason_returns_error() {
    let home = std::env::temp_dir().join(format!("agend-merge-force-test-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    // #1619: explicit `repository` so resolution succeeds and the test
    // reaches the force/force_reason guard it's actually exercising.
    let result = super::handle_merge_repo(
        &home,
        &json!({"pr": 1234, "force": true, "repository": "suzuke/agend-terminal"}),
        "dev",
    );
    assert!(result["error"].as_str().unwrap().contains("force_reason"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn merge_force_audit_write_failure_refuses_merge() {
    let home = std::env::temp_dir().join(format!("agend-merge-audit-fail-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let events_path = home.join("fleet_events.jsonl");
    std::fs::create_dir_all(&events_path).unwrap();

    // P0: the exact-head acquisition now runs before the audit — stub a reachable
    // head+base so the test still reaches the force-audit failure it exercises.
    let _g = crate::scm::set_test_scm_provider(std::sync::Arc::new(MergeHeadBaseStub));
    let result = super::handle_merge_repo(
        &home,
        // #1619: explicit `repository` so resolution succeeds and the
        // test reaches the force-path audit write it's exercising.
        &json!({"pr": 9999, "force": true, "force_reason": "test emergency", "repository": "suzuke/agend-terminal"}),
        "dev",
    );
    let err = result["error"].as_str().unwrap();
    assert!(
        err.contains("audit log write failed"),
        "expected audit failure error, got: {err}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #2539: `handle_tool_with_runtime` reads `AGEND_HOME` via `crate::home_dir()`
/// (no explicit `home` param, unlike `handle_merge_repo`), so exercising the
/// full dispatch chain needs the env var set process-wide. Serializes tests
/// that mutate it — same hazard/pattern as `mcp::handlers::tests::fleet_test_guard`.
fn ci_env_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GUARD.lock().unwrap_or_else(|e| e.into_inner())
}

/// #2539 confidence pin (§3.9 real-entry-point): the test above calls
/// `handle_merge_repo` directly, which is a mid-pipeline inject — it cannot
/// catch a schema-declaration gap because the daemon-internal dispatch never
/// filters by schema. This variant drives the SAME force-bypass scenario
/// through the full internal dispatch chain (`handle_tool` → `try_dispatch`
/// → `ci::handle_merge_repo`), confirming `force`/`force_reason` survive the
/// whole daemon-side path intact. It is NOT a RED test for #2539 (it passes
/// even without the `def_repo` schema fix, since that gap lives entirely at
/// the external MCP-client boundary this test cannot reach) — it documents
/// that the daemon-internal half of the chain was never the problem.
#[test]
fn merge_force_reaches_handler_through_full_dispatch_chain_2539() {
    let _g = ci_env_test_guard();
    let home = std::env::temp_dir().join(format!("agend-merge-force-chain-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let events_path = home.join("fleet_events.jsonl");
    std::fs::create_dir_all(&events_path).unwrap();
    let prev_home = std::env::var("AGEND_HOME").ok();
    std::env::set_var("AGEND_HOME", &home);

    // P0: stub a reachable head+base so the exact-head acquisition passes and the
    // test still reaches the force-audit failure (thread-local, same thread as
    // handle_tool).
    let _g = crate::scm::set_test_scm_provider(std::sync::Arc::new(MergeHeadBaseStub));
    let result = crate::mcp::handlers::handle_tool(
        "repo",
        &json!({
            "action": "merge",
            "pr": 9999,
            "force": true,
            "force_reason": "test emergency",
            "repository": "suzuke/agend-terminal",
        }),
        "dev",
    );

    match prev_home {
        Some(h) => std::env::set_var("AGEND_HOME", h),
        None => std::env::remove_var("AGEND_HOME"),
    }

    let err = result["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("audit log write failed"),
        "#2539: force/force_reason must survive the full handle_tool dispatch chain \
         unmodified (same audit-write reject as the handler-direct test), got: {result}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #1619: a merge with neither an explicit `repository` arg nor an
/// active binding must FAIL LOUD — never silently fall back to a
/// hardcoded maintainer repo (the old `.unwrap_or("suzuke/agend-terminal")`
/// bug, which would have mis-targeted merge/checks/state on someone
/// else's deployment).
#[test]
fn merge_no_repo_no_binding_errors_without_hardcoded_fallback() {
    let home = std::env::temp_dir().join(format!("agend-merge-norepo-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let result = super::handle_merge_repo(&home, &json!({"pr": 4242}), "dev");
    let err = result["error"]
        .as_str()
        .expect("expected an error, not a merge");
    assert_eq!(
        result["code"].as_str(),
        Some("no_binding_no_repo"),
        "no repo + no binding must surface no_binding_no_repo, got: {result}"
    );
    assert!(
        !err.contains("suzuke/agend-terminal"),
        "error must NOT leak / fall back to the maintainer repo slug, got: {err}"
    );
    // And it must not report a successful merge.
    assert!(result["merged"].as_bool() != Some(true));
    let _ = std::fs::remove_dir_all(&home);
}

/// #base-drift: the pure refusal decision — `BEHIND` (phantom-reversion) and
/// `DIRTY` (conflicts) refuse with a rebase hint; everything else
/// (CLEAN/UNSTABLE/BLOCKED/UNKNOWN/empty/...) proceeds (fail-open, since GitHub
/// may still be computing mergeability). force-bypass is structural — the gate
/// sits inside `handle_merge_repo`'s `if !force` block, so force skips it.
///
/// NOTE (per FLEET-DEV-PROTOCOL §3.9 real-entry discipline): the WIRING
/// (`handle_merge_repo` → `pr_view(["mergeStateStatus"])` → this decision, placed
/// after the CI-pass gate and before the merge) is gh-integration-only — there is
/// no `ScmProvider` test double — so it is verified by code-reading, not a unit
/// test. Only the pure decision is unit-tested here; flagged transparently rather
/// than hidden.
#[test]
fn base_drift_refusal_behind_and_dirty_refuse_else_proceed() {
    assert!(
        super::base_drift_refusal("BEHIND").is_some(),
        "BEHIND (base behind main) must refuse"
    );
    assert!(
        super::base_drift_refusal("DIRTY").is_some(),
        "DIRTY (conflicts) must refuse"
    );
    for ok in [
        "CLEAN",
        "UNSTABLE",
        "BLOCKED",
        "UNKNOWN",
        "",
        "HAS_HOOKS",
        "DRAFT",
    ] {
        assert!(
            super::base_drift_refusal(ok).is_none(),
            "{ok} must proceed (fail-open) — only BEHIND/DIRTY refuse"
        );
    }
    let (_why, hint) = super::base_drift_refusal("BEHIND").unwrap();
    assert!(
        hint.contains("rebase"),
        "BEHIND refusal must carry an actionable rebase hint, got: {hint}"
    );
}

/// #1447: `repo checkout` resolves the source from the cross-tool standard
/// `repository_path` arg. The legacy `source` / `source_repo` aliases (#1446)
/// were dropped in favor of the single canonical name.
#[test]
fn checkout_resolves_repository_path() {
    use serde_json::json;
    // Canonical `repository_path` resolves.
    assert_eq!(
        checkout_source(&json!({"repository_path": "/repo/b"})),
        Some("/repo/b")
    );
    // Empty `repository_path` → None.
    assert_eq!(checkout_source(&json!({"repository_path": ""})), None);
    // Dropped legacy aliases no longer resolve.
    assert_eq!(checkout_source(&json!({"source": "/repo/a"})), None);
    assert_eq!(checkout_source(&json!({"source_repo": "/repo/b"})), None);
    // Neither present → None (handler then returns the missing-arg error).
    assert_eq!(checkout_source(&json!({"branch": "feat-x"})), None);
}

// #1467: post-merge verification — `gh pr merge` exit 0 is not proof the PR
// landed; only state==MERGED + a non-empty merge commit oid confirms it.

#[test]
fn merge_view_confirmed_when_merged_with_commit() {
    // #PR-D: classify now takes the typed PrSummary (was a raw Value).
    let summary = crate::scm::PrSummary {
        state: Some("MERGED".into()),
        merge_commit_oid: Some("abc123def".into()),
        merge_state_status: Some("CLEAN".into()),
        merged_at: Some("2026-05-29T16:00:00Z".into()),
        ..Default::default()
    };
    match super::classify_merge_summary(&summary) {
        super::MergeVerdict::Confirmed(oid) => assert_eq!(oid, "abc123def"),
        super::MergeVerdict::Unconfirmed { state, .. } => {
            panic!("expected Confirmed, got Unconfirmed(state={state})")
        }
    }
}

#[test]
fn merge_view_unconfirmed_when_open_after_merge() {
    // The #1467 bug shape: `gh pr merge` exited 0 but the PR is still OPEN
    // (merge-queue / eventual consistency / branch-protection) — must NOT be
    // reported as merged. (mergeCommit null → merge_commit_oid None.)
    let summary = crate::scm::PrSummary {
        state: Some("OPEN".into()),
        merge_commit_oid: None,
        merge_state_status: Some("BLOCKED".into()),
        ..Default::default()
    };
    match super::classify_merge_summary(&summary) {
        super::MergeVerdict::Unconfirmed {
            state,
            merge_state_status,
        } => {
            assert_eq!(state, "OPEN");
            assert_eq!(merge_state_status, "BLOCKED");
        }
        super::MergeVerdict::Confirmed(oid) => {
            panic!("OPEN PR must not be Confirmed, got commit {oid}")
        }
    }
}

#[test]
fn merge_view_unconfirmed_when_merged_state_but_no_commit() {
    // Defensive: state says MERGED but no merge commit oid yet (race window) →
    // still unconfirmed; don't claim merged without the commit.
    let summary = crate::scm::PrSummary {
        state: Some("MERGED".into()),
        merge_commit_oid: None,
        merge_state_status: Some("UNKNOWN".into()),
        ..Default::default()
    };
    assert!(matches!(
        super::classify_merge_summary(&summary),
        super::MergeVerdict::Unconfirmed { .. }
    ));
}

// ── #1991: unwatch tombstone — explicit unwatch must STICK against PR-3 auto-arm ──

#[test]
fn unwatch_to_empty_leaves_tombstone_1991() {
    let home = watch_test_home("1991-tombstone");
    super::handle_watch_ci(
        &home,
        &serde_json::json!({"repository": "o/r", "branch": "feat/x"}),
        "dev-1",
    );
    let resp = super::handle_unwatch_ci(
        &home,
        &serde_json::json!({"repository": "o/r", "branch": "feat/x", "instance": "dev-1"}),
        "dev-1",
    );
    assert_eq!(resp["watching"], false);
    assert_eq!(resp["tombstone"], true);

    let path = watch_path_for(&home, "o/r", "feat/x");
    assert!(
        path.exists(),
        "#1991: the watch file must remain as a tombstone — deleting it lets \
         PR-3 auto-arm re-subscribe the agent that just unwatched"
    );
    let v = read_watch(&path);
    assert_eq!(v["auto_arm_optout"], true);
    assert!(
        crate::daemon::ci_watch::parse_subscribers(&v).is_empty(),
        "tombstone carries no subscribers"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn rewatch_clears_tombstone_optout_1991() {
    let home = watch_test_home("1991-rewatch");
    super::handle_watch_ci(
        &home,
        &serde_json::json!({"repository": "o/r", "branch": "feat/x"}),
        "dev-1",
    );
    super::handle_unwatch_ci(
        &home,
        &serde_json::json!({"repository": "o/r", "branch": "feat/x", "instance": "dev-1"}),
        "dev-1",
    );
    // Explicit re-watch: the human decision overrides the optout.
    super::handle_watch_ci(
        &home,
        &serde_json::json!({"repository": "o/r", "branch": "feat/x"}),
        "dev-2",
    );
    let v = read_watch(&watch_path_for(&home, "o/r", "feat/x"));
    assert!(
        v.get("auto_arm_optout").is_none(),
        "explicit re-watch must clear the optout: {v}"
    );
    assert_eq!(
        crate::daemon::ci_watch::parse_subscribers(&v),
        vec!["dev-2".to_string()]
    );
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// Arch-14 item 10: repo release canonical delegation (real dispatch entry)
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn managed_wt_fixture(
    tag: &str,
) -> (
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let base = release_guard_tmp(tag);
    let repo = base.join("source");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git_init(&repo);
    let wt = base.join("managed-wt");
    std::process::Command::new("git")
        .args(["worktree", "add", wt.to_str().unwrap(), "-b", "feat/test"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    let home = base.join("home");
    std::fs::create_dir_all(&home).ok();
    (base, home, repo, wt)
}

#[cfg(unix)]
fn git_init(repo: &std::path::Path) {
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
}

#[cfg(unix)]
fn seed_managed_marker(
    wt: &std::path::Path,
    repo: &std::path::Path,
    home: &std::path::Path,
    agent: &str,
    branch: &str,
) {
    std::fs::write(
        wt.join(".agend-managed"),
        format!(
            "agent={agent}\nbranch={branch}\nsource_repo={}\n",
            repo.display()
        ),
    )
    .expect("write marker");
    crate::binding::bind_full(home, agent, "", branch, wt, repo, false).expect("bind");
}

#[cfg(unix)]
fn dispatch_repo_release(home: &std::path::Path, caller: &str, path: &str) -> serde_json::Value {
    let args = serde_json::json!({"action": "release", "path": path});
    let sender: Option<crate::identity::Sender> = if caller.is_empty() {
        None
    } else {
        crate::identity::Sender::new(caller)
    };
    let ctx = crate::mcp::handlers::dispatch::HandlerCtx {
        home,
        args: &args,
        instance_name: sender.as_ref().map_or("", |s| s.as_str()),
        sender: &sender,
        runtime: None,
    };
    crate::mcp::handlers::dispatch::dispatch_repo(&ctx)
}

/// RED A1: managed worktree release must clear binding.
#[test]
#[cfg(unix)]
fn repo_release_managed_clears_binding_via_dispatch() {
    let (base, home, repo, wt) = managed_wt_fixture("a1");
    seed_managed_marker(&wt, &repo, &home, "agent-a1", "feat/test");
    assert!(
        crate::binding::read(&home, "agent-a1").is_some(),
        "precondition"
    );
    let _ = dispatch_repo_release(&home, "agent-a1", wt.to_str().unwrap());
    assert!(
        crate::binding::read(&home, "agent-a1").is_none(),
        "managed release must clear binding (RED: currently orphaned)"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// RED A3: corrupt marker must fail-closed — worktree preserved.
#[test]
#[cfg(unix)]
fn repo_release_corrupt_marker_refuses_via_dispatch() {
    let (base, home, _repo, wt) = managed_wt_fixture("a3");
    std::fs::write(wt.join(".agend-managed"), "corrupt\n").expect("corrupt");
    let _ = dispatch_repo_release(&home, "anyone", wt.to_str().unwrap());
    assert!(
        wt.exists(),
        "corrupt-marker managed worktree must be preserved (RED)"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// RED A5: stale marker naming agent-a while agent-a's binding is at
/// different live path → refuse, preserve live binding.
#[test]
#[cfg(unix)]
fn repo_release_stale_marker_preserves_live_binding() {
    let (base, home, repo, stale_wt) = managed_wt_fixture("a5");
    // Agent-a bound to a DIFFERENT live worktree.
    let live_wt = base.join("live-wt");
    std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            live_wt.to_str().unwrap(),
            "-b",
            "feat/live",
        ])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    seed_managed_marker(&live_wt, &repo, &home, "agent-a", "feat/live");
    // Stale worktree claims agent-a with full marker but binding points elsewhere.
    std::fs::write(
        stale_wt.join(".agend-managed"),
        format!(
            "agent=agent-a\nbranch=feat/test\nsource_repo={}\n",
            repo.display()
        ),
    )
    .expect("stale marker");

    let r = dispatch_repo_release(&home, "agent-a", stale_wt.to_str().unwrap());

    // Stale path must also be preserved (not deleted).
    assert!(
        stale_wt.exists(),
        "stale managed worktree must be preserved when binding mismatches (RED)"
    );
    assert!(
        r.get("error").is_some() || r.get("code").is_some(),
        "stale-path mismatch must return error/code (RED): {r}"
    );
    assert!(live_wt.exists(), "live worktree must survive stale release");
    let binding = crate::binding::read(&home, "agent-a");
    assert!(binding.is_some(), "binding must survive");
    assert_eq!(
        binding.unwrap()["worktree"].as_str().unwrap_or(""),
        live_wt.to_str().unwrap(),
        "binding must still point to live worktree"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// RED A2: dirty managed worktree → WIP preserved in recovery ref,
/// binding + worktree still released.
#[test]
#[cfg(unix)]
fn repo_release_dirty_managed_preserves_wip() {
    let (base, home, repo, wt) = managed_wt_fixture("a2");
    seed_managed_marker(&wt, &repo, &home, "agent-a2", "feat/test");
    // Seed tracked file + dirty it.
    std::fs::write(wt.join("tracked.txt"), "v1\n").expect("tracked");
    std::process::Command::new("git")
        .args(["add", "tracked.txt"])
        .current_dir(&wt)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "seed",
        ])
        .current_dir(&wt)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    std::fs::write(wt.join("tracked.txt"), "v1\nDIRTY\n").expect("dirty");

    let _r = dispatch_repo_release(&home, "agent-a2", wt.to_str().unwrap());

    // Binding must be cleared even for dirty release (WIP preserved first).
    assert!(
        crate::binding::read(&home, "agent-a2").is_none(),
        "dirty managed release must clear binding (RED)"
    );
    // Recovery ref must contain the DIRTY content.
    let refs = {
        let out = std::process::Command::new("git")
            .args([
                "for-each-ref",
                "--format=%(refname)",
                "refs/agend/recovery/feat/test/",
            ])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git for-each-ref");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    assert!(
        !refs.is_empty(),
        "dirty managed release must create a recovery ref (RED)"
    );
    let show = std::process::Command::new("git")
        .args(["show", &format!("{}:tracked.txt", refs[0])])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git show");
    assert!(
        String::from_utf8_lossy(&show.stdout).contains("DIRTY"),
        "recovery ref must contain the DIRTY modification (RED)"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// RED A4: non-owner peer caller is refused; owner succeeds.
#[test]
#[cfg(unix)]
fn repo_release_non_owner_refused_owner_succeeds() {
    let (base, home, repo, wt) = managed_wt_fixture("a4");
    seed_managed_marker(&wt, &repo, &home, "owner-agent", "feat/test");

    // Peer caller (not owner) → must be refused.
    let r = dispatch_repo_release(&home, "peer-agent", wt.to_str().unwrap());
    assert!(
        crate::binding::read(&home, "owner-agent").is_some(),
        "binding must survive peer attempt (RED)"
    );
    assert!(
        r.get("error").is_some(),
        "non-owner peer must be refused (RED): {r}"
    );
    assert!(
        wt.exists(),
        "worktree must survive non-owner release attempt (RED)"
    );

    // Owner caller → must succeed.
    let r = dispatch_repo_release(&home, "owner-agent", wt.to_str().unwrap());
    assert!(
        crate::binding::read(&home, "owner-agent").is_none(),
        "owner release must clear binding (RED)"
    );
    let _ = r; // response structure TBD
    std::fs::remove_dir_all(&base).ok();
}

/// Marker says branch=feat/other but binding has branch=feat/test → refuse,
/// preserve worktree + binding.
#[test]
#[cfg(unix)]
fn repo_release_marker_branch_mismatch_refuses() {
    let (base, home, repo, wt) = managed_wt_fixture("br-mm");
    // Bind normally (branch=feat/test from the worktree add).
    seed_managed_marker(&wt, &repo, &home, "agent-brmm", "feat/test");
    // Overwrite marker with a DIFFERENT branch — simulates stale/tampered marker.
    std::fs::write(
        wt.join(".agend-managed"),
        format!(
            "agent=agent-brmm\nbranch=feat/other\nsource_repo={}\n",
            repo.display()
        ),
    )
    .expect("overwrite marker");

    let r = dispatch_repo_release(&home, "agent-brmm", wt.to_str().unwrap());

    assert!(
        r.get("error").is_some(),
        "branch mismatch must return error: {r}"
    );
    assert!(wt.exists(), "worktree must be preserved on branch mismatch");
    assert!(
        crate::binding::read(&home, "agent-brmm").is_some(),
        "binding must be preserved on branch mismatch"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// Marker says source_repo=/other but binding has the real repo → refuse,
/// preserve worktree + binding.
#[test]
#[cfg(unix)]
fn repo_release_marker_source_mismatch_refuses() {
    let (base, home, repo, wt) = managed_wt_fixture("sr-mm");
    seed_managed_marker(&wt, &repo, &home, "agent-srmm", "feat/test");
    // Overwrite marker with a DIFFERENT source_repo.
    std::fs::write(
        wt.join(".agend-managed"),
        "agent=agent-srmm\nbranch=feat/test\nsource_repo=/bogus/repo\n",
    )
    .expect("overwrite marker");

    let r = dispatch_repo_release(&home, "agent-srmm", wt.to_str().unwrap());

    assert!(
        r.get("error").is_some(),
        "source mismatch must return error: {r}"
    );
    assert!(wt.exists(), "worktree must be preserved on source mismatch");
    assert!(
        crate::binding::read(&home, "agent-srmm").is_some(),
        "binding must be preserved on source mismatch"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// Marker rewritten AFTER pre-read but BEFORE canonical locks — proves
/// the under-lock fresh re-read catches it.
#[test]
#[cfg(unix)]
fn repo_release_marker_rewritten_after_snapshot_refuses() {
    use crate::worktree_pool::{release_test_seam, ReleaseTestPhase};

    let (base, home, repo, wt) = managed_wt_fixture("seam");
    seed_managed_marker(&wt, &repo, &home, "agent-seam", "feat/test");

    let wt_clone = wt.clone();
    let _guard = release_test_seam::install(move |phase| {
        if phase == ReleaseTestPhase::AfterBindingSnapshot {
            std::fs::write(
                wt_clone.join(".agend-managed"),
                "agent=agent-seam\nbranch=feat/tampered\nsource_repo=/tampered\n",
            )
            .expect("rewrite marker in seam");
        }
    });

    let r = dispatch_repo_release(&home, "agent-seam", wt.to_str().unwrap());

    assert!(
        r.get("error").is_some(),
        "marker rewritten after snapshot must be caught under lock: {r}"
    );
    assert!(
        wt.exists(),
        "worktree must be preserved when marker is tampered"
    );
    assert!(
        crate::binding::read(&home, "agent-seam").is_some(),
        "binding must be preserved when marker is tampered"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// Marker DELETED after pre-read — under-lock re-read must refuse.
#[test]
#[cfg(unix)]
fn repo_release_marker_deleted_after_snapshot_refuses() {
    use crate::worktree_pool::{release_test_seam, ReleaseTestPhase};

    let (base, home, repo, wt) = managed_wt_fixture("del");
    seed_managed_marker(&wt, &repo, &home, "agent-del", "feat/test");

    let wt_clone = wt.clone();
    let _guard = release_test_seam::install(move |phase| {
        if phase == ReleaseTestPhase::AfterBindingSnapshot {
            let _ = std::fs::remove_file(wt_clone.join(".agend-managed"));
        }
    });

    let r = dispatch_repo_release(&home, "agent-del", wt.to_str().unwrap());

    assert!(
        r.get("error").is_some(),
        "marker deleted after snapshot must be caught under lock: {r}"
    );
    assert!(
        wt.exists(),
        "worktree must be preserved when marker is deleted"
    );
    assert!(
        crate::binding::read(&home, "agent-del").is_some(),
        "binding must be preserved when marker is deleted"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// Marker rewritten with BLANK branch/source after pre-read — must refuse.
#[test]
#[cfg(unix)]
fn repo_release_marker_blanked_after_snapshot_refuses() {
    use crate::worktree_pool::{release_test_seam, ReleaseTestPhase};

    let (base, home, repo, wt) = managed_wt_fixture("blank");
    seed_managed_marker(&wt, &repo, &home, "agent-blank", "feat/test");

    let wt_clone = wt.clone();
    let _guard = release_test_seam::install(move |phase| {
        if phase == ReleaseTestPhase::AfterBindingSnapshot {
            std::fs::write(
                wt_clone.join(".agend-managed"),
                "agent=agent-blank\nbranch=\nsource_repo=\n",
            )
            .expect("blank marker in seam");
        }
    });

    let r = dispatch_repo_release(&home, "agent-blank", wt.to_str().unwrap());

    assert!(
        r.get("error").is_some(),
        "blanked marker after snapshot must be caught under lock: {r}"
    );
    assert!(
        wt.exists(),
        "worktree must be preserved when marker is blanked"
    );
    assert!(
        crate::binding::read(&home, "agent-blank").is_some(),
        "binding must be preserved when marker is blanked"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// Both binding and marker share the same non-existent source_repo path.
/// Canonicalize must fail → refuse, not pass via raw string equality.
#[test]
#[cfg(unix)]
fn repo_release_nonexistent_source_refuses() {
    let (base, home, _repo, wt) = managed_wt_fixture("nosrc");
    let bogus = "/nonexistent/source/repo/for/test";
    // Write marker with non-existent source.
    std::fs::write(
        wt.join(".agend-managed"),
        format!("agent=agent-nosrc\nbranch=feat/test\nsource_repo={bogus}\n"),
    )
    .expect("write marker");
    // Bind with the same non-existent source.
    crate::binding::bind_full(
        &home,
        "agent-nosrc",
        "",
        "feat/test",
        &wt,
        std::path::Path::new(bogus),
        false,
    )
    .expect("bind");

    let r = dispatch_repo_release(&home, "agent-nosrc", wt.to_str().unwrap());

    assert!(
        r.get("error").is_some(),
        "non-existent source_repo must refuse even when both match: {r}"
    );
    assert!(
        wt.exists(),
        "worktree must be preserved on non-existent source"
    );
    assert!(
        crate::binding::read(&home, "agent-nosrc").is_some(),
        "binding must be preserved on non-existent source"
    );
    std::fs::remove_dir_all(&base).ok();
}

// ═══ Arch14 managed-marker source parity + safe legacy adoption ═══════════
// (t-20260719234255106641-39872-4, decision d-20260719234211852352-4)
//
// RED group (red on 1394597d, green after the producer/adoption fix):
//   the real checkout/create producers write 3-line sourceless markers that
//   the #2810 deep-validated path-addressed release categorically refuses.
// GREEN-guard group (green on 1394597d, MUST stay green): the fail-closed
//   negative matrix — refusals assert `error` presence + state preservation
//   only, never message text, so the fix cannot be satisfied by rewording.

/// Write a pre-fix LEGACY marker: agent/branch/leased_at, NO source_repo line.
#[cfg(unix)]
fn arch14_write_legacy_marker(wt: &std::path::Path, agent: &str, branch: &str) {
    std::fs::write(
        wt.join(".agend-managed"),
        format!("agent={agent}\nbranch={branch}\nleased_at=2026-07-18T00:00:00+00:00\n"),
    )
    .expect("write legacy marker");
}

/// RED 1: the full real chain — `repo checkout bind:true` then path-addressed
/// `repo release` on the produced worktree — must succeed end-to-end.
///
/// Root RED-gate fix: the fixture lives under a release-eligible $HOME path
/// (release_guard_tmp pattern) — a temp_dir home canonicalizes to
/// /private/var/… and dies at `validate_release_path`'s system-path check,
/// which would mask the marker/deep-validation seam this test pins.
#[test]
#[cfg(unix)]
fn arch14_checkout_then_path_release_succeeds() {
    let base = release_guard_tmp("arch14-chain");
    let home = base.join("home");
    std::fs::create_dir_all(&home).expect("mkdir home");
    let source = p778_setup_source_repo(&base, "feat/arch14");
    let agent = "arch14-chain-agent";

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/arch14",
            "bind": true,
        }),
        agent,
    );
    assert!(resp.get("error").is_none(), "checkout must succeed: {resp}");
    let wt = std::path::PathBuf::from(resp["path"].as_str().expect("path"));

    let r = dispatch_repo_release(&home, agent, wt.to_str().unwrap());
    assert!(
        r["released"].as_bool() == Some(true) && r.get("error").and_then(|e| e.as_str()).is_none(),
        "path-addressed release of a checkout-provisioned worktree must succeed: {r}"
    );
    assert!(!wt.exists(), "released worktree must be removed: {r}");
    std::fs::remove_dir_all(&base).ok();
}

/// RED 2: the checkout producer itself writes the canonical four-field
/// identity — `source_repo=` present and pointing at the canonical source.
#[test]
#[cfg(unix)]
fn arch14_checkout_marker_writes_canonical_source_repo() {
    let home = p778_tmp_home("arch14-prod");
    let parent = p778_tmp_home("arch14-prod-src");
    let source = p778_setup_source_repo(&parent, "feat/arch14p");
    let agent = "arch14-prod-agent";

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "repository_path": source.display().to_string(),
            "branch": "feat/arch14p",
            "bind": true,
        }),
        agent,
    );
    assert!(resp.get("error").is_none(), "checkout must succeed: {resp}");
    let wt = std::path::PathBuf::from(resp["path"].as_str().expect("path"));
    let marker =
        std::fs::read_to_string(wt.join(crate::worktree_pool::MANAGED_MARKER)).expect("marker");
    let src_line = marker
        .lines()
        .find_map(|l| l.strip_prefix("source_repo="))
        .map(str::trim)
        .unwrap_or("");
    assert!(
        !src_line.is_empty(),
        "checkout-written marker must carry a non-empty source_repo= line, got:\n{marker}"
    );
    let canon_marker = std::fs::canonicalize(src_line).expect("marker source must resolve");
    let canon_source = std::fs::canonicalize(&source).expect("fixture source resolves");
    assert_eq!(
        canon_marker, canon_source,
        "marker source_repo must canonicalize to the checkout source"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

/// RED 3: the earliest producer — `worktree::create`'s orphan-guard first
/// write — must already carry source_repo, so no crash window can leave a
/// sourceless marker on disk.
#[test]
#[cfg(unix)]
fn arch14_worktree_create_first_marker_writes_source_repo() {
    let home = p778_tmp_home("arch14-create");
    let parent = p778_tmp_home("arch14-create-src");
    let source = p778_setup_source_repo(&parent, "feat/arch14c");

    let info = crate::worktree::create(&home, &source, "arch14-create-agent", Some("feat/arch14c"))
        .expect("create must succeed");
    let marker = std::fs::read_to_string(info.path.join(crate::worktree_pool::MANAGED_MARKER))
        .expect("marker written by create");
    assert!(
        marker.lines().any(|l| l
            .strip_prefix("source_repo=")
            .is_some_and(|v| !v.trim().is_empty())),
        "create's first marker write must include non-empty source_repo=, got:\n{marker}"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

/// RED 4: safe legacy adoption — a known pre-fix three-line marker (missing
/// source_repo line entirely) with an AUTHORITATIVE disk-fresh binding whose
/// agent/branch corroborate the marker and whose worktree Git pointer matches
/// must release successfully.
#[test]
#[cfg(unix)]
fn arch14_legacy_three_line_marker_adopted_on_release() {
    let (base, home, repo, wt) = managed_wt_fixture("arch14-adopt");
    arch14_write_legacy_marker(&wt, "arch14-adopt-agent", "feat/test");
    crate::binding::bind_full(
        &home,
        "arch14-adopt-agent",
        "",
        "feat/test",
        &wt,
        &repo,
        false,
    )
    .expect("bind");

    let r = dispatch_repo_release(&home, "arch14-adopt-agent", wt.to_str().unwrap());
    assert!(
        r["released"].as_bool() == Some(true) && r.get("error").and_then(|e| e.as_str()).is_none(),
        "legacy sourceless marker with corroborating authoritative binding must be adopted and released: {r}"
    );
    assert!(!wt.exists(), "adopted worktree must be removed: {r}");
    std::fs::remove_dir_all(&base).ok();
}

/// RED 5: a producer write failure must surface — a reused-worktree lease
/// whose marker path is unwritable (a directory) must either return Err or
/// leave a valid readable marker; silently succeeding with a broken marker
/// is the pre-fix swallowed-error behavior.
#[test]
#[cfg(unix)]
fn arch14_lease_marker_write_failure_fails_loud() {
    let home = p778_tmp_home("arch14-loud");
    let parent = p778_tmp_home("arch14-loud-src");
    let source = p778_setup_source_repo(&parent, "feat/arch14l");
    let agent = "arch14-loud-agent";

    let first = crate::worktree_pool::lease(&home, &source, agent, "feat/arch14l")
        .expect("first lease succeeds");
    let marker_path = first.path.join(crate::worktree_pool::MANAGED_MARKER);
    std::fs::remove_file(&marker_path).expect("remove marker");
    std::fs::create_dir(&marker_path).expect("block marker path with a directory");

    let second = crate::worktree_pool::lease(&home, &source, agent, "feat/arch14l");
    match second {
        Err(_) => {} // fail-loud: acceptable post-fix behavior
        Ok(lease) => {
            let content =
                std::fs::read_to_string(lease.path.join(crate::worktree_pool::MANAGED_MARKER));
            assert!(
                content.is_ok_and(|c| c.lines().any(|l| l.starts_with("agent="))),
                "lease reported success but the managed marker is not a valid readable file — \
                 producer write errors must not be silently swallowed"
            );
        }
    }
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

// ── GREEN-guard negative matrix (green today, must stay green) ───────────

/// Guard A: a marker with no agent= identity is refused and state preserved.
#[test]
#[cfg(unix)]
fn arch14_release_still_refuses_agentless_marker() {
    let (base, home, repo, wt) = managed_wt_fixture("arch14-noagent");
    std::fs::write(wt.join(".agend-managed"), "").expect("empty marker");
    crate::binding::bind_full(&home, "arch14-na-agent", "", "feat/test", &wt, &repo, false)
        .expect("bind");
    let r = dispatch_repo_release(&home, "arch14-na-agent", wt.to_str().unwrap());
    assert!(
        r.get("error").is_some() || r.get("code").is_some(),
        "agentless marker must be refused: {r}"
    );
    assert!(wt.exists(), "worktree must be preserved on refusal");
    std::fs::remove_dir_all(&base).ok();
}

/// Guard B: an EXPLICIT blank `source_repo=` line is NOT the legacy-missing
/// case — it must stay refused (adoption is for a missing line only).
#[test]
#[cfg(unix)]
fn arch14_release_still_refuses_explicit_blank_source() {
    let (base, home, repo, wt) = managed_wt_fixture("arch14-blank");
    std::fs::write(
        wt.join(".agend-managed"),
        "agent=arch14-blank-agent\nbranch=feat/test\nsource_repo=\nleased_at=2026-07-18T00:00:00+00:00\n",
    )
    .expect("marker");
    crate::binding::bind_full(
        &home,
        "arch14-blank-agent",
        "",
        "feat/test",
        &wt,
        &repo,
        false,
    )
    .expect("bind");
    let r = dispatch_repo_release(&home, "arch14-blank-agent", wt.to_str().unwrap());
    assert!(
        r.get("error").is_some() || r.get("code").is_some(),
        "explicit blank source_repo= must stay refused: {r}"
    );
    assert!(wt.exists(), "worktree must be preserved on refusal");
    std::fs::remove_dir_all(&base).ok();
}

/// Guard C: identity drift — the marker claims a different agent than the
/// binding/caller — stays refused, state preserved.
#[test]
#[cfg(unix)]
fn arch14_release_still_refuses_identity_drift() {
    let (base, home, repo, wt) = managed_wt_fixture("arch14-drift");
    arch14_write_legacy_marker(&wt, "someone-else", "feat/test");
    crate::binding::bind_full(
        &home,
        "arch14-drift-agent",
        "",
        "feat/test",
        &wt,
        &repo,
        false,
    )
    .expect("bind");
    let r = dispatch_repo_release(&home, "arch14-drift-agent", wt.to_str().unwrap());
    assert!(
        r.get("error").is_some() || r.get("code").is_some(),
        "marker/binding agent drift must stay refused: {r}"
    );
    assert!(wt.exists(), "worktree must be preserved on refusal");
    std::fs::remove_dir_all(&base).ok();
}

/// Guard D: an explicit marker source_repo that MISMATCHES the binding's
/// source stays refused, state preserved.
#[test]
#[cfg(unix)]
fn arch14_release_still_refuses_mismatching_source() {
    let (base, home, repo, wt) = managed_wt_fixture("arch14-mismatch");
    let other = base.join("other-existing-dir");
    std::fs::create_dir_all(&other).expect("mkdir other");
    std::fs::write(
        wt.join(".agend-managed"),
        format!(
            "agent=arch14-mm-agent\nbranch=feat/test\nsource_repo={}\nleased_at=2026-07-18T00:00:00+00:00\n",
            other.display()
        ),
    )
    .expect("marker");
    crate::binding::bind_full(&home, "arch14-mm-agent", "", "feat/test", &wt, &repo, false)
        .expect("bind");
    let r = dispatch_repo_release(&home, "arch14-mm-agent", wt.to_str().unwrap());
    assert!(
        r.get("error").is_some() || r.get("code").is_some(),
        "marker/binding source mismatch must stay refused: {r}"
    );
    assert!(wt.exists(), "worktree must be preserved on refusal");
    std::fs::remove_dir_all(&base).ok();
}

/// Guard E: a malformed worktree Git pointer stays refused even when marker
/// and binding fully agree — no blind-.git authority in either direction.
#[test]
#[cfg(unix)]
fn arch14_release_still_refuses_malformed_git_pointer() {
    let (base, home, repo, wt) = managed_wt_fixture("arch14-gitptr");
    seed_managed_marker(&wt, &repo, &home, "arch14-gp-agent", "feat/test");
    std::fs::write(
        wt.join(".git"),
        "gitdir: /nonexistent/definitely/not/a/gitdir\n",
    )
    .expect("corrupt git pointer");
    let r = dispatch_repo_release(&home, "arch14-gp-agent", wt.to_str().unwrap());
    assert!(
        r.get("error").is_some() || r.get("code").is_some(),
        "malformed git pointer must stay refused: {r}"
    );
    assert!(wt.exists(), "worktree must be preserved on refusal");
    std::fs::remove_dir_all(&base).ok();
}

/// Guard F: legacy adoption never proceeds off a NON-authoritative binding —
/// a hand-tampered binding.json (invalid signature) plus a legacy marker
/// stays refused, state preserved.
#[test]
#[cfg(unix)]
fn arch14_adoption_refused_without_authoritative_binding() {
    let (base, home, repo, wt) = managed_wt_fixture("arch14-noauth");
    arch14_write_legacy_marker(&wt, "arch14-nb-agent", "feat/test");
    crate::binding::bind_full(&home, "arch14-nb-agent", "", "feat/test", &wt, &repo, false)
        .expect("bind");
    // Tamper: rewrite binding.json bytes directly — the HMAC sidecar no longer
    // matches, so the binding is NOT authoritative.
    let bpath = crate::paths::runtime_dir(&home)
        .join("arch14-nb-agent")
        .join("binding.json");
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&bpath).expect("read binding"))
            .expect("parse binding");
    doc["task_id"] = serde_json::json!("tampered");
    std::fs::write(&bpath, serde_json::to_string_pretty(&doc).expect("ser")).expect("tamper");

    let r = dispatch_repo_release(&home, "arch14-nb-agent", wt.to_str().unwrap());
    assert!(
        r.get("error").is_some() || r.get("code").is_some(),
        "legacy adoption must be refused when the binding is not authoritative: {r}"
    );
    assert!(wt.exists(), "worktree must be preserved on refusal");
    std::fs::remove_dir_all(&base).ok();
}

// ── Supplemental (root RED-gate 1 additions, m-20260720000244982995-64) ──

/// Parity guard (green today, must stay green): the NORMAL lease and
/// re-lease paths write the canonical four-field identity with a
/// source_repo that canonicalizes to the actual source.
#[test]
#[cfg(unix)]
fn arch14_lease_rewrite_marker_source_parity() {
    let base = release_guard_tmp("arch14-parity");
    let home = base.join("home");
    std::fs::create_dir_all(&home).expect("mkdir home");
    let source = p778_setup_source_repo(&base, "feat/arch14pp");
    let agent = "arch14-parity-agent";

    let assert_canonical = |path: &std::path::Path, label: &str| {
        let marker = std::fs::read_to_string(path.join(crate::worktree_pool::MANAGED_MARKER))
            .unwrap_or_else(|e| panic!("{label}: marker must be readable: {e}"));
        for field in ["agent=", "branch=", "source_repo=", "leased_at="] {
            assert!(
                marker
                    .lines()
                    .any(|l| l.strip_prefix(field).is_some_and(|v| !v.trim().is_empty())),
                "{label}: marker must carry non-empty {field}, got:\n{marker}"
            );
        }
        let src = marker
            .lines()
            .find_map(|l| l.strip_prefix("source_repo="))
            .map(str::trim)
            .expect("source_repo present");
        assert_eq!(
            std::fs::canonicalize(src).expect("marker source resolves"),
            std::fs::canonicalize(&source).expect("fixture source resolves"),
            "{label}: marker source_repo must canonicalize to the lease source"
        );
    };

    let first = crate::worktree_pool::lease(&home, &source, agent, "feat/arch14pp")
        .expect("first lease succeeds");
    assert_canonical(&first.path, "first lease");

    let second = crate::worktree_pool::lease(&home, &source, agent, "feat/arch14pp")
        .expect("re-lease of an existing worktree succeeds");
    assert_eq!(first.path, second.path, "re-lease reuses the same worktree");
    assert_canonical(&second.path, "re-lease rewrite");
    std::fs::remove_dir_all(&base).ok();
}

/// Guard G: legacy missing-source marker whose BRANCH drifts from the
/// binding stays refused — adoption requires agent/branch corroboration,
/// and today's empty-identity refusal already preserves state.
#[test]
#[cfg(unix)]
fn arch14_adoption_refused_on_branch_drift() {
    let (base, home, repo, wt) = managed_wt_fixture("arch14-bdrift");
    arch14_write_legacy_marker(&wt, "arch14-bd-agent", "feat/other-branch");
    crate::binding::bind_full(&home, "arch14-bd-agent", "", "feat/test", &wt, &repo, false)
        .expect("bind");
    let r = dispatch_repo_release(&home, "arch14-bd-agent", wt.to_str().unwrap());
    assert!(
        r.get("error").is_some() || r.get("code").is_some(),
        "legacy marker with branch drift must stay refused: {r}"
    );
    assert!(wt.exists(), "worktree must be preserved on refusal");
    std::fs::remove_dir_all(&base).ok();
}

/// Guard H: a VALID Git pointer that resolves to a DIFFERENT real repo —
/// marker and binding fully agree with each other but the worktree actually
/// belongs elsewhere — stays refused (complements the malformed-pointer
/// guard: both the fail-safe and the fail-closed arm are pinned).
#[test]
#[cfg(unix)]
fn arch14_release_still_refuses_wrong_repo_git_pointer() {
    let (base, home, repo, wt) = managed_wt_fixture("arch14-wrongptr");
    seed_managed_marker(&wt, &repo, &home, "arch14-wp-agent", "feat/test");

    // A second REAL repo with a REAL worktree; graft its valid .git pointer
    // onto the managed worktree so the pointer resolves — to the wrong repo.
    let other_repo = base.join("other-source");
    std::fs::create_dir_all(&other_repo).expect("mkdir other");
    git_init(&other_repo);
    let other_wt = base.join("other-wt");
    std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            other_wt.to_str().unwrap(),
            "-b",
            "feat/other",
        ])
        .current_dir(&other_repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    let grafted = std::fs::read_to_string(other_wt.join(".git")).expect("other .git pointer");
    std::fs::write(wt.join(".git"), grafted).expect("graft wrong-repo pointer");

    let r = dispatch_repo_release(&home, "arch14-wp-agent", wt.to_str().unwrap());
    assert!(
        r.get("error").is_some() || r.get("code").is_some(),
        "valid-but-wrong-repo git pointer must stay refused: {r}"
    );
    assert!(wt.exists(), "worktree must be preserved on refusal");
    std::fs::remove_dir_all(&base).ok();
}

/// Guard I: an otherwise fully parseable marker that lacks ONLY the agent=
/// line stays refused — distinct from the zero-byte case, which pins the
/// unreadable-identity arm rather than the parseable-but-agentless one.
#[test]
#[cfg(unix)]
fn arch14_release_still_refuses_missing_agent_parseable_marker() {
    let (base, home, repo, wt) = managed_wt_fixture("arch14-noagentline");
    std::fs::write(
        wt.join(".agend-managed"),
        format!(
            "branch=feat/test\nsource_repo={}\nleased_at=2026-07-18T00:00:00+00:00\n",
            repo.display()
        ),
    )
    .expect("agentless parseable marker");
    crate::binding::bind_full(
        &home,
        "arch14-nal-agent",
        "",
        "feat/test",
        &wt,
        &repo,
        false,
    )
    .expect("bind");
    let r = dispatch_repo_release(&home, "arch14-nal-agent", wt.to_str().unwrap());
    assert!(
        r.get("error").is_some() || r.get("code").is_some(),
        "parseable marker missing only agent= must stay refused: {r}"
    );
    assert!(wt.exists(), "worktree must be preserved on refusal");
    std::fs::remove_dir_all(&base).ok();
}

/// arch14 correction (root-validated review of 6fafa912): producers persist
/// the CANONICAL source identity, so a lease taken through a symlink ALIAS
/// stays releasable after the alias is removed — the recorded identity never
/// depends on the alias's continued existence. Deterministic: symlink
/// creation/removal is synchronous; no timing.
#[test]
#[cfg(unix)]
fn arch14_symlink_alias_lease_survives_alias_removal() {
    let base = release_guard_tmp("arch14-symlink");
    let home = base.join("home");
    std::fs::create_dir_all(&home).expect("mkdir home");
    let real = p778_setup_source_repo(&base, "feat/arch14sym");
    let alias = base.join("alias-source");
    std::os::unix::fs::symlink(&real, &alias).expect("symlink alias");
    let agent = "arch14-sym-agent";

    let lease = crate::worktree_pool::lease(&home, &alias, agent, "feat/arch14sym")
        .expect("lease through the alias succeeds");
    let marker = std::fs::read_to_string(lease.path.join(crate::worktree_pool::MANAGED_MARKER))
        .expect("marker");
    let recorded = marker
        .lines()
        .find_map(|l| l.strip_prefix("source_repo="))
        .map(str::trim)
        .expect("source_repo present");
    let real_canonical = std::fs::canonicalize(&real).expect("real source resolves");
    assert_eq!(
        std::path::Path::new(recorded),
        real_canonical.as_path(),
        "marker must record the CANONICAL source, not the alias: {marker}"
    );

    // Bind with the same canonical identity (mirrors checkout's bind_full,
    // which passes source_canonical).
    crate::binding::bind_full(
        &home,
        agent,
        "",
        "feat/arch14sym",
        &lease.path,
        &real_canonical,
        false,
    )
    .expect("bind");

    // The alias disappears — the recorded canonical identity must keep the
    // lease releasable.
    std::fs::remove_file(&alias).expect("remove alias symlink");
    let r = dispatch_repo_release(&home, agent, lease.path.to_str().unwrap());
    assert!(
        r["released"].as_bool() == Some(true) && r.get("error").and_then(|e| e.as_str()).is_none(),
        "release from the recorded canonical identity must succeed after alias removal: {r}"
    );
    assert!(
        !lease.path.exists(),
        "released worktree must be removed: {r}"
    );
    std::fs::remove_dir_all(&base).ok();
}

// ═══ Arch14 legacy marker: absent-binding release (t-…-39872-18) ══════════
// Superseding contract d-20260720044124067125-6. #2860 landed producer
// canonical source parity + BINDING-authoritative legacy adoption; the
// remaining hole: a legacy sourceless-but-otherwise-valid managed worktree
// whose BINDING no longer exists is refused forever
// (managed_release_no_binding) — no retry source can ever settle it.

/// RED (fail today): binding ABSENT + valid linked worktree + non-empty
/// agent/branch three-line legacy marker + authorized caller (the marker
/// agent itself) must path-release via the target's own verified .git
/// linkage. Today delegate_managed_release dies at
/// `managed_release_no_binding` before any identity derivation.
#[test]
#[cfg(unix)]
fn arch14_absent_binding_legacy_marker_releases_via_git_linkage() {
    let (base, home, _repo, wt) = managed_wt_fixture("arch14-nobind");
    arch14_write_legacy_marker(&wt, "arch14-nb2-agent", "feat/test");
    // Deliberately NO binding — the legacy worktree's agent is long gone.

    let r = dispatch_repo_release(&home, "arch14-nb2-agent", wt.to_str().unwrap());
    assert!(
        r["released"].as_bool() == Some(true) && r.get("error").and_then(|e| e.as_str()).is_none(),
        "absent-binding legacy marker with verified .git linkage must release: {r}"
    );
    assert!(!wt.exists(), "released worktree must be removed: {r}");
    std::fs::remove_dir_all(&base).ok();
}

/// Over-correction guard (green today, must stay green): ABSENT binding +
/// a marker missing ONLY the branch= line stays refused and preserved — the
/// absent-binding arm must never derive identity without a branch to match.
#[test]
#[cfg(unix)]
fn arch14_release_still_refuses_missing_branch_marker() {
    let (base, home, repo, wt) = managed_wt_fixture("arch14-nobranch");
    std::fs::write(
        wt.join(".agend-managed"),
        format!(
            "agent=arch14-nbr-agent\nsource_repo={}\nleased_at=2026-07-18T00:00:00+00:00\n",
            repo.display()
        ),
    )
    .expect("branchless marker");
    // Deliberately NO binding — this guard constrains the binding-ABSENT arm.
    let r = dispatch_repo_release(&home, "arch14-nbr-agent", wt.to_str().unwrap());
    assert!(
        r.get("error").is_some() || r.get("code").is_some(),
        "marker missing only branch= must stay refused: {r}"
    );
    assert!(wt.exists(), "worktree must be preserved on refusal");
    std::fs::remove_dir_all(&base).ok();
}

/// Over-correction guard (green today, must stay green): absent binding +
/// zero-byte marker stays refused — the absent-binding arm derives identity
/// only for a marker that still carries non-empty agent AND branch.
#[test]
#[cfg(unix)]
fn arch14_absent_binding_zero_byte_marker_still_refused() {
    let (base, home, _repo, wt) = managed_wt_fixture("arch14-nb-zero");
    std::fs::write(wt.join(".agend-managed"), "").expect("empty marker");

    let r = dispatch_repo_release(&home, "arch14-nbz-agent", wt.to_str().unwrap());
    assert!(
        r.get("error").is_some() || r.get("code").is_some(),
        "absent binding + zero-byte marker must stay refused: {r}"
    );
    assert!(wt.exists(), "worktree must be preserved on refusal");
    std::fs::remove_dir_all(&base).ok();
}

// ── Supplemental RED 2 (root rejection of 98df/7e81 GREEN,
// d-20260720053617389698-7): the new absent-binding arm must (a) support a
// VALID RELATIVE gitlink identity and (b) run its revalidation inside the
// canonical absent-target transaction so a deterministic marker rewrite at
// ReleaseTestPhase::AfterBindingSnapshot is refused. Today it does neither:
// the lexical derive fails on relative gitdirs, and the arm never reaches
// the canonical seam.

/// RED A (fail today): a linked worktree whose `.git` gitlink is RELATIVE
/// (a shape git itself fully supports and resolves) must release exactly
/// like the absolute form — the identity authority is git's common-dir
/// resolution, not lexical path arithmetic on the gitlink text.
#[test]
#[cfg(unix)]
fn arch14_absent_binding_relative_gitlink_releases() {
    let (base, home, _repo, wt) = managed_wt_fixture("arch14-relgit");
    // Rewrite the absolute gitlink to the equivalent RELATIVE form; git
    // resolves it relative to the worktree, so identity stays valid.
    std::fs::write(
        wt.join(".git"),
        "gitdir: ../source/.git/worktrees/managed-wt\n",
    )
    .expect("relative gitlink");
    let probe = crate::git_helpers::git_cmd(&wt, &["rev-parse", "--git-common-dir"])
        .expect("git must still resolve the relative gitlink");
    assert!(
        probe.contains("source"),
        "fixture sanity: relative gitlink resolves to the source repo: {probe}"
    );
    arch14_write_legacy_marker(&wt, "arch14-rel-agent", "feat/test");
    // Deliberately NO binding.

    let r = dispatch_repo_release(&home, "arch14-rel-agent", wt.to_str().unwrap());
    assert!(
        r["released"].as_bool() == Some(true) && r.get("error").and_then(|e| e.as_str()).is_none(),
        "valid RELATIVE gitlink identity must release like the absolute form: {r}"
    );
    assert!(!wt.exists(), "released worktree must be removed: {r}");
    std::fs::remove_dir_all(&base).ok();
}

/// RED B (fail today): a deterministic marker rewrite injected at the
/// canonical ReleaseTestPhase::AfterBindingSnapshot seam must be REFUSED
/// with the worktree preserved — the absent-binding arm must revalidate
/// marker agent/branch/source identity INSIDE the canonical absent-target
/// transaction, after the seam. Today the arm never reaches that seam (its
/// checks run before its own ad-hoc locks), so the rewrite goes unnoticed
/// and the worktree is deleted under a changed identity.
#[test]
#[cfg(unix)]
fn arch14_absent_binding_seam_marker_rewrite_refused() {
    let (base, home, _repo, wt) = managed_wt_fixture("arch14-seam");
    arch14_write_legacy_marker(&wt, "arch14-seam-agent", "feat/test");
    // Deliberately NO binding.

    // Deterministic rewrite: at AfterBindingSnapshot the marker's identity
    // changes hands — the transaction's post-seam revalidation must refuse.
    let marker_path = wt.join(".agend-managed");
    let _seam = crate::worktree_pool::release_test_seam::install(move |phase| {
        if phase == crate::worktree_pool::ReleaseTestPhase::AfterBindingSnapshot {
            let _ = std::fs::write(
                &marker_path,
                "agent=someone-else\nbranch=feat/test\nleased_at=2026-07-18T00:00:00+00:00\n",
            );
        }
    });

    let r = dispatch_repo_release(&home, "arch14-seam-agent", wt.to_str().unwrap());
    assert!(
        r.get("error").is_some() || r.get("code").is_some(),
        "a marker identity rewrite at the canonical seam must be refused: {r}"
    );
    assert!(
        wt.exists(),
        "worktree must be preserved when the seam rewrite is refused"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// Supplemental RED 3 (root gate d-20260720060251593745-8, reviewer finding
/// on 3fbddf7c): the canonical absent-target transaction re-validates
/// binding/marker/source after AfterBindingSnapshot but never re-reads the
/// target's ACTUAL Git HEAD branch — a deterministic post-seam checkout
/// drift (binding still absent, marker agent/branch/source unchanged) must
/// be refused with the worktree preserved; today the stale pre-gate branch
/// identity passes and the removal proceeds.
#[test]
#[cfg(unix)]
fn arch14_absent_binding_seam_head_drift_refused() {
    let (base, home, _repo, wt) = managed_wt_fixture("arch14-headdrift");
    arch14_write_legacy_marker(&wt, "arch14-hd-agent", "feat/test");
    // Deliberately NO binding.

    // Deterministic ACTUAL-HEAD drift at the canonical seam: the worktree's
    // checked-out branch changes hands while marker/binding stay untouched.
    let wt_for_hook = wt.clone();
    let _seam = crate::worktree_pool::release_test_seam::install(move |phase| {
        if phase == crate::worktree_pool::ReleaseTestPhase::AfterBindingSnapshot {
            let _ = std::process::Command::new("git")
                .args(["checkout", "-b", "feat/drifted"])
                .current_dir(&wt_for_hook)
                .env("AGEND_GIT_BYPASS", "1")
                .output();
        }
    });

    let r = dispatch_repo_release(&home, "arch14-hd-agent", wt.to_str().unwrap());
    assert!(
        r.get("error").is_some() || r.get("code").is_some(),
        "post-seam ACTUAL-HEAD drift must be refused: {r}"
    );
    assert!(
        r["released"].as_bool() != Some(true),
        "post-seam ACTUAL-HEAD drift must not report released: {r}"
    );
    assert!(
        wt.exists(),
        "worktree must be preserved when the HEAD drift is refused"
    );
    std::fs::remove_dir_all(&base).ok();
}
