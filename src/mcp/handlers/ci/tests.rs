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
