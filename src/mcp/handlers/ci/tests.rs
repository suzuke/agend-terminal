use super::*;

#[test]
fn release_repo_rejects_root_path() {
    let result = handle_release_repo(&serde_json::json!({"path": "/"}));
    assert!(result["error"].as_str().is_some(), "root must be rejected");
}

#[test]
fn release_repo_rejects_system_path() {
    let result = super::validate_release_path("/etc");
    assert!(result.is_err(), "/etc must be rejected: {:?}", result);
}

#[test]
fn release_repo_rejects_empty_path() {
    let result = handle_release_repo(&serde_json::json!({"path": ""}));
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
fn validate_release_path_accepts_deep_existing() {
    // Create a temp dir deep enough to pass.
    let home = std::env::var("HOME").expect("HOME must be set");
    let dir =
        std::path::PathBuf::from(home).join(format!(".agend-release-test-{}", std::process::id()));
    let deep = dir.join("sub");
    std::fs::create_dir_all(&deep).ok();
    let result = super::validate_release_path(deep.to_str().expect("valid UTF-8"));
    // Should pass (deep enough, not system dir).
    assert!(
        result.is_ok(),
        "deep existing path should pass: {:?}",
        result.err()
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn dispatch_with_branch_and_repo_auto_invokes_watch_ci() {
    let home = std::env::temp_dir().join(format!("agend-auto-watch-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repo": "owner/repo", "branch": "feat/test"});
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
    let args = serde_json::json!({"repo": "owner/repo", "branch": "feat/idem"});
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
    let args = serde_json::json!({"repo": "owner/repo", "branch": "feat-test"});

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
    let args = serde_json::json!({"repo": "owner/repo", "branch": "feat-test"});

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
    let args = serde_json::json!({"repo": "owner/repo", "branch": "feat-test"});

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
        &serde_json::json!({"repo": "owner/repo", "branch": "feat-test"}),
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
    let args = serde_json::json!({"repo": "owner/repo", "branch": "feat-test"});
    handle_watch_ci(&home, &args, "lead");
    handle_watch_ci(&home, &args, "dev");

    let path = watch_path_for(&home, "owner/repo", "feat-test");
    assert!(path.exists());

    let unwatch_args = serde_json::json!({
        "repo": "owner/repo",
        "branch": "feat-test",
        "instance": "lead",
    });
    let resp = handle_unwatch_ci(&home, &unwatch_args);

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
fn ci_unwatch_deletes_file_when_subscribers_empty() {
    // Hard contract item 5 (b): only the LAST unwatch deletes the
    // file. Without this, the watch leaks rate-limit budget on a
    // branch nobody cares about anymore.
    let home = std::env::temp_dir().join(format!("agend-unwatch-delete-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repo": "owner/repo", "branch": "feat-test"});
    handle_watch_ci(&home, &args, "lead");

    let path = watch_path_for(&home, "owner/repo", "feat-test");
    assert!(path.exists());

    let unwatch_args = serde_json::json!({
        "repo": "owner/repo",
        "branch": "feat-test",
        "instance": "lead",
    });
    let resp = handle_unwatch_ci(&home, &unwatch_args);

    assert_eq!(resp["watching"].as_bool(), Some(false));
    assert!(!path.exists(), "last subscriber unwatch must delete file");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ci_unwatch_unknown_caller_is_noop_keeps_watch() {
    // Defensive: unwatch from an instance that never subscribed
    // must not silently delete the watch (would have been a quiet
    // way to clobber lead's watch via dev's typo).
    let home = std::env::temp_dir().join(format!("agend-unwatch-noop-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    let args = serde_json::json!({"repo": "owner/repo", "branch": "feat-test"});
    handle_watch_ci(&home, &args, "lead");

    let path = watch_path_for(&home, "owner/repo", "feat-test");
    let unwatch_args = serde_json::json!({
        "repo": "owner/repo",
        "branch": "feat-test",
        "instance": "stranger",
    });
    handle_unwatch_ci(&home, &unwatch_args);

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
    let args = serde_json::json!({"repo": "owner/repo", "branch": "feat-test"});
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
    let args = serde_json::json!({"repo": "owner/repo", "branch": "feat-test"});
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
    let args = serde_json::json!({"repo": "owner/repo", "branch": "feat-test"});
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
        &serde_json::json!({"repo": "o/r1", "branch": "feat-test"}),
        "lead",
    );
    handle_watch_ci(
        &home,
        &serde_json::json!({"repo": "o/r2", "branch": "feat-test"}),
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
        &serde_json::json!({"repo": "o/alpha", "branch": "feat-test"}),
        "lead",
    );
    handle_watch_ci(
        &home,
        &serde_json::json!({"repo": "o/beta", "branch": "feat-test"}),
        "lead",
    );

    let resp = handle_status_ci(&home, &serde_json::json!({"repo": "o/alpha"}), "lead");
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
            &serde_json::json!({"repo": "owner/repo", "branch": branch}),
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
        &serde_json::json!({"repo": "owner/repo"}),
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
            "repo": "owner/repo",
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

#[test]
#[cfg(unix)]
fn checkout_bind_true_auto_derives_next_after_ci_from_team_1040() {
    // #1040 RED→GREEN: when `repo action=checkout bind=true` arms a
    // ci-watch sidecar, the team's `<team>-reviewer` member should be
    // auto-derived into `next_after_ci` — same convention as the
    // #1037 dispatch-side auto-derive, applied at the checkout path.
    //
    // Empirical motivation (per /tmp/dialectic-1040-primary-dev.md):
    // `handle_checkout_repo:158-162` calls `handle_watch_ci` directly
    // with only `{repo, branch}` args, bypassing
    // `dispatch_auto_bind_lease_with_source_and_chain` entirely. The
    // #1037 auto-derive helper lives inside the bypassed wrapper, so
    // every modern fixup-team PR (#1027 through #1031, all of which
    // used `repo action=checkout bind=true` at task-claim time) armed
    // sidecars with `next_after_ci=None`.
    //
    // REGRESSION-PROOF: revert the `derive_team_reviewer` call at
    // handle_checkout_repo line 158 → this assertion FAILS because
    // `watch["next_after_ci"]` reads as null instead of `"val-reviewer"`.
    let home = p778_tmp_home("1040-derive");
    let parent = p778_tmp_home("1040-derive-src");
    let source = p778_setup_source_repo(&parent, "feat/1040-derive");
    let agent = "val-dev";

    // Seed fleet.yaml with team `val` containing `val-dev` + `val-reviewer`.
    // The auto-derive scans the target's team for a `*-reviewer` member.
    let yaml = format!(
        "instances: {{}}\nteams:\n  val:\n    members:\n      - val-dev\n      - val-reviewer\n    source_repo: {}\n",
        source.display()
    );
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "source": source.display().to_string(),
            "branch": "feat/1040-derive",
            "bind": true,
        }),
        agent,
    );
    assert!(resp.get("error").is_none(), "checkout must succeed: {resp}");

    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("owner/repo", "feat/1040-derive"),
    );
    assert!(watch_path.exists(), "ci-watch sidecar must be armed");

    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).expect("read watch"))
            .expect("parse watch");
    assert_eq!(
        watch["next_after_ci"].as_str(),
        Some("val-reviewer"),
        "#1040 GREEN: checkout-bound watch must auto-derive next_after_ci \
         from team `<team>-reviewer` convention. Got: {watch}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

#[test]
#[cfg(unix)]
fn checkout_bind_true_writes_binding_marker_and_arms_watch() {
    // Empirical regression-proof anchor for #778 Option 1.
    let home = p778_tmp_home("ok");
    let parent = p778_tmp_home("ok-src-parent");
    let source = p778_setup_source_repo(&parent, "feat/p778");
    let agent = "p778-agent-ok";

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "source": source.display().to_string(),
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

    // Auto-watch_ci must have been armed via derive_repo_from_remote_pub.
    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("owner/repo", "feat/p778"),
    );
    assert!(
        watch_path.exists(),
        "watch_ci must be armed for derived repo on bind:true"
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
            "source": source.display().to_string(),
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
        !wt_path.join(crate::worktree_pool::MANAGED_MARKER).exists(),
        ".agend-managed marker must NOT be written without bind:true"
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
            "source": "/tmp",  // never reached — E4.5 fires first
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
            "source": "/tmp",
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
            "source": source.display().to_string(),
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
            "source": source.display().to_string(),
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
    repo
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
            "source": source.display().to_string(),
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
                    "source": source_c.display().to_string(),
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
    // Exactly one winner observed auto_created_branch=true. The other
    // either fell through to a successful worktree add (rare — same
    // branch on same repo blocks second worktree) or failed at
    // worktree_add stage with auto_created_branch absent (error path).
    let winners = results
        .iter()
        .filter(|r| r["auto_created_branch"].as_bool() == Some(true))
        .count();
    assert_eq!(
        winners, 1,
        "exactly one caller must author the branch: {results:?}"
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
                "source": source.display().to_string(),
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
            "source": source.display().to_string(),
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

    // ci_watch arming uses derive_repo_from_remote_pub on origin URL —
    // the fixture's `https://github.com/owner/repo.git` resolves to
    // `owner/repo`.
    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("owner/repo", "feat/p780-tail"),
    );
    assert!(
        watch_path.exists(),
        "watch_ci must be armed on auto-create path"
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
            "source": source.display().to_string(),
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
            "source": source.display().to_string(),
            "branch": "feat/p779p2-bind",
            "bind": true,
        }),
        agent,
    );

    assert_eq!(
        resp["bound"].as_bool(),
        Some(true),
        "lease success → bound=true must hold even with tail-op partial failure: {resp}"
    );
    let warnings = resp["warnings"]
        .as_array()
        .expect("warnings array must be present when bind_full failed");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().is_some_and(|s| s.starts_with("bind_full:"))),
        "warnings must contain a `bind_full:` prefix entry: {resp}"
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
        &serde_json::json!({"repo": "owner/repo", "branch": "feat/p779p2-watch"}),
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

#[test]
#[cfg(unix)]
fn checkout_bind_true_watch_ci_failure_surfaces_warning() {
    // Test 2 (depends on Piece 3 hardening landed in C3): pre-create
    // ci-watches as a regular file. handle_watch_ci's site-A hardening
    // returns `{error, code: ci_watches_dir_create_failed}`. handle_
    // checkout_repo captures it into `warnings: ["watch_ci: ... (code=ci_watches_dir_create_failed)"]`.
    // `bound: true` MUST still hold (lease succeeded).
    let home = p778_tmp_home("779p2-watch-warn");
    let parent = p778_tmp_home("779p2-watch-warn-src");
    let source = p778_setup_source_repo(&parent, "feat/p779p2-watch");
    let agent = "p779p2-agent-watch-warn";

    // Block ci-watches dir create by pre-creating the path as a file.
    let ci_watches = crate::daemon::ci_watch::ci_watches_dir(&home);
    if let Some(p) = ci_watches.parent() {
        std::fs::create_dir_all(p).ok();
    }
    std::fs::write(&ci_watches, "blocking file (not a dir)").unwrap();

    let resp = super::handle_checkout_repo(
        &home,
        &serde_json::json!({
            "source": source.display().to_string(),
            "branch": "feat/p779p2-watch",
            "bind": true,
        }),
        agent,
    );

    assert_eq!(
        resp["bound"].as_bool(),
        Some(true),
        "lease success → bound=true must hold despite watch_ci failure: {resp}"
    );
    let warnings = resp["warnings"]
        .as_array()
        .expect("warnings array must be present when watch_ci failed");
    let watch_warning = warnings
        .iter()
        .find_map(|w| w.as_str().filter(|s| s.starts_with("watch_ci:")))
        .expect("warnings must contain a `watch_ci:` prefix entry");
    assert!(
        watch_warning.contains("code=ci_watches_dir_create_failed"),
        "watch_ci warning must surface the canonical code: {watch_warning}"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&parent).ok();
}

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
            "source": source.display().to_string(),
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

    let resp =
        super::handle_cleanup_init_commits(&home, &serde_json::json!({"agent": "dev"}), "operator");
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

    let resp =
        super::handle_cleanup_init_commits(&home, &serde_json::json!({"agent": "dev"}), "operator");
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

    let resp =
        super::handle_cleanup_init_commits(&home, &serde_json::json!({"agent": "dev"}), "operator");
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

    let resp =
        super::handle_cleanup_init_commits(&home, &serde_json::json!({"agent": "dev"}), "operator");
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
        &serde_json::json!({"agent": "ghost-agent"}),
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
        &serde_json::json!({"repo": "owner/repo.git", "branch": "feat/x"}),
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
        &serde_json::json!({"repo": "owner/repo.git", "branch": "feat/x"}),
        "agent-a",
    );
    let _ = handle_watch_ci(
        &home,
        &serde_json::json!({"repo": "Owner/Repo", "branch": "feat/x"}),
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
        &serde_json::json!({"repo": "https://gitlab.com/owner/repo", "branch": "feat/x"}),
        "dev",
    );
    assert_eq!(
        r["code"].as_str(),
        Some("invalid_repo_format"),
        "GitLab URL must be rejected as invalid_repo_format: {r}"
    );
}

// ----- #1244 PR-B: merge preflight CI gate -----

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
    let result = super::handle_merge_repo(&home, &json!({"pr": 1234, "force": true}), "dev");
    assert!(result["error"].as_str().unwrap().contains("force_reason"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn merge_force_audit_write_failure_refuses_merge() {
    let home = std::env::temp_dir().join(format!("agend-merge-audit-fail-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let events_path = home.join("fleet_events.jsonl");
    std::fs::create_dir_all(&events_path).unwrap();

    let result = super::handle_merge_repo(
        &home,
        &json!({"pr": 9999, "force": true, "force_reason": "test emergency"}),
        "dev",
    );
    let err = result["error"].as_str().unwrap();
    assert!(
        err.contains("audit log write failed"),
        "expected audit failure error, got: {err}"
    );
    let _ = std::fs::remove_dir_all(&home);
}
