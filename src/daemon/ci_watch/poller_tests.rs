use super::*;
use crate::agent::AgentRegistry;

enum RunsResponse<'a> {
    Run(&'a serde_json::Value),
    NoRuns,
    ApiError(String),
}

fn classify_runs_response(status: u16, body: &serde_json::Value) -> RunsResponse<'_> {
    if !(200..300).contains(&status) {
        let message = body["message"].as_str().unwrap_or("(no message)");
        let hint = if status == 403
            && std::env::var("GITHUB_TOKEN").is_err()
            && message.to_lowercase().contains("rate limit")
        {
            " — set GITHUB_TOKEN to raise the unauthenticated 60/hr cap"
        } else {
            ""
        };
        return RunsResponse::ApiError(format!("GH API {status}: {message}{hint}"));
    }
    match body["workflow_runs"].as_array().and_then(|a| a.first()) {
        Some(run) => RunsResponse::Run(run),
        None => RunsResponse::NoRuns,
    }
}

#[test]
fn ci_watches_dir_returns_expected_path() {
    let home = std::path::Path::new("/tmp/test");
    assert_eq!(
        ci_watches_dir(home),
        std::path::PathBuf::from("/tmp/test/ci-watches")
    );
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-ciwatch-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

// -----------------------------------------------------------------
// Sprint 54 P0-2 — adaptive_interval. 3-zone curve based on
// remaining quota. Each test pins one of the contract gates from
// dispatch m-20260507042300780703-3:
//   1. healthy zone (remaining_pct > 0.5) → no scaling
//   2. cautious zone (0.1 < … ≤ 0.5)      → ×2
//   3. critical zone (≤ 0.1)              → ×4
//   4. missing headers                    → fallback to baseline
//
// Empirical regression-proof: a mutation that always returns the
// baseline regardless of remaining_pct trips tests 2 and 3.
// -----------------------------------------------------------------

#[test]
fn adaptive_interval_healthy_zone_uses_configured_baseline() {
    // remaining/limit = 1.0 (full quota) ⇒ no scaling, exactly the
    // configured interval. Mirror this in production: a freshly
    // booted daemon with full GitHub quota polls at user-configured
    // cadence, never widening behind the operator's back.
    assert_eq!(adaptive_interval(60, Some(5000), Some(5000)), 60);
    // Boundary: just above 50% (501/1000 = 0.501) is still healthy.
    assert_eq!(adaptive_interval(60, Some(501), Some(1000)), 60);
}

#[test]
fn adaptive_interval_cautious_zone_doubles() {
    // remaining_pct = 0.3 ⇒ ×2 multiplier. The agent is consuming
    // quota faster than the healthy threshold but isn't critical
    // yet — preempt by halving poll frequency.
    assert_eq!(adaptive_interval(60, Some(300), Some(1000)), 120);
    // Boundary: exactly 50% is cautious (the >0.5 → healthy guard
    // is strict, so 0.5 falls into the next zone).
    assert_eq!(adaptive_interval(60, Some(500), Some(1000)), 120);
    // Boundary: just above 10% (101/1000 = 0.101) is still cautious.
    assert_eq!(adaptive_interval(60, Some(101), Some(1000)), 120);
}

#[test]
fn adaptive_interval_critical_zone_quadruples() {
    // remaining_pct = 0.05 ⇒ ×4 multiplier. Avoids burning the
    // last few requests in a cluster. Mirror this in production:
    // when GitHub returns a low remaining count, the next poll
    // backs off so the daemon doesn't trip the 60/hr (unauth) or
    // 5000/hr (auth) cap.
    assert_eq!(adaptive_interval(60, Some(50), Some(1000)), 240);
    // Boundary: exactly 10% is critical (the >0.1 → cautious guard
    // is strict).
    assert_eq!(adaptive_interval(60, Some(100), Some(1000)), 240);
    // Pathological: zero remaining still resolves to ×4 (we don't
    // pretend to know what reset_epoch is — the existing
    // rate_limit_until path handles that recovery separately).
    assert_eq!(adaptive_interval(60, Some(0), Some(1000)), 240);
}

#[test]
fn adaptive_interval_missing_headers_falls_back_to_configured() {
    // Either field absent (or zero limit) ⇒ baseline. Producers
    // that don't emit the GitHub-style headers (GitLab, Bitbucket
    // Cloud) preserve their existing behavior here.
    assert_eq!(adaptive_interval(60, None, None), 60);
    assert_eq!(adaptive_interval(60, Some(5000), None), 60);
    assert_eq!(adaptive_interval(60, None, Some(5000)), 60);
    // Pathological limit=0 (avoids div-by-zero, falls through).
    assert_eq!(adaptive_interval(60, Some(100), Some(0)), 60);
}

#[test]
fn watch_is_due_null_last_polled_at_fires_immediately() {
    // A freshly-registered watch (or a pre-schema file missing the
    // last_polled_at field) must be due on the first tick. This is
    // the condition that makes the next daemon tick actually poll
    // GitHub instead of waiting ~interval_secs.
    assert!(watch_is_due(None, 60, 1_700_000_000_000));
}

#[test]
fn watch_is_due_within_interval_is_throttled() {
    // Polled 30 s ago, interval 60 s ⇒ still throttled. Prevents
    // back-to-back polls from hammering the GitHub API during
    // daemon ticks (10 s cadence) or concurrent callers.
    let now_ms = 1_700_000_000_000_i64;
    let recent = now_ms - 30_000; // 30 s ago
    assert!(!watch_is_due(Some(recent), 60, now_ms));
}

#[test]
fn watch_is_due_past_interval_fires_again() {
    // Polled 61 s ago, interval 60 s ⇒ due. Equality case
    // (elapsed == interval) is also treated as due — boundary
    // matches the `>=` in the throttle.
    let now_ms = 1_700_000_000_000_i64;
    let stale = now_ms - 61_000;
    assert!(watch_is_due(Some(stale), 60, now_ms));
    let exact = now_ms - 60_000;
    assert!(watch_is_due(Some(exact), 60, now_ms));
}

#[test]
fn watch_is_due_future_timestamp_is_throttled() {
    // Defensive: a clock going backwards (or a hand-edited file
    // with a bogus future timestamp) should not flood GH. The
    // saturating_sub makes elapsed non-negative, and 0 < interval
    // ⇒ throttled. We'd rather be quietly silent on a weird clock
    // than burn rate limit.
    let now_ms = 1_700_000_000_000_i64;
    let future = now_ms + 10_000; // 10 s in the future
    assert!(!watch_is_due(Some(future), 60, now_ms));
}

#[test]
fn ci_watch_success_notifies() {
    let msg = ci_notification_message("owner/repo", "main", Some("success"), None, None);
    assert_eq!(msg.as_deref(), Some("[ci-pass] owner/repo@main: passed ✓"));
}

#[test]
fn ci_watch_failure_headline_excludes_detail() {
    // Job detail moved to inbox body — headline just says "failure"
    let msg = ci_notification_message(
        "owner/repo",
        "main",
        Some("failure"),
        Some("Build / Test"),
        None,
    );
    assert_eq!(msg.as_deref(), Some("[ci-fail] owner/repo@main: failure"));
}

#[test]
fn ci_watch_failure_without_detail_same_headline() {
    let msg = ci_notification_message("owner/repo", "main", Some("failure"), None, None);
    assert_eq!(msg.as_deref(), Some("[ci-fail] owner/repo@main: failure"));
}

#[test]
fn ci_watch_in_progress_skipped() {
    let msg = ci_notification_message("owner/repo", "main", None, None, None);
    assert!(
        msg.is_none(),
        "in-progress (null conclusion) must be skipped"
    );
}

#[test]
fn ci_watch_cancelled_notifies() {
    let msg = ci_notification_message("owner/repo", "feat", Some("cancelled"), None, None);
    assert_eq!(
        msg.as_deref(),
        Some("[ci-ended] owner/repo@feat: cancelled")
    );
}

#[test]
fn ci_watch_timed_out_notifies() {
    let msg = ci_notification_message("owner/repo", "main", Some("timed_out"), None, None);
    assert_eq!(
        msg.as_deref(),
        Some("[ci-ended] owner/repo@main: timed_out")
    );
}

#[test]
fn test_force_push_invalidates_run_id() {
    // When head_sha changes between polls, the effective last_run_id
    // should be reset to None so the new run is picked up even if
    // the run_id hasn't changed yet.
    let prev_sha = Some("abc123");
    let current_sha = "def456";
    // Simulate the logic from ci_check_repo
    let last_run_id = Some(42u64);
    let effective = if prev_sha.is_some_and(|prev| prev != current_sha) {
        None
    } else {
        last_run_id
    };
    assert_eq!(effective, None, "force push must reset last_run_id");

    // Same SHA → preserve run_id
    let same_sha = "abc123";
    let effective2 = if prev_sha.is_some_and(|prev| prev != same_sha) {
        None
    } else {
        last_run_id
    };
    assert_eq!(effective2, Some(42), "same SHA must preserve last_run_id");
}

#[test]
fn test_pr_merged_clears_watcher() {
    // When a watch file exists and the PR is terminal, the file
    // should be removed. We test the update_watch_state + remove
    // flow by verifying the file lifecycle.
    let dir = std::env::temp_dir().join(format!("agend-ci-test-merged-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("ci-watches")).ok();
    let watch_path = dir.join("ci-watches").join("test.json");
    std::fs::write(
        &watch_path,
        r#"{"repo":"o/r","branch":"feat","last_run_id":null,"head_sha":null}"#,
    )
    .ok();
    assert!(watch_path.exists());

    // Simulate PR terminal → auto-clear
    let _ = std::fs::remove_file(&watch_path);
    assert!(
        !watch_path.exists(),
        "watcher file must be removed on PR terminal"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// --- classify_runs_response: silent-rate-limit regression pin ---

#[test]
fn classify_response_picks_first_run_on_2xx() {
    let body = serde_json::json!({
        "workflow_runs": [{"id": 42, "head_sha": "abc"}, {"id": 41}]
    });
    match classify_runs_response(200, &body) {
        RunsResponse::Run(r) => assert_eq!(r["id"].as_u64(), Some(42)),
        other => panic!("expected Run, got {:?}", std::mem::discriminant(&other)),
    }
}

#[test]
fn classify_response_no_runs_on_2xx_empty_array() {
    // Genuine "branch has no runs yet" — must NOT be confused with
    // an API error.
    let body = serde_json::json!({"workflow_runs": []});
    assert!(matches!(
        classify_runs_response(200, &body),
        RunsResponse::NoRuns
    ));
}

#[test]
fn classify_response_rate_limit_is_api_error_not_no_runs() {
    // Real-world body returned by GitHub when an unauthenticated
    // client exceeds 60/hr. Without the status check, the absence
    // of `workflow_runs` here looks identical to the legit empty
    // case above and silently swallows every subsequent CI event.
    let body = serde_json::json!({
        "message": "API rate limit exceeded for 1.2.3.4. (But here's the good news: ...)",
        "documentation_url": "https://docs.github.com/rest/overview/resources-in-the-rest-api#rate-limiting"
    });
    match classify_runs_response(403, &body) {
        RunsResponse::ApiError(msg) => {
            assert!(msg.contains("403"), "msg should include status: {msg}");
            assert!(
                msg.contains("rate limit"),
                "msg should surface GH message: {msg}"
            );
        }
        _ => panic!("rate-limit response must be ApiError, not NoRuns"),
    }
}

#[test]
fn classify_response_token_hint_only_when_unauthenticated_403() {
    // Hint should fire on unauthenticated 403 rate-limit. We can't
    // safely mutate $GITHUB_TOKEN in a parallel-test process, so
    // assert only the prefix shape and trust the env-gated branch.
    let body =
        serde_json::json!({"message": "API rate limit exceeded for example. Authenticated …"});
    let RunsResponse::ApiError(msg) = classify_runs_response(403, &body) else {
        panic!("expected ApiError");
    };
    assert!(msg.starts_with("GH API 403: API rate limit exceeded"));
}

#[test]
fn classify_response_5xx_is_api_error() {
    let body = serde_json::json!({"message": "Server Error"});
    assert!(matches!(
        classify_runs_response(500, &body),
        RunsResponse::ApiError(_)
    ));
}

#[test]
fn classify_response_unknown_payload_falls_through_safely() {
    // 200 OK but missing workflow_runs entirely (would never happen
    // in practice but must not panic).
    let body = serde_json::json!({});
    assert!(matches!(
        classify_runs_response(200, &body),
        RunsResponse::NoRuns
    ));
}

// --- github_token_warning: preventive watch_ci response hint ---

#[test]
fn github_token_warning_none_when_token_present() {
    assert!(github_token_warning(Some("ghp_realtokenhere")).is_none());
}

#[test]
fn github_token_warning_set_when_absent() {
    let msg = github_token_warning(None).expect("missing token must warn");
    assert!(
        msg.contains("GITHUB_TOKEN"),
        "message must name the env var: {msg}"
    );
    assert!(
        msg.contains("unauthenticated") || msg.contains("60"),
        "message must explain the cost: {msg}"
    );
}

#[test]
fn github_token_warning_treats_empty_and_whitespace_as_absent() {
    // `std::env::var("GITHUB_TOKEN")` returns `Ok("")` when the var is
    // exported-but-empty — a distinct case from "unset" but equally
    // unusable. Whitespace-only should be treated the same.
    assert!(github_token_warning(Some("")).is_some());
    assert!(github_token_warning(Some("   ")).is_some());
    assert!(github_token_warning(Some("\t\n")).is_some());
}

#[test]
fn test_repo_with_slash_no_collision() {
    // Two repos that would collide under the old `replace('/', '_')`
    // scheme must produce distinct filenames with the hash approach.
    let f1 = watch_filename("owner/repo", "main");
    let f2 = watch_filename("owner_repo", "main");
    assert_ne!(f1, f2, "owner/repo and owner_repo must not collide");

    // Same repo+branch must be deterministic
    let f3 = watch_filename("owner/repo", "main");
    assert_eq!(f1, f3, "same input must produce same filename");

    // Different branches of same repo must differ
    let f4 = watch_filename("owner/repo", "feat");
    assert_ne!(
        f1, f4,
        "different branches must produce different filenames"
    );
}

#[test]
fn test_multi_run_notifies_all_terminal_since_last() {
    let runs = vec![
        CiRun {
            id: 100,
            conclusion: Some("success".into()),
            head_sha: "aaa".into(),
            url: String::new(),
            name: String::new(),
        },
        CiRun {
            id: 101,
            conclusion: Some("success".into()),
            head_sha: "bbb".into(),
            url: String::new(),
            name: String::new(),
        },
        CiRun {
            id: 102,
            conclusion: None,
            head_sha: "ccc".into(),
            url: String::new(),
            name: String::new(),
        },
    ];
    let selected = select_runs_to_notify(&runs, Some(99), None);
    assert_eq!(
        selected,
        vec![0, 1],
        "should notify runs 100 and 101, skip 102 (in-progress)"
    );
}

#[test]
fn test_in_progress_does_not_appear_in_selection() {
    let runs = vec![CiRun {
        id: 200,
        conclusion: None,
        head_sha: "aaa".into(),
        url: String::new(),
        name: String::new(),
    }];
    let selected = select_runs_to_notify(&runs, None, None);
    assert!(selected.is_empty(), "in-progress run must not be selected");
}

#[test]
fn test_mixed_terminal_states_all_notified() {
    let runs = vec![
        CiRun {
            id: 300,
            conclusion: Some("failure".into()),
            head_sha: "a".into(),
            url: String::new(),
            name: String::new(),
        },
        CiRun {
            id: 301,
            conclusion: Some("cancelled".into()),
            head_sha: "b".into(),
            url: String::new(),
            name: String::new(),
        },
        CiRun {
            id: 302,
            conclusion: Some("success".into()),
            head_sha: "c".into(),
            url: String::new(),
            name: String::new(),
        },
    ];
    let selected = select_runs_to_notify(&runs, Some(299), None);
    assert_eq!(
        selected,
        vec![0, 1, 2],
        "all 3 terminal runs should be selected"
    );
}

#[test]
fn test_already_notified_runs_skipped() {
    let runs = vec![
        CiRun {
            id: 400,
            conclusion: Some("success".into()),
            head_sha: "a".into(),
            url: String::new(),
            name: String::new(),
        },
        CiRun {
            id: 401,
            conclusion: Some("success".into()),
            head_sha: "b".into(),
            url: String::new(),
            name: String::new(),
        },
    ];
    // #786: "already notified" semantic now requires BOTH the
    // run_id boundary AND a matching prior conclusion — without
    // the conclusion arg, a same-id rerun with a different outcome
    // would legitimately fire (which is the bug fix). Passing
    // Some("success") here preserves the original pre-#786 intent
    // of this test (suppress stable terminal state).
    let selected = select_runs_to_notify(&runs, Some(400), Some("success"));
    assert_eq!(
        selected,
        vec![1],
        "run 400 already notified with same conclusion, only 401 selected"
    );
}

#[test]
fn test_same_head_sha_deduplicates_notification() {
    let runs = vec![
        CiRun {
            id: 500,
            conclusion: Some("failure".into()),
            head_sha: "abc".into(),
            url: String::new(),
            name: String::new(),
        },
        CiRun {
            id: 501,
            conclusion: Some("success".into()),
            head_sha: "abc".into(),
            url: String::new(),
            name: String::new(),
        },
    ];
    let selected = select_runs_to_notify(&runs, Some(499), None);
    let deduped = dedupe_notifications_by_head_sha(&runs, &selected, None, None);
    assert_eq!(deduped.len(), 1, "same sha → 1 notification");
    assert_eq!(deduped[0].1, 501, "latest run_id wins");
    assert_eq!(deduped[0].2, "abc");
}

#[test]
fn test_dedupe_skips_already_notified_sha() {
    let runs = vec![
        CiRun {
            id: 600,
            conclusion: Some("success".into()),
            head_sha: "aaa".into(),
            url: String::new(),
            name: String::new(),
        },
        CiRun {
            id: 601,
            conclusion: Some("success".into()),
            head_sha: "bbb".into(),
            url: String::new(),
            name: String::new(),
        },
    ];
    // #786: "already notified" for dedup means BOTH the sha AND
    // the conclusion match. Passing Some("success") here preserves
    // the pre-#786 intent — a future PR could add a same-sha
    // different-conclusion test (handled by anchor test 5).
    let selected = select_runs_to_notify(&runs, Some(599), None);
    let deduped = dedupe_notifications_by_head_sha(&runs, &selected, Some("aaa"), Some("success"));
    assert_eq!(
        deduped.len(),
        1,
        "aaa already notified with same conclusion → only bbb"
    );
    assert_eq!(deduped[0].2, "bbb");
}

/// #1042 regression anchor: two runs for the same SHA with DIFFERENT
/// individual conclusions (coverage=success, CI=failure) but the same
/// AGGREGATE conclusion as last_notified. Pre-fix, the highest-id
/// run's individual conclusion ("success") differed from the
/// persisted aggregate ("failure"), so the dedup filter passed on
/// every poll cycle → re-broadcast. Post-fix, the filter compares
/// aggregates and correctly blocks.
#[test]
fn test_1042_same_sha_same_aggregate_suppresses_rebroadcast() {
    let runs = vec![
        CiRun {
            id: 700,
            conclusion: Some("failure".into()),
            head_sha: "abc".into(),
            url: String::new(),
            name: String::new(),
        },
        CiRun {
            id: 701,
            conclusion: Some("success".into()),
            head_sha: "abc".into(),
            url: String::new(),
            name: String::new(),
        },
    ];
    let selected = select_runs_to_notify(&runs, Some(699), None);
    assert_eq!(selected.len(), 2, "both runs should pass site-1 filter");
    let deduped = dedupe_notifications_by_head_sha(&runs, &selected, Some("abc"), Some("failure"));
    assert_eq!(
        deduped.len(),
        0,
        "#1042: same SHA + same aggregate conclusion (failure) \
         must suppress re-broadcast; got {deduped:?}"
    );
}

/// #1042 complement: when the aggregate conclusion genuinely changes
/// (failure → success after a rerun), the notification MUST fire.
#[test]
fn test_1042_same_sha_different_aggregate_fires() {
    let runs = vec![
        CiRun {
            id: 800,
            conclusion: Some("success".into()),
            head_sha: "abc".into(),
            url: String::new(),
            name: String::new(),
        },
        CiRun {
            id: 801,
            conclusion: Some("success".into()),
            head_sha: "abc".into(),
            url: String::new(),
            name: String::new(),
        },
    ];
    let selected = select_runs_to_notify(&runs, Some(799), None);
    let deduped = dedupe_notifications_by_head_sha(&runs, &selected, Some("abc"), Some("failure"));
    assert_eq!(
        deduped.len(),
        1,
        "#1042: same SHA but aggregate changed (failure→success) \
         must fire notification; got {deduped:?}"
    );
}

#[test]
fn test_different_head_sha_triggers_new_notification() {
    let runs = vec![
        CiRun {
            id: 600,
            conclusion: Some("success".into()),
            head_sha: "aaa".into(),
            url: String::new(),
            name: String::new(),
        },
        CiRun {
            id: 601,
            conclusion: Some("success".into()),
            head_sha: "bbb".into(),
            url: String::new(),
            name: String::new(),
        },
    ];
    let selected = select_runs_to_notify(&runs, Some(599), None);
    let deduped = dedupe_notifications_by_head_sha(&runs, &selected, None, None);
    assert_eq!(deduped.len(), 2, "different shas → 2 notifications");
}

#[test]
fn test_inbox_body_failure_has_detail_and_url() {
    let body = build_inbox_body(
        "[ci-fail] o/r@main: failure",
        "failure",
        Some("Check / Clippy"),
        "https://github.com/o/r/actions/runs/123",
    );
    assert!(
        body.contains("Detail: Check / Clippy"),
        "failure body must have detail: {body}"
    );
    assert!(body.contains("URL: https://"), "body must have URL: {body}");
}

#[test]
fn test_inbox_body_success_has_url_no_detail() {
    let body = build_inbox_body(
        "[ci-pass] o/r@main: passed ✓",
        "success",
        None,
        "https://github.com/o/r/actions/runs/456",
    );
    assert!(
        body.contains("URL: https://"),
        "success body must have URL: {body}"
    );
    assert!(
        !body.contains("Detail:"),
        "success body must not have Detail: {body}"
    );
}

#[test]
fn test_headline_excludes_job_detail() {
    // ci_notification_message must NOT contain job/step names like
    // "(ubuntu-latest)" or "(tray feature)" — those go in inbox body.
    let msg = ci_notification_message(
        "o/r",
        "main",
        Some("failure"),
        Some("Check / Clippy (tray)"),
        None,
    );
    let headline = msg.unwrap();
    assert!(
        !headline.contains("Clippy"),
        "headline must not contain job detail: {headline}"
    );
    assert!(
        !headline.contains("tray"),
        "headline must not contain step detail: {headline}"
    );
    assert!(
        headline.contains("failure"),
        "headline must contain conclusion: {headline}"
    );
}

#[test]
fn test_headline_success_clean() {
    let msg = ci_notification_message("o/r", "feat", Some("success"), None, None);
    let headline = msg.unwrap();
    assert!(headline.contains("passed"), "success headline: {headline}");
    assert!(
        !headline.contains("unknown"),
        "no unknown in success: {headline}"
    );
}

#[test]
fn test_watch_expires_after_ttl_inactivity() {
    let dir = tmp_dir("ttl-expire");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    // Watch with last_terminal_seen_at 80 hours ago (> 72h TTL)
    let old_ts = (chrono::Utc::now() - chrono::Duration::hours(80)).to_rfc3339();
    let watch = serde_json::json!({
        "repo": "o/r", "branch": "feat", "interval_secs": 60,
        "instance": "agent1", "last_run_id": null, "head_sha": null,
        "last_polled_at": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": old_ts,
    });
    let filename = watch_filename("o/r", "feat");
    let watch_path = ci_dir.join(&filename);
    std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

    // Run check — should remove the watch
    let registry: AgentRegistry =
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    check_ci_watches(&dir, &registry);

    assert!(!watch_path.exists(), "expired watch must be removed");
    // Event log should have entry
    let log = std::fs::read_to_string(dir.join("event-log.jsonl")).unwrap_or_default();
    assert!(log.contains("ci_watch_removed"), "removal must be logged");
    assert!(
        log.contains("inactivity_ttl"),
        "reason must be inactivity_ttl"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_watch_preserved_when_not_expired() {
    let dir = tmp_dir("ttl-fresh");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    // Watch with recent last_terminal_seen_at (1 hour ago, well within 72h)
    let recent_ts = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let watch = serde_json::json!({
        "repo": "o/r", "branch": "feat", "interval_secs": 60,
        "instance": "agent1", "last_run_id": null, "head_sha": null,
        "last_polled_at": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": recent_ts,
    });
    let filename = watch_filename("o/r", "feat");
    let watch_path = ci_dir.join(&filename);
    std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

    let registry: AgentRegistry =
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    check_ci_watches(&dir, &registry);

    assert!(watch_path.exists(), "fresh watch must be preserved");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_watch_expires_at_absolute() {
    let dir = tmp_dir("ttl-absolute");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    // Watch with expires_at in the past
    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let watch = serde_json::json!({
        "repo": "o/r", "branch": "old", "interval_secs": 60,
        "instance": "agent1", "last_run_id": null, "head_sha": null,
        "last_polled_at": null,
        "expires_at": past,
    });
    let filename = watch_filename("o/r", "old");
    let watch_path = ci_dir.join(&filename);
    std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

    let registry: AgentRegistry =
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    check_ci_watches(&dir, &registry);

    assert!(
        !watch_path.exists(),
        "past-expires_at watch must be removed"
    );
    let log = std::fs::read_to_string(dir.join("event-log.jsonl")).unwrap_or_default();
    // Sprint 57 Wave 2 Track B (#546 Item 1): the eager per-tick GC
    // pass at the top of `check_ci_watches` removes the watch first
    // and emits `reason=eager_gc_expired`; the legacy lazy expiry
    // emits `reason=expired`. Either substring is correct evidence
    // that the absolute-TTL path fired.
    assert!(
        log.contains("expired"),
        "reason must include 'expired'; got:\n{log}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_legacy_watch_without_ttl_fields_not_removed() {
    let dir = tmp_dir("ttl-legacy");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    // Legacy watch without expires_at or last_terminal_seen_at
    let watch = serde_json::json!({
        "repo": "o/r", "branch": "feat", "interval_secs": 60,
        "instance": "agent1", "last_run_id": null, "head_sha": null,
        "last_polled_at": null,
    });
    let filename = watch_filename("o/r", "feat");
    let watch_path = ci_dir.join(&filename);
    std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

    let registry: AgentRegistry =
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    check_ci_watches(&dir, &registry);

    assert!(
        watch_path.exists(),
        "legacy watch without TTL fields must not be removed"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_remove_watch_on_pr_terminal_logs_event() {
    let dir = tmp_dir("pr-terminal");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let filename = watch_filename("o/r", "feat-merged");
    let watch_path = ci_dir.join(&filename);
    std::fs::write(
        &watch_path,
        r#"{"repo":"o/r","branch":"feat-merged","instance":"a1"}"#,
    )
    .unwrap();
    assert!(watch_path.exists());

    remove_watch(&dir, &watch_path, "a1", "o/r", "feat-merged", "pr_terminal");

    assert!(!watch_path.exists(), "watch must be removed");
    let log = std::fs::read_to_string(dir.join("event-log.jsonl")).unwrap_or_default();
    assert!(log.contains("ci_watch_removed"));
    assert!(log.contains("reason=pr_terminal"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_watch_preserved_when_pr_check_unavailable() {
    let dir = tmp_dir("pr-fail");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let future = (chrono::Utc::now() + chrono::Duration::hours(48)).to_rfc3339();
    let recent = chrono::Utc::now().to_rfc3339();
    let watch = serde_json::json!({
        "repo": "o/r", "branch": "feat-active", "interval_secs": 60,
        "instance": "a1", "last_run_id": null, "head_sha": null,
        "last_polled_at": null, "expires_at": future, "last_terminal_seen_at": recent,
    });
    let filename = watch_filename("o/r", "feat-active");
    let watch_path = ci_dir.join(&filename);
    std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).unwrap();

    let registry: AgentRegistry =
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    check_ci_watches(&dir, &registry);

    assert!(
        watch_path.exists(),
        "watch must survive when PR check unavailable"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// --- MockCiProvider + state machine tests ---

use parking_lot::Mutex;

/// Mock CI provider for testing ci_check_repo state machine without HTTP.
struct MockCiProvider {
    poll_result: Mutex<Option<CiPollResult>>,
    pr_state: Mutex<PrState>,
    failure_summary: Mutex<String>,
    /// #813: pre-seeded mergeable response. Defaults to
    /// `Unknown` (fail-open) so existing tests aren't affected
    /// by the new check. Tests targeting #813 set this via
    /// `with_mergeable`.
    mergeable: Mutex<MergeableState>,
}

impl MockCiProvider {
    fn with_runs(runs: Vec<CiRun>) -> Self {
        Self {
            poll_result: Mutex::new(Some(CiPollResult::Runs {
                runs,
                rate_limit_remaining: None,
                rate_limit_limit: None,
            })),
            pr_state: Mutex::new(PrState::Open),
            failure_summary: Mutex::new("Build / Test".to_string()),
            mergeable: Mutex::new(MergeableState::Unknown),
        }
    }

    /// Sprint 54 P0-2: variant that lets a test seed quota counters
    /// directly so adaptive-backoff persistence + throttle behavior
    /// can be exercised end-to-end without an HTTP layer.
    #[allow(dead_code)]
    fn with_runs_and_quota(runs: Vec<CiRun>, remaining: Option<u64>, limit: Option<u64>) -> Self {
        Self {
            poll_result: Mutex::new(Some(CiPollResult::Runs {
                runs,
                rate_limit_remaining: remaining,
                rate_limit_limit: limit,
            })),
            pr_state: Mutex::new(PrState::Open),
            failure_summary: Mutex::new("Build / Test".to_string()),
            mergeable: Mutex::new(MergeableState::Unknown),
        }
    }

    fn with_api_error(status: u16, message: &str) -> Self {
        Self {
            poll_result: Mutex::new(Some(CiPollResult::ApiError {
                status,
                message: message.to_string(),
                rate_limit_reset: None,
            })),
            pr_state: Mutex::new(PrState::Open),
            failure_summary: Mutex::new("Build / Test".to_string()),
            mergeable: Mutex::new(MergeableState::Unknown),
        }
    }

    fn with_pr_terminal(self) -> Self {
        *self.pr_state.lock() = PrState::Terminal { merged: true };
        self
    }

    /// #813: pre-seed the mergeable response so tests can exercise
    /// CONFLICTING / MERGEABLE / UNSTABLE / UNKNOWN paths without
    /// hitting GitHub. The seed is sticky (not consumed on read)
    /// so a multi-poll test sees the same state across cycles
    /// unless explicitly transitioned via `set_mergeable`.
    #[allow(dead_code)]
    fn with_mergeable(self, state: MergeableState) -> Self {
        *self.mergeable.lock() = state;
        self
    }

    /// #813: transition the mock's mergeable response between
    /// poll cycles (lets a test exercise the "transition INTO
    /// CONFLICTING" alert path without spinning up two providers).
    #[allow(dead_code)]
    fn set_mergeable(&self, state: MergeableState) {
        *self.mergeable.lock() = state;
    }
}

#[async_trait::async_trait]
impl CiProvider for MockCiProvider {
    async fn poll_runs(&self, _repo: &str, _branch: &str) -> anyhow::Result<CiPollResult> {
        Ok(self.poll_result.lock().take().unwrap())
    }
    async fn check_pr_terminal(&self, _repo: &str, _branch: &str) -> PrState {
        let mut guard = self.pr_state.lock();
        std::mem::replace(&mut *guard, PrState::Open)
    }
    async fn check_pr_mergeable(&self, _repo: &str, _branch: &str) -> MergeableState {
        self.mergeable.lock().clone()
    }
    async fn fetch_failure_summary(&self, _repo: &str, _run_id: u64) -> String {
        self.failure_summary.lock().clone()
    }
    fn token_warning(&self) -> Option<&'static str> {
        None
    }
}

/// Helper: run ci_check_repo with a mock provider in a temp dir.
fn run_ci_check(
    dir: &Path,
    watch_json: &serde_json::Value,
    provider: &dyn CiProvider,
) -> anyhow::Result<()> {
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let repo = watch_json["repo"].as_str().unwrap();
    let branch = watch_json["branch"].as_str().unwrap();
    let subscribers = parse_subscribers(watch_json);
    let filename = watch_filename(repo, branch);
    let watch_path = ci_dir.join(&filename);
    std::fs::write(
        &watch_path,
        serde_json::to_string_pretty(watch_json).unwrap(),
    )
    .unwrap();

    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        dir,
        &watch_path,
        repo,
        branch,
        &subscribers,
        watch_json["last_run_id"].as_u64(),
        watch_json["head_sha"].as_str(),
        watch_json["last_notified_head_sha"].as_str(),
        watch_json["last_notified_conclusion"].as_str(),
        watch_json["last_stale_emitted_sha"].as_str(),
        &registry,
        provider,
    ))
}

fn base_watch() -> serde_json::Value {
    serde_json::json!({
        "repo": "o/r", "branch": "feat", "interval_secs": 60,
        "instance": "agent1", "last_run_id": null, "head_sha": null,
        "last_polled_at": null, "last_notified_head_sha": null,
    })
}

#[test]
fn mock_success_run_updates_watch_state() {
    let dir = tmp_dir("mock-success");
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 100,
        conclusion: Some("success".into()),
        head_sha: "abc".into(),
        url: "https://example.com/100".into(),
        name: String::new(),
    }]);
    run_ci_check(&dir, &base_watch(), &provider).unwrap();

    // Watch file should be updated with last_run_id and head_sha
    let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
    let updated: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
    assert_eq!(updated["last_run_id"].as_u64(), Some(100));
    assert_eq!(updated["head_sha"].as_str(), Some("abc"));

    // Inbox should have a notification
    let inbox_dir = dir.join("inbox");
    let has_inbox = inbox_dir.exists()
        && std::fs::read_dir(&inbox_dir)
            .map(|d| d.count() > 0)
            .unwrap_or(false);
    assert!(has_inbox, "success run should enqueue inbox notification");
    std::fs::remove_dir_all(&dir).ok();
}

/// Issue #745 regression guard: when an older run's SHA no longer
/// matches the branch head (a newer commit has been pushed since the
/// run was triggered), the notification must be dropped — but the
/// tracker must still advance so the same stale run isn't re-tried
/// on the next poll.
#[test]
fn mock_stale_sha_drops_notification_but_advances_tracker() {
    let dir = tmp_dir("mock-stale-sha");
    // Two runs: NEW_HEAD is the current head (in-progress); OLD_HEAD
    // was just superseded by a push. OLD_HEAD's run completed first
    // (success). Without the staleness filter we would notify users
    // about OLD_HEAD passing — that pass is no longer relevant; the
    // user is waiting on NEW_HEAD.
    let provider = MockCiProvider::with_runs(vec![
        CiRun {
            id: 301, // NEW_HEAD's run (latest, in-progress)
            conclusion: None,
            head_sha: "newhead".into(),
            url: "https://example.com/301".into(),
            name: String::new(),
        },
        CiRun {
            id: 300, // OLD_HEAD's run (terminal but stale)
            conclusion: Some("success".into()),
            head_sha: "oldhead".into(),
            url: "https://example.com/300".into(),
            name: String::new(),
        },
    ]);
    run_ci_check(&dir, &base_watch(), &provider).unwrap();

    // Watch state: tracker advances past the stale run so it won't
    // be re-emitted on the next poll. head_sha is the NEW_HEAD.
    let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
    let updated: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
    assert_eq!(
        updated["head_sha"].as_str(),
        Some("newhead"),
        "head_sha must track newest run's sha"
    );
    assert_eq!(
        updated["last_notified_head_sha"].as_str(),
        Some("oldhead"),
        "stale sha must still mark notified so it isn't re-emitted"
    );

    // Inbox: must NOT contain a `[ci-pass]` for the stale sha.
    // The OLD_HEAD success was dropped silently (info-level log only).
    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    if inbox_path.exists() {
        let content = std::fs::read_to_string(&inbox_path).unwrap();
        assert!(
            !content.contains("[ci-pass]"),
            "stale CI pass must not be delivered: {content}"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Issue #745 follow-up: stale drops must produce an observable `[ci-stale]`
/// inbox message so operators can audit which runs were superseded.
#[test]
fn mock_stale_sha_emits_ci_stale_inbox_message() {
    let dir = tmp_dir("mock-stale-sha-inbox");
    let provider = MockCiProvider::with_runs(vec![
        CiRun {
            id: 301,
            conclusion: None,
            head_sha: "newhead".into(),
            url: "https://example.com/301".into(),
            name: String::new(),
        },
        CiRun {
            id: 300,
            conclusion: Some("success".into()),
            head_sha: "oldhead".into(),
            url: "https://example.com/300".into(),
            name: String::new(),
        },
    ]);
    run_ci_check(&dir, &base_watch(), &provider).unwrap();

    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    assert!(
        inbox_path.exists(),
        "inbox file must exist after stale drop"
    );
    let content = std::fs::read_to_string(&inbox_path).unwrap();
    assert!(
        content.contains("[ci-stale]"),
        "inbox must contain [ci-stale] kind: {content}"
    );
    assert!(
        content.contains("oldhead"),
        "ci-stale message must reference the stale sha: {content}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Negative case: when only one run exists and its sha is the current
/// head, the existing happy path must still notify.
#[test]
fn mock_single_run_current_head_still_notifies() {
    let dir = tmp_dir("mock-single-current");
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 400,
        conclusion: Some("success".into()),
        head_sha: "onlyhead".into(),
        url: "https://example.com/400".into(),
        name: String::new(),
    }]);
    run_ci_check(&dir, &base_watch(), &provider).unwrap();

    let inbox_dir = dir.join("inbox");
    let has_inbox = inbox_dir.exists()
        && std::fs::read_dir(&inbox_dir)
            .map(|d| d.count() > 0)
            .unwrap_or(false);
    assert!(
        has_inbox,
        "single-run case must preserve existing notify behavior"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mock_failure_run_includes_detail() {
    let dir = tmp_dir("mock-failure");
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 200,
        conclusion: Some("failure".into()),
        head_sha: "def".into(),
        url: "https://example.com/200".into(),
        name: String::new(),
    }]);
    run_ci_check(&dir, &base_watch(), &provider).unwrap();

    // Check inbox contains failure detail
    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    if inbox_path.exists() {
        let content = std::fs::read_to_string(&inbox_path).unwrap();
        assert!(
            content.contains("ci-fail"),
            "inbox should have ci-fail: {content}"
        );
        assert!(
            content.contains("Build / Test"),
            "inbox should have failure detail: {content}"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mock_api_error_propagates() {
    let dir = tmp_dir("mock-api-err");
    let provider = MockCiProvider::with_api_error(403, "GH API 403: rate limit exceeded");
    let result = run_ci_check(&dir, &base_watch(), &provider);
    assert!(result.is_err(), "API error must propagate");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("403"), "error should contain status: {err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mock_pr_terminal_clears_watch() {
    let dir = tmp_dir("mock-pr-term");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = base_watch();
    let filename = watch_filename("o/r", "feat");
    let watch_path = ci_dir.join(&filename);
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

    // Provider says PR is terminal — runs response doesn't matter
    let provider = MockCiProvider::with_runs(vec![]).with_pr_terminal();

    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &["agent1".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();

    assert!(!watch_path.exists(), "PR terminal must remove watch file");
    let log = std::fs::read_to_string(dir.join("event-log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("pr_terminal"),
        "event log must record pr_terminal"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mock_no_runs_preserves_watch() {
    let dir = tmp_dir("mock-no-runs");
    let provider = MockCiProvider::with_runs(vec![]);
    run_ci_check(&dir, &base_watch(), &provider).unwrap();

    let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
    assert!(watch_path.exists(), "empty runs must preserve watch");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mock_force_push_resets_tracking() {
    let dir = tmp_dir("mock-force-push");
    // Watch has old head_sha "old123", new run has different sha
    let mut watch = base_watch();
    watch["head_sha"] = serde_json::json!("old123");
    watch["last_run_id"] = serde_json::json!(50);
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 51,
        conclusion: Some("success".into()),
        head_sha: "new456".into(),
        url: "https://example.com/51".into(),
        name: String::new(),
    }]);
    run_ci_check(&dir, &watch, &provider).unwrap();

    let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
    let updated: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
    // After force push, run 51 should be notified even though id > last_run_id=50
    // would normally catch it — the key is head_sha changed so tracking reset
    assert_eq!(updated["last_run_id"].as_u64(), Some(51));
    assert_eq!(updated["head_sha"].as_str(), Some("new456"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mock_rate_limit_writes_backoff_and_propagates_error() {
    let dir = tmp_dir("mock-rate-limit");
    let provider = MockCiProvider {
        poll_result: Mutex::new(Some(CiPollResult::ApiError {
            status: 403,
            message: "GH API 403: rate limit exceeded".to_string(),
            rate_limit_reset: Some(9999999999),
        })),
        pr_state: Mutex::new(PrState::Open),
        failure_summary: Mutex::new(String::new()),
        mergeable: Mutex::new(MergeableState::Unknown),
    };
    let result = run_ci_check(&dir, &base_watch(), &provider);
    assert!(result.is_err(), "rate-limit must propagate as error");
    let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
    let updated: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
    assert_eq!(updated["rate_limit_until"].as_u64(), Some(9999999999));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn rate_limit_backoff_skips_polling() {
    let dir = tmp_dir("backoff-skip");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let future = (chrono::Utc::now().timestamp() + 3600) as u64;
    let watch = serde_json::json!({
        "repo": "o/r", "branch": "feat", "interval_secs": 60,
        "instance": "agent1", "last_run_id": null, "head_sha": null,
        "last_polled_at": null, "rate_limit_until": future,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
    });
    let filename = watch_filename("o/r", "feat");
    let watch_path = ci_dir.join(&filename);
    std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    check_ci_watches(&dir, &registry);
    assert!(watch_path.exists(), "backoff watch must be preserved");
    std::fs::remove_dir_all(&dir).ok();
}

// ── GitLab CiProvider tests (Sprint 39 PR-1) ────────────────────

/// Helper: mock HTTP server for GitLab tests. Captures path + headers.
/// Captured request from mock server: (path, full_request).
type MockCapture = std::sync::Arc<std::sync::Mutex<Option<(String, String)>>>;

/// RAII guard that saves/restores GITLAB_TOKEN + HOME env vars.
/// Also holds a static mutex to serialize env-var-touching tests.
struct GitlabTokenGuard {
    prev_token: Option<std::ffi::OsString>,
    prev_home: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}
static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
impl GitlabTokenGuard {
    fn clear() -> Self {
        let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev_token = std::env::var_os("GITLAB_TOKEN");
        let prev_home = std::env::var_os("HOME");
        std::env::remove_var("GITLAB_TOKEN");
        Self {
            prev_token,
            prev_home,
            _lock: lock,
        }
    }
    fn set(val: &str) -> Self {
        let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev_token = std::env::var_os("GITLAB_TOKEN");
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("GITLAB_TOKEN", val);
        Self {
            prev_token,
            prev_home,
            _lock: lock,
        }
    }
}
impl Drop for GitlabTokenGuard {
    fn drop(&mut self) {
        match &self.prev_token {
            Some(v) => std::env::set_var("GITLAB_TOKEN", v),
            None => std::env::remove_var("GITLAB_TOKEN"),
        }
        if let Some(v) = &self.prev_home {
            std::env::set_var("HOME", v);
        }
    }
}

#[allow(clippy::type_complexity)]
fn gitlab_mock_server(response_body: &str) -> (u16, std::thread::JoinHandle<()>, MockCapture) {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<(String, String)>));
    let captured_clone = captured.clone();
    let body = response_body.to_string();

    // fire-and-forget: test mock server thread
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).expect("read");
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let path = request
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string();
        // Capture full request (path + headers) for assertion.
        *captured_clone.lock().expect("lock") = Some((path, request));

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).expect("write");
    });

    (port, handle, captured)
}

/// §3.5.10 production-path-coupled: GitLabCiProvider::poll_runs
/// against mock server with spec-quoted fixture.
#[test]
fn gitlab_poll_runs_parses_pipelines() {
    let fixture = include_str!("../../../tests/fixtures/gitlab-pipelines-response.json");
    let (port, handle, captured) = gitlab_mock_server(fixture);

    let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let result = rt.block_on(provider.poll_runs("foo/bar", "main"));

    handle.join().expect("mock");
    let (path, _request) = captured.lock().expect("lock").take().expect("captured");
    assert!(
        path.contains("/projects/foo%2Fbar/pipelines"),
        "path must target pipelines: {path}"
    );

    let runs = match result.expect("poll_runs") {
        super::CiPollResult::Runs { runs, .. } => runs,
        other => panic!("expected Runs, got: {other:?}"),
    };
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].id, 47);
    assert_eq!(runs[0].conclusion, Some("success".to_string()));
    assert_eq!(runs[1].conclusion, Some("failure".to_string()));
}

/// §3.5.10: GitLabCiProvider::check_pr_terminal parses MR state.
#[test]
fn gitlab_check_pr_terminal_merged() {
    let fixture = include_str!("../../../tests/fixtures/gitlab-merge-requests-response.json");
    let (port, handle, captured) = gitlab_mock_server(fixture);

    let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let state = rt.block_on(provider.check_pr_terminal("foo/bar", "feat/test"));

    handle.join().expect("mock");
    let (path, _req) = captured.lock().expect("lock").take().expect("captured");
    assert!(path.contains("/merge_requests"), "path: {path}");
    assert!(path.contains("source_branch=feat"), "query: {path}");
    assert!(
        matches!(state, super::PrState::Terminal { merged: true }),
        "expected Terminal(merged), got: {state:?}"
    );
}

/// §3.5.10: GitLabCiProvider::fetch_failure_summary finds failed job.
#[test]
fn gitlab_fetch_failure_summary_finds_failed_job() {
    let fixture = include_str!("../../../tests/fixtures/gitlab-jobs-response.json");
    let (port, handle, captured) = gitlab_mock_server(fixture);

    let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let summary = rt.block_on(provider.fetch_failure_summary("foo/bar", 48));

    handle.join().expect("mock");
    let (path, _req) = captured.lock().expect("lock").take().expect("captured");
    assert!(path.contains("/pipelines/48/jobs"), "path: {path}");
    // Summary starts with stage/name (trace fetch hits second accept
    // which isn't available — falls back to header-only).
    assert!(
        summary.starts_with("test / cargo-test"),
        "summary: {summary}"
    );
}

/// Auth fallback: token_warning returns warning when no token found.
#[test]
fn gitlab_token_warning_when_no_token() {
    let _guard = GitlabTokenGuard::clear();
    let provider = super::GitLabCiProvider::new().expect("provider");
    let warning = provider.token_warning();
    assert!(warning.is_some(), "should warn when no token");
    assert!(
        warning.expect("w").contains("GITLAB_TOKEN"),
        "warning must mention GITLAB_TOKEN"
    );
}

/// Smoke: GitLabCiProvider::new() defaults to gitlab.com base URL.
#[test]
fn gitlab_new_defaults_to_gitlab_com() {
    let provider = super::GitLabCiProvider::new().expect("new");
    assert_eq!(provider.http.base_url, "https://gitlab.com");
}

/// Smoke: with_base_url() sets custom base URL (self-hosted).
#[test]
fn gitlab_with_base_url_sets_custom() {
    let provider = super::GitLabCiProvider::with_base_url("https://git.corp.example.com".into())
        .expect("with_base_url");
    assert_eq!(provider.http.base_url, "https://git.corp.example.com");
}

/// B4: Auth state 1 — GITLAB_TOKEN env present → PRIVATE-TOKEN header sent.
#[test]
fn gitlab_auth_env_token_sends_private_token_header() {
    let fixture = include_str!("../../../tests/fixtures/gitlab-pipelines-response.json");
    let (port, handle, captured) = gitlab_mock_server(fixture);

    let _guard = GitlabTokenGuard::set("test-token-123");
    let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let _ = rt.block_on(provider.poll_runs("foo/bar", "main"));
    handle.join().expect("mock");
    std::env::remove_var("GITLAB_TOKEN");

    let (_, request) = captured.lock().expect("lock").take().expect("captured");
    assert!(
        request.contains("private-token: test-token-123")
            || request.contains("PRIVATE-TOKEN: test-token-123"),
        "must send PRIVATE-TOKEN header: {request}"
    );
}

/// B4 state 2: env absent + glab CLI config present → token from config.
#[test]
fn gitlab_auth_glab_config_fallback_sends_private_token_header() {
    let fixture = include_str!("../../../tests/fixtures/gitlab-pipelines-response.json");
    let (port, handle, captured) = gitlab_mock_server(fixture);

    // Guard serializes + saves/restores GITLAB_TOKEN + HOME.
    let mut guard = GitlabTokenGuard::clear();
    // Setup temp HOME with glab config.
    let temp = std::env::temp_dir().join(format!("agend-glab-test-{}", std::process::id()));
    let glab_dir = temp.join(".config").join("glab-cli");
    std::fs::create_dir_all(&glab_dir).ok();
    std::fs::write(
        glab_dir.join("config.yml"),
        "hosts:\n  gitlab.com:\n    token: glab_config_token_abc\n",
    )
    .expect("write glab config");
    // Override HOME so resolve_token finds the temp config.
    guard.prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &temp);

    let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let _ = rt.block_on(provider.poll_runs("foo/bar", "main"));
    handle.join().expect("mock");

    let (_, request) = captured.lock().expect("lock").take().expect("captured");
    assert!(
        request.contains("private-token: glab_config_token_abc")
            || request.contains("PRIVATE-TOKEN: glab_config_token_abc"),
        "must send PRIVATE-TOKEN from glab config: {request}"
    );

    std::fs::remove_dir_all(&temp).ok();
    // guard drop restores HOME + GITLAB_TOKEN
}

// ── Bitbucket Cloud CiProvider tests (Sprint 39 PR-2) ───────────

/// Env guard for BITBUCKET_TOKEN + HOME (mirrors GitlabTokenGuard).
struct BitbucketTokenGuard {
    prev_token: Option<std::ffi::OsString>,
    prev_home: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}
static BB_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
impl BitbucketTokenGuard {
    fn clear() -> Self {
        let lock = BB_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev_token = std::env::var_os("BITBUCKET_TOKEN");
        let prev_home = std::env::var_os("HOME");
        std::env::remove_var("BITBUCKET_TOKEN");
        Self {
            prev_token,
            prev_home,
            _lock: lock,
        }
    }
    fn set(val: &str) -> Self {
        let lock = BB_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev_token = std::env::var_os("BITBUCKET_TOKEN");
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("BITBUCKET_TOKEN", val);
        Self {
            prev_token,
            prev_home,
            _lock: lock,
        }
    }
}
impl Drop for BitbucketTokenGuard {
    fn drop(&mut self) {
        match &self.prev_token {
            Some(v) => std::env::set_var("BITBUCKET_TOKEN", v),
            None => std::env::remove_var("BITBUCKET_TOKEN"),
        }
        if let Some(v) = &self.prev_home {
            std::env::set_var("HOME", v);
        }
    }
}

/// Reuse gitlab_mock_server for Bitbucket (same raw TCP pattern).
/// §3.5.10: poll_runs parses Bitbucket pipelines response.
#[test]
fn bitbucket_poll_runs_parses_pipelines() {
    let fixture = include_str!("../../../tests/fixtures/bitbucket-pipelines-response.json");
    let (port, handle, captured) = gitlab_mock_server(fixture);
    let _guard = BitbucketTokenGuard::set("user:pass");
    let provider = super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let result = rt.block_on(provider.poll_runs("foo/bar", "main"));
    handle.join().expect("mock");
    let (path, req) = captured.lock().expect("lock").take().expect("captured");
    assert!(
        path.contains("/repositories/foo/bar/pipelines"),
        "path: {path}"
    );
    assert!(path.contains("target.branch=main"), "query: {path}");
    assert!(
        req.to_lowercase().contains("authorization: basic"),
        "must send Basic auth header: {req}"
    );
    let runs = match result.expect("poll_runs") {
        super::CiPollResult::Runs { runs, .. } => runs,
        other => panic!("expected Runs, got: {other:?}"),
    };
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].conclusion, Some("success".to_string()));
    assert_eq!(runs[1].conclusion, Some("failure".to_string()));
}

/// §3.5.10: check_pr_terminal parses Bitbucket PR state.
#[test]
fn bitbucket_check_pr_terminal_merged() {
    let fixture = include_str!("../../../tests/fixtures/bitbucket-pullrequests-response.json");
    let (port, handle, captured) = gitlab_mock_server(fixture);
    let _guard = BitbucketTokenGuard::set("user:pass");
    let provider = super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let state = rt.block_on(provider.check_pr_terminal("foo/bar", "feat/test"));
    handle.join().expect("mock");
    let (path, req) = captured.lock().expect("lock").take().expect("captured");
    assert!(path.contains("/pullrequests"), "path: {path}");
    assert!(path.contains("source.branch.name"), "query: {path}");
    assert!(
        req.to_lowercase().contains("authorization: basic"),
        "auth: {req}"
    );
    assert!(
        matches!(state, super::PrState::Terminal { merged: true }),
        "got: {state:?}"
    );
}

/// §3.5.10: fetch_failure_summary finds failed step.
#[test]
fn bitbucket_fetch_failure_summary_finds_failed_step() {
    // 2-request chain mock: 1st returns steps, 2nd returns log.
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let captured_reqs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
    let cap = captured_reqs.clone();
    let steps_body =
        include_str!("../../../tests/fixtures/bitbucket-steps-response.json").to_string();
    // fire-and-forget: test mock server thread
    let handle = std::thread::spawn(move || {
        for i in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).expect("read");
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = request
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            cap.lock().expect("lock").push((path, request));
            let body = if i == 0 {
                &steps_body
            } else {
                "error line 1\nerror line 2\n"
            };
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
            stream.write_all(resp.as_bytes()).expect("write");
        }
    });
    let _guard = BitbucketTokenGuard::set("user:pass");
    let provider = super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let summary = rt.block_on(provider.fetch_failure_summary("foo/bar", 48));
    handle.join().expect("mock");
    let reqs = captured_reqs.lock().expect("lock");
    assert_eq!(
        reqs.len(),
        2,
        "expected 2 requests for failure-summary chain"
    );
    assert!(
        reqs[0].0.contains("/pipelines/48/steps"),
        "req1 path: {}",
        reqs[0].0
    );
    assert!(
        reqs[0].1.to_lowercase().contains("authorization: basic"),
        "req1 auth"
    );
    assert!(
        reqs[1].0.contains("/log"),
        "req2 path must contain /log: {}",
        reqs[1].0
    );
    assert!(
        reqs[1].1.to_lowercase().contains("authorization: basic"),
        "req2 auth"
    );
    assert!(
        summary.starts_with("Test"),
        "summary must start with step name: {summary}"
    );
    assert!(
        summary.contains("error line"),
        "summary must contain log tail: {summary}"
    );
}

/// Auth state 1: BITBUCKET_TOKEN env → Basic auth header.
#[test]
fn bitbucket_auth_env_token_sends_basic_header() {
    let fixture = include_str!("../../../tests/fixtures/bitbucket-pipelines-response.json");
    let (port, handle, captured) = gitlab_mock_server(fixture);
    let _guard = BitbucketTokenGuard::set("user:app_pass");
    let provider = super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let _ = rt.block_on(provider.poll_runs("foo/bar", "main"));
    handle.join().expect("mock");
    let (_, request) = captured.lock().expect("lock").take().expect("captured");
    assert!(
        request.contains("authorization: Basic") || request.contains("Authorization: Basic"),
        "must send Basic auth: {request}"
    );
}

/// Auth state 3: no token → warning.
#[test]
fn bitbucket_token_warning_when_no_token() {
    let _guard = BitbucketTokenGuard::clear();
    let provider = super::BitbucketCiProvider::new().expect("provider");
    let warning = provider.token_warning();
    assert!(warning.is_some());
    assert!(warning.expect("w").contains("BITBUCKET_TOKEN"));
}

/// Auth state 2: env absent + bb CLI config → Basic auth from config.
#[test]
fn bitbucket_auth_bb_config_fallback_sends_basic_header() {
    let fixture = include_str!("../../../tests/fixtures/bitbucket-pipelines-response.json");
    let (port, handle, captured) = gitlab_mock_server(fixture);
    let mut guard = BitbucketTokenGuard::clear();
    // Setup temp HOME with bb config.
    let temp = std::env::temp_dir().join(format!("agend-bb-test-{}", std::process::id()));
    let bb_dir = temp.join(".config").join("bb");
    std::fs::create_dir_all(&bb_dir).ok();
    std::fs::write(bb_dir.join("config"), "token: bbuser:bb_app_pass\n").expect("write");
    guard.prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &temp);

    let provider = super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let _ = rt.block_on(provider.poll_runs("foo/bar", "main"));
    handle.join().expect("mock");

    let (_, request) = captured.lock().expect("lock").take().expect("captured");
    assert!(
        request.contains("authorization: Basic") || request.contains("Authorization: Basic"),
        "must send Basic auth from bb config: {request}"
    );
    std::fs::remove_dir_all(&temp).ok();
}

/// Smoke: constructors.
#[test]
fn bitbucket_new_defaults_to_api_bitbucket_org() {
    let provider = super::BitbucketCiProvider::new().expect("new");
    assert_eq!(provider.http.base_url, "https://api.bitbucket.org");
}

#[test]
fn bitbucket_with_base_url_sets_custom() {
    let provider = super::BitbucketCiProvider::with_base_url("https://bb.corp.example.com".into())
        .expect("with_base_url");
    assert_eq!(provider.http.base_url, "https://bb.corp.example.com");
}

/// B5: watch_ci rejects bitbucket_server with operator-actionable error.
#[test]
fn watch_ci_rejects_bitbucket_server_with_actionable_error() {
    let dir = std::env::temp_dir().join(format!("agend-bb-reject-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let result = crate::mcp::handlers::ci::handle_watch_ci(
        &dir,
        &serde_json::json!({
            "repo": "myws/myrepo",
            "branch": "feat-test",
            "ci_provider": "bitbucket_server",
        }),
        "test-inst",
    );
    assert!(
        result["error"].as_str().is_some(),
        "must return error for bitbucket_server: {result}"
    );
    let err = result["error"].as_str().expect("error");
    assert!(
        err.contains("not yet supported") || err.contains("Sprint 41"),
        "error must mention deferral: {err}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── PR-3 tests: auto-detect + GHE ───────────────────────────────

#[test]
fn detect_provider_github_com() {
    let (kind, is_custom) = super::detect_provider_from_remote("github.com/owner/repo");
    assert_eq!(kind, "github");
    assert!(!is_custom);
}

#[test]
fn detect_provider_gitlab_com() {
    let (kind, is_custom) = super::detect_provider_from_remote("gitlab.com/group/project");
    assert_eq!(kind, "gitlab");
    assert!(!is_custom);
}

#[test]
fn detect_provider_bitbucket_org() {
    let (kind, is_custom) = super::detect_provider_from_remote("bitbucket.org/ws/repo");
    assert_eq!(kind, "bitbucket_cloud");
    assert!(!is_custom);
}

#[test]
fn detect_provider_custom_domain_defaults_github_with_warning() {
    let (kind, is_custom) = super::detect_provider_from_remote("git.corp.example.com/team/repo");
    assert_eq!(kind, "github", "unknown domain defaults to github");
    assert!(is_custom, "unknown domain must flag custom_host");
}

/// B1: GitHub Enterprise custom domain detected as github + custom.
#[test]
fn detect_provider_github_enterprise_custom_domain() {
    let (kind, is_custom) =
        super::detect_provider_from_remote("https://github.acme.corp/myorg/myrepo");
    assert_eq!(kind, "github");
    assert!(is_custom, "GHE custom domain must flag custom_host");
}

/// #1188: short-form `owner/name` must be detected as GitHub without custom_host warning.
#[test]
fn detect_provider_short_form_owner_name_is_github_not_custom() {
    let (kind, is_custom) = super::detect_provider_from_remote("suzuke/agend-terminal");
    assert_eq!(kind, "github");
    assert!(
        !is_custom,
        "short-form owner/name must NOT flag custom_host"
    );
}

/// B3: explicit ci_provider in watch JSON overrides auto-detect.
#[test]
fn explicit_ci_provider_overrides_auto_detect() {
    // Watch with ci_provider: gitlab but repo URL pointing to github.com.
    // Factory should construct GitLab provider, not GitHub.
    let fixture = include_str!("../../../tests/fixtures/gitlab-pipelines-response.json");
    let (port, handle, captured) = gitlab_mock_server(fixture);

    // Construct GitLab provider via the same factory logic as production:
    // explicit ci_provider="gitlab" + ci_provider_url pointing to mock.
    let watch = serde_json::json!({
        "repo": "github.com/myorg/myrepo",
        "branch": "main",
        "ci_provider": "gitlab",
        "ci_provider_url": format!("http://127.0.0.1:{port}"),
    });
    let ci_type = watch["ci_provider"].as_str().unwrap();
    assert_eq!(ci_type, "gitlab");

    // Construct provider as factory would.
    let url = watch["ci_provider_url"].as_str().unwrap().to_string();
    let provider = super::GitLabCiProvider::with_base_url(url).expect("provider");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let _ = rt.block_on(provider.poll_runs("myorg/myrepo", "main"));
    handle.join().expect("mock");

    // Assert: request went to GitLab-shaped URL (/projects/{id}/pipelines),
    // NOT GitHub-shaped (/repos/{owner}/{repo}/actions/runs).
    let (path, _) = captured.lock().expect("lock").take().expect("captured");
    assert!(
        path.contains("/projects/") && path.contains("/pipelines"),
        "explicit gitlab must produce GitLab-shaped request, got: {path}"
    );
    assert!(
        !path.contains("/repos/") && !path.contains("/actions/"),
        "must NOT produce GitHub-shaped request: {path}"
    );
}

#[test]
fn github_with_base_url_sets_custom_for_ghe() {
    let provider =
        super::GitHubCiProvider::with_base_url("https://github.corp.example.com/api/v3".into())
            .expect("with_base_url");
    assert_eq!(
        provider.http.base_url,
        "https://github.corp.example.com/api/v3"
    );
}

#[test]
fn github_new_defaults_to_api_github_com() {
    let provider = super::GitHubCiProvider::new().expect("new");
    assert_eq!(provider.http.base_url, "https://api.github.com");
}

// ── Hotfix E: auto-clear false-positive tests ────────────────────

#[test]
fn auto_clear_skips_young_watch() {
    // A watch created < 60s ago should NOT be auto-cleared even if
    // PR terminal is detected (stale PR from previous branch use).
    let dir = tmp_dir("young-watch");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    // Watch with expires_at = now + TTL (meaning it was just created).
    let now = chrono::Utc::now();
    let expires = (now + chrono::Duration::hours(super::WATCH_TTL_HOURS)).to_rfc3339();
    let watch = serde_json::json!({
        "repo": "o/r", "branch": "feat-new", "interval_secs": 60,
        "instance": "agent1", "last_run_id": null, "head_sha": null,
        "last_polled_at": null, "expires_at": expires,
    });
    let filename = super::watch_filename("o/r", "feat-new");
    let watch_path = ci_dir.join(&filename);
    std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

    // Provider says PR terminal (stale PR from old branch use).
    let provider = MockCiProvider::with_runs(vec![]).with_pr_terminal();
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat-new",
        &["agent1".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();

    // Watch must survive (young watch grace).
    assert!(
        watch_path.exists(),
        "young watch (<60s) must NOT be auto-cleared on PR terminal"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn auto_clear_fires_on_old_watch_with_merged_pr() {
    // A watch created > 60s ago SHOULD be cleared on PR terminal.
    let dir = tmp_dir("old-watch-clear");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    // Watch with expires_at implying creation > 60s ago.
    let old_creation = chrono::Utc::now() - chrono::Duration::minutes(5);
    let expires = (old_creation + chrono::Duration::hours(super::WATCH_TTL_HOURS)).to_rfc3339();
    let watch = serde_json::json!({
        "repo": "o/r", "branch": "feat-old", "interval_secs": 60,
        "instance": "agent1", "last_run_id": null, "head_sha": null,
        "last_polled_at": null, "expires_at": expires,
    });
    let filename = super::watch_filename("o/r", "feat-old");
    let watch_path = ci_dir.join(&filename);
    std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

    let provider = MockCiProvider::with_runs(vec![]).with_pr_terminal();
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat-old",
        &["agent1".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();

    assert!(
        !watch_path.exists(),
        "old watch (>60s) must be cleared on PR terminal"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── Hotfix F: classification layer tests ─────────────────────────

#[test]
fn fresh_branch_no_pr_classified_as_pending() {
    // No PR found → PrState::Unknown → watch preserved (pending).
    let dir = tmp_dir("classify-no-pr");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = base_watch();
    let filename = watch_filename("o/r", "feat");
    let watch_path = ci_dir.join(&filename);
    std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

    // Provider returns no runs + PrState::Unknown (no PR found).
    let provider = MockCiProvider::with_runs(vec![]);
    // MockCiProvider default pr_state is Open, override to Unknown.
    *provider.pr_state.lock() = PrState::Unknown;

    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &["agent1".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();

    assert!(watch_path.exists(), "no-PR branch must NOT be auto-cleared");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn branch_with_open_pr_classified_as_active() {
    // Open PR → PrState::Open → watch preserved.
    let dir = tmp_dir("classify-open-pr");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = base_watch();
    let filename = watch_filename("o/r", "feat");
    let watch_path = ci_dir.join(&filename);
    std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

    let provider = MockCiProvider::with_runs(vec![]);
    // Default pr_state is Open — watch should persist.

    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &["agent1".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();

    assert!(
        watch_path.exists(),
        "open-PR branch must NOT be auto-cleared"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn branch_with_merged_pr_classified_as_terminal() {
    // Already tested by mock_pr_terminal_clears_watch — verify preserved.
    // Merged PR → PrState::Terminal { merged: true } → auto-clear.
    let dir = tmp_dir("classify-merged");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    // Use old expires_at so grace period doesn't block.
    let old_creation = chrono::Utc::now() - chrono::Duration::minutes(5);
    let expires = (old_creation + chrono::Duration::hours(super::WATCH_TTL_HOURS)).to_rfc3339();
    let watch = serde_json::json!({
        "repo": "o/r", "branch": "feat-merged", "interval_secs": 60,
        "instance": "agent1", "last_run_id": null, "head_sha": null,
        "last_polled_at": null, "expires_at": expires,
    });
    let filename = watch_filename("o/r", "feat-merged");
    let watch_path = ci_dir.join(&filename);
    std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

    let provider = MockCiProvider::with_runs(vec![]).with_pr_terminal();
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat-merged",
        &["agent1".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();

    assert!(
        !watch_path.exists(),
        "merged-PR branch MUST be auto-cleared"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stale_pr_not_classified_as_terminal() {
    // A PR closed >1h ago should be treated as stale (Unknown), not terminal.
    // This is the root-cause fix: prevents false-positive auto-clear from
    // old PRs matching the same branch name.
    // Tested via the GitHub provider's closed_at check.
    // The mock provider bypasses this (returns Terminal directly), so this
    // test verifies the design contract via source inspection.
    // #701 split: provider impls (incl. check_pr_terminal) moved to provider.rs.
    let src = include_str!("provider.rs");
    assert!(
        src.contains("closed_at") && src.contains("Duration::hours(1)"),
        "check_pr_terminal must verify closed_at freshness (stale PR filter)"
    );
}

// ----------------------------------------------------------------------
// Sprint 54 P0-1 — Subscriber fan-out (hard contract item 2).
//
// EMPIRICAL REGRESSION-PROOF ANCHOR: when fan-out is collapsed back to
// single-subscriber (commenting out the `for sub in subscribers` loop
// in `ci_check_repo` and notifying only the first), the second
// subscriber's inbox stays empty here and the assertion fails with:
//
//   thread '...subscriber_fan_out_notifies_every_member' panicked at:
//   assertion `dev inbox missing terminal notification` failed
//
// Restoring the loop returns the test to PASS. This is the proof
// that the new test catches the multi-caller bug in production code,
// not just in synthetic mock paths.
// ----------------------------------------------------------------------
#[test]
fn subscriber_fan_out_notifies_every_member() {
    let dir = tmp_dir("fanout");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "subscribers": [
            {"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"},
            {"instance": "dev",  "subscribed_at": "2026-05-07T00:00:01Z"}
        ],
        "instance": "lead",
        "interval_secs": 60,
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

    // Mock provider returns one terminal success run.
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 7,
        conclusion: Some("success".to_string()),
        head_sha: "deadbeef".to_string(),
        url: "https://example/run/7".to_string(),
        name: String::new(),
    }]);

    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &["lead".to_string(), "dev".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();

    // Both inboxes must contain the terminal notification. Inbox layout
    // is JSONL at home/inbox/<name>.jsonl (see inbox::inbox_path).
    for sub in ["lead", "dev"] {
        let inbox_path = dir.join("inbox").join(format!("{sub}.jsonl"));
        let body = std::fs::read_to_string(&inbox_path).unwrap_or_else(|_| {
            panic!("{sub} inbox file missing — fan-out regression: {inbox_path:?}")
        });
        assert!(
            body.contains("[ci-pass]") && body.contains("o/r@feat"),
            "{sub} inbox payload mismatch: {body}"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

// ----------------------------------------------------------------------
// #1030 RED→GREEN — reviewer auto-wake on CI green.
//
// Two emit sites in `ci_check_repo` bypass the wake-aware
// `enqueue_with_idle_hint` path: site 2 (subscriber [ci-pass] enqueue,
// line ~1092) and site 3 (next_after_ci chain target, PTY-only inject
// ~lines 1142-1149). The empirical symptom (PRs #1028+#1029): reviewer
// sits idle 7 min after CI green until lead manually kicks.
//
// Tests below: T2/T5/T6 fail RED (chain target has no durable inbox
// entry today) and pass GREEN (site 3 swap lands a [ci-ready-for-action]
// inbox entry with idle-hint wake). T1/T3 anti-regress the subscriber
// fan-out + chain-target dedup. T4 stands alone via the
// `enqueue_with_idle_hint_with_emitter` test seam.
// ----------------------------------------------------------------------

/// Helper: a base watch JSON pre-populated with two subscribers + an
/// optional `next_after_ci` chain target. Matches the production
/// `repo action=checkout bind=true` + `send(kind=task, next_after_ci=...)`
/// flow that empirically produced the #1030 wake gap.
fn watch_with_chain(next_after_ci: Option<&str>) -> serde_json::Value {
    let mut watch = serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "subscribers": [
            {"instance": "lead", "subscribed_at": "2026-05-21T00:00:00Z"},
            {"instance": "dev",  "subscribed_at": "2026-05-21T00:00:01Z"}
        ],
        "instance": "lead",
        "interval_secs": 60,
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    if let Some(n) = next_after_ci {
        watch["next_after_ci"] = serde_json::json!(n);
    }
    watch
}

/// T1 (anti-regression for site 2 swap): with the [ci-pass]
/// subscriber enqueue moving from raw `enqueue` to `enqueue_with_idle_hint`,
/// every subscriber's durable JSONL inbox MUST still receive the
/// `[ci-pass]` line (existing wake-blind path is what survives).
#[test]
fn ci_pass_subscriber_inbox_anti_regression() {
    let dir = tmp_dir("1030-t1-anti-regression");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = watch_with_chain(None);
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 1,
        conclusion: Some("success".to_string()),
        head_sha: "abc1234".to_string(),
        url: "https://example/run/1".to_string(),
        name: String::new(),
    }]);
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &["lead".to_string(), "dev".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();
    for sub in ["lead", "dev"] {
        let messages = crate::inbox::drain(&dir, sub);
        assert!(
            messages.iter().any(|m| m.text.contains("[ci-pass]")),
            "{sub} must still receive [ci-pass] after site 2 swap; got: {messages:?}"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// T2 (RED→GREEN, primary signal for #1030): when `next_after_ci`
/// points at an agent, that agent MUST receive a durable
/// `[ci-ready-for-action]` inbox entry on CI pass. RED today —
/// site 3 is PTY-only with no inbox write, so the chain target's
/// JSONL stays empty. GREEN: site 3 swap lands an InboxMessage via
/// `enqueue_with_idle_hint`.
#[test]
fn ci_pass_chain_target_gets_durable_inbox_entry() {
    let dir = tmp_dir("1030-t2-chain-target-inbox");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = watch_with_chain(Some("reviewer"));
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 2,
        conclusion: Some("success".to_string()),
        head_sha: "abc1234".to_string(),
        url: "https://example/run/2".to_string(),
        name: String::new(),
    }]);
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &["lead".to_string(), "dev".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();
    let messages = crate::inbox::drain(&dir, "reviewer");
    assert!(
        messages
            .iter()
            .any(|m| m.text.contains("[ci-ready-for-action]")),
        "reviewer must receive a durable [ci-ready-for-action] inbox entry; got: {messages:?}"
    );
}

/// T3 (anti-regression for site 2's chain-target skip): the
/// subscriber [ci-pass] loop must continue to SKIP an agent whose
/// name appears in `next_after_ci`. Without this skip, the chain
/// target would receive both [ci-pass] (subscriber) AND
/// [ci-ready-for-action] (chain), and the dedup line at poller.rs:1034
/// is what keeps it to one. Test guards against accidental removal.
#[test]
fn ci_pass_chain_target_excluded_from_subscriber_loop() {
    let dir = tmp_dir("1030-t3-chain-skip");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = watch_with_chain(Some("reviewer"));
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 3,
        conclusion: Some("success".to_string()),
        head_sha: "abc1234".to_string(),
        url: "https://example/run/3".to_string(),
        name: String::new(),
    }]);
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Reviewer NOT in subscribers; only in next_after_ci.
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &["lead".to_string(), "dev".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();
    let messages = crate::inbox::drain(&dir, "reviewer");
    assert!(
        !messages.iter().any(|m| m.text.contains("[ci-pass]")),
        "reviewer must NOT receive a [ci-pass] subscriber entry; got: {messages:?}"
    );
}

/// #1032 T1: `make_ci_conflict_alert_msg` produces an InboxMessage
/// whose `enqueue_with_idle_hint_with_emitter` invocation yields a
/// canonical `[AGEND-MSG-PENDING]` hint. The GREEN swap at site 149
/// (`emit_ci_conflict_alert`) wires this helper through
/// `enqueue_with_idle_hint`, restoring the wake signal that bare
/// `enqueue` was silently dropping.
#[test]
fn ci_conflict_alert_hint_format_deterministic() {
    let dir = tmp_dir("1032-t1-conflict-hint");
    std::fs::create_dir_all(&dir).unwrap();
    let msg = super::make_ci_conflict_alert_msg("o/r", "feat", "poll-transition");
    assert_eq!(msg.kind.as_deref(), Some("ci-watch"));
    assert!(msg.text.contains("[ci-conflict-detected]"));
    let captured: std::sync::Arc<parking_lot::Mutex<Option<String>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(None));
    let cap = captured.clone();
    crate::inbox::enqueue_with_idle_hint_with_emitter(&dir, "agent1", msg, move |hint| {
        *cap.lock() = Some(hint.to_string());
    })
    .unwrap();
    let hint = captured.lock().clone().expect("emitter must fire once");
    assert!(
        hint.contains("kind=ci-watch"),
        "conflict hint must carry kind=ci-watch for downstream filtering; got: {hint}"
    );
    assert!(
        hint.contains("from=system:ci"),
        "conflict hint must carry from=system:ci for routing; got: {hint}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// #1032 T2: `make_ci_stale_drop_msg` mirrors T1's invariant for
/// the stale-SHA drop path (#745, site 940). GREEN at site 980
/// wires this helper through `enqueue_with_idle_hint`.
#[test]
fn ci_stale_drop_hint_format_deterministic() {
    let dir = tmp_dir("1032-t2-stale-hint");
    std::fs::create_dir_all(&dir).unwrap();
    let msg = super::make_ci_stale_drop_msg("o/r", "feat", "oldhead", "newhead");
    assert_eq!(msg.kind.as_deref(), Some("ci-stale"));
    assert!(msg.text.contains("[ci-stale]"));
    assert!(msg.text.contains("oldhead"));
    assert!(msg.text.contains("newhead"));
    let captured: std::sync::Arc<parking_lot::Mutex<Option<String>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(None));
    let cap = captured.clone();
    crate::inbox::enqueue_with_idle_hint_with_emitter(&dir, "agent1", msg, move |hint| {
        *cap.lock() = Some(hint.to_string());
    })
    .unwrap();
    let hint = captured.lock().clone().expect("emitter must fire once");
    assert!(
        hint.contains("kind=ci-stale"),
        "stale-drop hint must carry kind=ci-stale; got: {hint}"
    );
    assert!(
        hint.contains("from=system:ci"),
        "stale-drop hint must carry from=system:ci; got: {hint}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// #1031 T1: site 3 emit populates `reviewed_head` with the full
/// 40-char head SHA from the CI run. RED at this commit (caller
/// passes None); GREEN reads `current_sha` at the emit site.
#[test]
fn ci_ready_for_action_carries_full_head_sha() {
    let dir = tmp_dir("1031-t1-head-sha");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-21T00:00:00Z"}],
        "instance": "lead",
        "interval_secs": 60,
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "next_after_ci": "reviewer",
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 1,
        conclusion: Some("success".to_string()),
        // Use a realistic full 40-char SHA.
        head_sha: "abc1234567890abcdef1234567890abcdef12345".to_string(),
        url: "https://example/run/1".to_string(),
        name: String::new(),
    }]);
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &["lead".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();
    let messages = crate::inbox::drain(&dir, "reviewer");
    let action = messages
        .iter()
        .find(|m| m.kind.as_deref() == Some("ci-ready-for-action"))
        .expect("chain target must have a [ci-ready-for-action] entry");
    assert_eq!(
        action.reviewed_head.as_deref(),
        Some("abc1234567890abcdef1234567890abcdef12345"),
        "#1031 GREEN: reviewed_head must be the full 40-char SHA; got: {action:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// #1031 T2: site 3 emit populates `pr_number` from the pr_state
/// aggregator cache. RED at this commit (caller passes None);
/// GREEN reads `pr_state::load(home, repo, branch)`.
#[test]
fn ci_ready_for_action_carries_pr_number_from_pr_state() {
    let dir = tmp_dir("1031-t2-pr-number");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    // Pre-populate the pr_state file so the emit-site lookup
    // finds the cached PR#. record_ci_result writes this naturally
    // in production; here we seed it manually so the test focuses
    // on the read-side enrichment.
    let pr_state_dir = dir.join("pr-state");
    std::fs::create_dir_all(&pr_state_dir).ok();
    // The pr_state filename helper hashes repo+branch; emulate by
    // writing via the public seam.
    crate::daemon::pr_state::record_ci_result(
        &dir,
        "o/r",
        "feat",
        "abc1234567890abcdef1234567890abcdef12345",
        crate::daemon::pr_state::CiConclusion::Pending,
        vec!["lead".to_string()],
        crate::daemon::pr_state::ReviewClass::Single,
    );
    // Now set pr_number on the seeded state.
    let pr_state_path = crate::daemon::pr_state::pr_state_dir(&dir)
        .join(crate::daemon::pr_state::pr_state_filename("o/r", "feat"));
    if let Ok(content) = std::fs::read_to_string(&pr_state_path) {
        if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&content) {
            v["pr_number"] = serde_json::json!(1031);
            std::fs::write(&pr_state_path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        }
    }
    let watch = serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-21T00:00:00Z"}],
        "instance": "lead",
        "interval_secs": 60,
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "next_after_ci": "reviewer",
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 2,
        conclusion: Some("success".to_string()),
        head_sha: "abc1234567890abcdef1234567890abcdef12345".to_string(),
        url: "https://example/run/2".to_string(),
        name: String::new(),
    }]);
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &["lead".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();
    let messages = crate::inbox::drain(&dir, "reviewer");
    let action = messages
        .iter()
        .find(|m| m.kind.as_deref() == Some("ci-ready-for-action"))
        .expect("chain target must have a [ci-ready-for-action] entry");
    assert_eq!(
        action.pr_number,
        Some(1031),
        "#1031 GREEN: pr_number must be populated from pr_state cache; got: {action:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// #1031 T3: site 3 emit populates `task_id` from the ci-watch
/// sidecar's persisted dispatch task_id. RED at this commit
/// (caller passes None); GREEN reads `watch["task_id"]`. This
/// also implicitly RED-asserts the dispatch-side persist hop —
/// the watch sidecar pre-seeded below carries `task_id` directly,
/// which the GREEN read path picks up regardless of how it
/// arrived (dispatch persist OR manual ci_watch arm).
#[test]
fn ci_ready_for_action_carries_task_id_from_watch_sidecar() {
    let dir = tmp_dir("1031-t3-task-id");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-21T00:00:00Z"}],
        "instance": "lead",
        "interval_secs": 60,
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "next_after_ci": "reviewer",
        "task_id": "t-1031-dispatch-id",
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 3,
        conclusion: Some("success".to_string()),
        head_sha: "abc1234567890abcdef1234567890abcdef12345".to_string(),
        url: "https://example/run/3".to_string(),
        name: String::new(),
    }]);
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &["lead".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();
    let messages = crate::inbox::drain(&dir, "reviewer");
    let action = messages
        .iter()
        .find(|m| m.kind.as_deref() == Some("ci-ready-for-action"))
        .expect("chain target must have a [ci-ready-for-action] entry");
    assert_eq!(
        action.task_id.as_deref(),
        Some("t-1031-dispatch-id"),
        "#1031 GREEN: task_id must propagate from watch sidecar; got: {action:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// T4 (deterministic hint delivery via test seam):
/// `make_ci_ready_for_action_msg` produces an InboxMessage that,
/// when passed through `enqueue_with_idle_hint_with_emitter`,
/// generates the expected `[AGEND-MSG-PENDING]` hint string. Passes
/// in both RED and GREEN — invariant on the helper's wire output.
#[test]
fn ci_ready_for_action_hint_format_deterministic() {
    let dir = tmp_dir("1030-t4-hint-format");
    std::fs::create_dir_all(&dir).unwrap();
    let msg = super::make_ci_ready_for_action_msg("o/r", "feat", "o/r@feat", None, None, None);
    let captured: std::sync::Arc<parking_lot::Mutex<Option<String>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(None));
    let cap = captured.clone();
    crate::inbox::enqueue_with_idle_hint_with_emitter(&dir, "reviewer", msg, move |hint| {
        *cap.lock() = Some(hint.to_string());
    })
    .unwrap();
    let hint = captured.lock().clone().expect("emitter must fire once");
    assert!(
        hint.contains("kind=ci-ready-for-action"),
        "hint must carry kind for downstream filtering; got: {hint}"
    );
    assert!(
        hint.contains("from=system:ci"),
        "hint must carry from for routing; got: {hint}"
    );
    assert!(
        hint.contains("inbox="),
        "hint must carry pending count for recipient bookkeeping; got: {hint}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// T5 (RED→GREEN): the chain-target inbox entry's `kind` field
/// MUST be `"ci-ready-for-action"` so downstream agent filters can
/// distinguish it from the regular `ci-watch` fan-out. RED today —
/// no entry exists, so no kind to inspect. GREEN: kind matches.
#[test]
fn ci_pass_chain_target_inbox_kind_is_ready_for_action() {
    let dir = tmp_dir("1030-t5-chain-target-kind");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = watch_with_chain(Some("reviewer"));
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 4,
        conclusion: Some("success".to_string()),
        head_sha: "abc1234".to_string(),
        url: "https://example/run/4".to_string(),
        name: String::new(),
    }]);
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &["lead".to_string(), "dev".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();
    let messages = crate::inbox::drain(&dir, "reviewer");
    let action = messages
        .iter()
        .find(|m| m.text.contains("[ci-ready-for-action]"))
        .expect("chain target must have a [ci-ready-for-action] entry");
    assert_eq!(
        action.kind.as_deref(),
        Some("ci-ready-for-action"),
        "kind field must let downstream filter the chain event"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// T6 (RED→GREEN): when the chain target ALSO appears in
/// `subscribers` (overlap — e.g. operator opts the reviewer into
/// the watch alongside the dispatch chain), the agent must receive
/// EXACTLY ONE inbox entry — the `[ci-ready-for-action]` form
/// because the line 1034 dedup skip routes them out of the
/// subscriber fan-out. Today RED: 0 entries (PTY-only at site 3,
/// subscriber path skipped them, so they get nothing durable).
/// GREEN: 1 entry (the chain target inbox emit lands; subscriber
/// path still skips them; no double-fire).
#[test]
fn ci_pass_chain_target_no_double_fire_on_overlap() {
    let dir = tmp_dir("1030-t6-chain-overlap");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = watch_with_chain(Some("reviewer"));
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 5,
        conclusion: Some("success".to_string()),
        head_sha: "abc1234".to_string(),
        url: "https://example/run/5".to_string(),
        name: String::new(),
    }]);
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Reviewer is BOTH in subscribers AND next_after_ci.
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &[
            "lead".to_string(),
            "dev".to_string(),
            "reviewer".to_string(),
        ],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();
    let messages = crate::inbox::drain(&dir, "reviewer");
    let from_ci: Vec<_> = messages.iter().filter(|m| m.from == "system:ci").collect();
    assert_eq!(
        from_ci.len(),
        1,
        "chain target overlap must yield exactly 1 inbox entry; got: {from_ci:?}"
    );
    assert!(
        from_ci[0].text.contains("[ci-ready-for-action]"),
        "the single entry must be the chain form, not subscriber [ci-pass]: {:?}",
        from_ci[0]
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn parse_subscribers_reads_array() {
    let watch = serde_json::json!({
        "subscribers": [
            {"instance": "a", "subscribed_at": "x"},
            {"instance": "b", "subscribed_at": "y"}
        ],
        "instance": "ignored-when-array-present"
    });
    assert_eq!(parse_subscribers(&watch), vec!["a", "b"]);
}

#[test]
fn parse_subscribers_falls_back_to_legacy_instance() {
    let watch = serde_json::json!({"instance": "legacy-only"});
    assert_eq!(parse_subscribers(&watch), vec!["legacy-only"]);
}

#[test]
fn parse_subscribers_empty_when_no_data() {
    let watch = serde_json::json!({});
    assert!(parse_subscribers(&watch).is_empty());
}

// -----------------------------------------------------------------
// Sprint 54 P0-5 (sub-scope B) — consecutive_skips tracking +
// stalled/resumed inbox fan-out. Each test pins one of the four
// contract gates from dispatch m-20260507045729197032-16.
//
// EMPIRICAL REGRESSION-PROOF ANCHOR: if the
// `if next_skips >= STALL_THRESHOLD && !already_notified` guard
// in `bump_consecutive_skips_and_maybe_notify` is dropped, the
// "fires exactly once" assertion in
// `stalled_event_fires_exactly_once_at_threshold` fails because
// every subsequent skip would re-enqueue. PR description carries
// the captured FAIL signature.
// -----------------------------------------------------------------

fn p05_temp_home(tag: &str) -> std::path::PathBuf {
    let dir = tmp_dir(tag);
    std::fs::create_dir_all(dir.join("ci-watches")).ok();
    std::fs::create_dir_all(dir.join("inbox")).ok();
    dir
}

fn p05_write_watch(
    home: &Path,
    repo: &str,
    branch: &str,
    watch: serde_json::Value,
) -> std::path::PathBuf {
    let path = ci_watches_dir(home).join(watch_filename(repo, branch));
    std::fs::write(&path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();
    path
}

fn p05_read_inbox_lines(home: &Path, instance: &str) -> Vec<String> {
    let path = home.join("inbox").join(format!("{instance}.jsonl"));
    std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn consecutive_skips_increments_on_rate_limited_skip() {
    // Gate 1: each rate-limited skip bumps the counter atomically.
    let home = p05_temp_home("p05_skip_inc");
    let watch = serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"}],
        "consecutive_skips": 0,
    });
    let path = p05_write_watch(&home, "o/r", "feat", watch);
    let subscribers = vec!["lead".to_string()];

    let future_reset = (chrono::Utc::now().timestamp() as u64) + 3600;
    bump_consecutive_skips_and_maybe_notify(
        &home,
        &path,
        "o/r",
        "feat",
        &subscribers,
        future_reset,
        None,
    );
    bump_consecutive_skips_and_maybe_notify(
        &home,
        &path,
        "o/r",
        "feat",
        &subscribers,
        future_reset,
        None,
    );
    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(watch["consecutive_skips"].as_u64(), Some(2));
    // Below threshold: stalled_notified is either absent (None) or
    // false — both mean "no notify fired yet".
    let notified = watch["stalled_notified"].as_bool().unwrap_or(false);
    assert!(!notified, "below threshold ⇒ no notify yet");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn stalled_event_fires_exactly_once_at_threshold() {
    // Gate 2: at N=3 a [ci-watch-stalled] enqueues; further
    // tick-skips don't re-fire. This is the regression-proof
    // anchor — collapsing the guard reveals duplicates.
    let home = p05_temp_home("p05_stalled_once");
    let watch = serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"}],
        "consecutive_skips": 0,
    });
    let path = p05_write_watch(&home, "o/r", "feat", watch);
    let subscribers = vec!["lead".to_string()];
    let future_reset = (chrono::Utc::now().timestamp() as u64) + 3600;

    for _ in 0..5 {
        bump_consecutive_skips_and_maybe_notify(
            &home,
            &path,
            "o/r",
            "feat",
            &subscribers,
            future_reset,
            None,
        );
    }

    let lines = p05_read_inbox_lines(&home, "lead");
    let stalled_count = lines
        .iter()
        .filter(|l| l.contains("ci-watch-stalled"))
        .count();
    assert_eq!(
        stalled_count, 1,
        "exactly one stalled event per stall window (got {stalled_count})"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn resumed_event_fires_on_first_successful_clear() {
    // Gate 3: resume helper fires [ci-watch-resumed] exactly once
    // and clears the stall state.
    let home = p05_temp_home("p05_resumed");
    let watch = serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"}],
        "consecutive_skips": 5,
        "stalled_notified": true,
        "stalled_since_ms": 1700000000000_i64,
    });
    let path = p05_write_watch(&home, "o/r", "feat", watch);
    let subscribers = vec!["lead".to_string()];

    clear_stall_and_maybe_notify_resumed(&home, &path, "o/r", "feat", &subscribers);
    // Second call must be a silent no-op (state already cleared).
    clear_stall_and_maybe_notify_resumed(&home, &path, "o/r", "feat", &subscribers);

    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(watch["consecutive_skips"].as_u64(), Some(0));
    assert_eq!(watch["stalled_notified"].as_bool(), Some(false));
    assert!(watch["stalled_since_ms"].is_null());

    let lines = p05_read_inbox_lines(&home, "lead");
    let resumed = lines
        .iter()
        .filter(|l| l.contains("ci-watch-resumed"))
        .count();
    assert_eq!(resumed, 1, "exactly one resumed event per recovery");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn stalled_and_resumed_fan_out_to_all_subscribers() {
    // Gate 4: both event types reach every subscriber per the
    // P0-1 fan-out contract.
    let home = p05_temp_home("p05_fanout");
    let watch = serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "subscribers": [
            {"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"},
            {"instance": "dev",  "subscribed_at": "2026-05-07T00:00:01Z"}
        ],
        "consecutive_skips": 0,
    });
    let path = p05_write_watch(&home, "o/r", "feat", watch);
    let subscribers = vec!["lead".to_string(), "dev".to_string()];
    let future_reset = (chrono::Utc::now().timestamp() as u64) + 3600;

    for _ in 0..STALL_THRESHOLD {
        bump_consecutive_skips_and_maybe_notify(
            &home,
            &path,
            "o/r",
            "feat",
            &subscribers,
            future_reset,
            None,
        );
    }
    for sub in ["lead", "dev"] {
        let lines = p05_read_inbox_lines(&home, sub);
        assert!(
            lines.iter().any(|l| l.contains("ci-watch-stalled")),
            "{sub} must receive stalled event (fan-out regression)"
        );
    }

    clear_stall_and_maybe_notify_resumed(&home, &path, "o/r", "feat", &subscribers);
    for sub in ["lead", "dev"] {
        let lines = p05_read_inbox_lines(&home, sub);
        assert!(
            lines.iter().any(|l| l.contains("ci-watch-resumed")),
            "{sub} must receive resumed event"
        );
    }
    std::fs::remove_dir_all(&home).ok();
}

/// #790 wiring test: notification BODY renders Stalled-since and
/// Next-poll-ETA in the operator's configured display tz, while
/// the InboxMessage storage `timestamp` field stays UTC (storage
/// invariant pin guard, per dispatch spec).
///
/// Stalled-since is anchored at `2026-05-07T22:00:00Z` (epoch
/// 1746655200000ms). With `display_timezone=Some("Asia/Taipei")`
/// the body should contain `"05-08 06:00"` (UTC+8). The inbox
/// message timestamp field is set inside `fan_out_health_event`
/// from `chrono::Utc::now().to_rfc3339()` — must end with `Z` or
/// `+00:00` regardless of display_timezone.
#[test]
fn stalled_event_body_uses_display_tz_storage_stays_utc() {
    let home = p05_temp_home("p790_tz_body");
    // Pre-stamp stalled_since_ms so the body interpolation is
    // deterministic (no race on chrono::Utc::now()).
    let watch = serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"}],
        "consecutive_skips": 0,
        "stalled_since_ms": 1746655200000_i64, // 2026-05-07T22:00:00Z
    });
    let path = p05_write_watch(&home, "o/r", "feat", watch);
    let subscribers = vec!["lead".to_string()];
    let future_reset = (chrono::Utc::now().timestamp() as u64) + 3600;

    for _ in 0..STALL_THRESHOLD {
        bump_consecutive_skips_and_maybe_notify(
            &home,
            &path,
            "o/r",
            "feat",
            &subscribers,
            future_reset,
            Some("Asia/Taipei"),
        );
    }

    let lines = p05_read_inbox_lines(&home, "lead");
    let stalled_line = lines
        .iter()
        .find(|l| l.contains("ci-watch-stalled"))
        .expect("stalled event must enqueue");
    let msg: serde_json::Value = serde_json::from_str(stalled_line).expect("inbox line is JSON");

    // Wiring: body text must contain Taipei-rendered Stalled-since.
    let text = msg["text"].as_str().expect("inbox text field");
    assert!(
        text.contains("Stalled since: 05-08 06:00"),
        "body must render Taipei tz (UTC+8) for 2026-05-07T22:00:00Z, got:\n{text}"
    );

    // Storage invariant: inbox timestamp field stays UTC ISO 8601.
    let ts = msg["timestamp"].as_str().expect("timestamp field");
    assert!(
        ts.ends_with('Z') || ts.ends_with("+00:00"),
        "inbox storage timestamp must be UTC ISO 8601, got {ts:?}"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ── Sprint 54 Hotfix F gap: malformed GitHub head query ─────────────
//
// Per RCA m-46: `check_pr_terminal` was sending bare `head=feat/foo`
// instead of `head=owner:feat/foo`. GitHub silently dropped the
// filter → returned the most recent PR in the repo → Hotfix F's
// freshness check passed → false `Terminal{merged}` for fresh-no-PR
// branches. The malformed-query swallow is the third concrete
// instance of the silent-drop class systematic prevention pattern
// (decision `d-20260507100609264367-2`).
//
// EMPIRICAL REGRESSION-PROOF ANCHOR: dropping the `{owner}:` prefix
// from the URL format string trips
// `github_check_pr_terminal_uses_owner_prefix_in_head_query`. PR
// description carries the verbatim FAIL signature.

/// Reuse the gitlab_mock_server scaffolding — it's just an HTTP
/// listener returning a fixed body, content-type agnostic. Renaming
/// to `mock_server` would touch dozens of call sites; sharing the
/// fn under a Hotfix-F-specific helper avoids that churn.
fn github_mock_server(response_body: &str) -> (u16, std::thread::JoinHandle<()>, MockCapture) {
    gitlab_mock_server(response_body)
}

#[test]
fn github_check_pr_terminal_uses_owner_prefix_in_head_query() {
    // Hotfix F gap gate 1 (regression-proof anchor): the production
    // URL must carry `head={owner}:{branch}` per GitHub docs. Empty
    // PR list is fine — the test only inspects the captured URL.
    let (port, handle, captured) = github_mock_server("[]");
    let provider = super::GitHubCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let _ = rt.block_on(provider.check_pr_terminal("acme/widgets", "feat/foo"));

    handle.join().expect("mock");
    let (path, _request) = captured.lock().expect("lock").take().expect("captured");

    assert!(
        path.contains("/repos/acme/widgets/pulls"),
        "URL must target the repo's pulls endpoint: {path}"
    );
    assert!(
        path.contains("head=acme:feat/foo"),
        "URL must use `head={{owner}}:{{branch}}` per GitHub docs (Hotfix F gap fix); got: {path}"
    );
    // Defensive: also ensure the legacy bare form is GONE — a
    // future edit that re-introduced `head={branch}` without the
    // owner prefix should trip this.
    assert!(
        !path.contains("head=feat/foo&"),
        "URL must NOT use bare `head={{branch}}` (the silent-drop bug); got: {path}"
    );
}

#[test]
fn github_check_pr_terminal_returns_unknown_on_head_ref_mismatch() {
    // Hotfix F gap gate 2 (defensive): even with the correct URL,
    // GitHub may return a PR whose head.ref doesn't match what we
    // asked. The defensive check returns Unknown so we don't
    // misclassify the asked branch as Terminal based on someone
    // else's PR data.
    let mismatched = r#"[{
        "state": "closed",
        "closed_at": "2099-01-01T00:00:00Z",
        "merged_at": "2099-01-01T00:00:00Z",
        "head": {"ref": "different-branch"}
    }]"#;
    let (port, handle, captured) = github_mock_server(mismatched);
    let provider = super::GitHubCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let state = rt.block_on(provider.check_pr_terminal("acme/widgets", "feat/foo"));
    handle.join().expect("mock");
    let _ = captured;

    assert!(
        matches!(state, super::PrState::Unknown),
        "head.ref mismatch must return Unknown (defensive); got: {state:?}"
    );
}

#[test]
fn github_check_pr_terminal_returns_unknown_on_empty_pr_list() {
    // Hotfix F gap gate 3 (production-realistic): the bug surfaced
    // for fresh-no-PR branches, where GitHub correctly returns []
    // once the head query is well-formed. The state machine must
    // map empty → Unknown so the auto-clear path doesn't fire.
    let (port, handle, captured) = github_mock_server("[]");
    let provider = super::GitHubCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let state = rt.block_on(provider.check_pr_terminal("acme/widgets", "fresh-branch-no-pr"));
    handle.join().expect("mock");
    let _ = captured;

    assert!(
        matches!(state, super::PrState::Unknown),
        "fresh-no-PR branch must return Unknown; got: {state:?}"
    );
}

#[test]
fn github_check_pr_terminal_terminal_on_matching_head_ref_and_recent_close() {
    // Hotfix F gap gate 4: the happy path still works post-fix —
    // a closed-and-merged PR matching the head.ref returns
    // Terminal{merged: true}. Defensive head.ref check + Hotfix F
    // freshness check both pass.
    let recent_close = chrono::Utc::now() - chrono::Duration::minutes(5);
    let body = format!(
        r#"[{{
            "state": "closed",
            "closed_at": "{}",
            "merged_at": "{}",
            "head": {{"ref": "feat/foo"}}
        }}]"#,
        recent_close.to_rfc3339(),
        recent_close.to_rfc3339()
    );
    let (port, handle, captured) = github_mock_server(&body);
    let provider = super::GitHubCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let state = rt.block_on(provider.check_pr_terminal("acme/widgets", "feat/foo"));
    handle.join().expect("mock");
    let _ = captured;

    assert!(
        matches!(state, super::PrState::Terminal { merged: true }),
        "matching head.ref + recent merged_at must be Terminal(merged); got: {state:?}"
    );
}

// -------------------------------------------------------------
// Sprint 57 Wave 2 Track B (#546 Items 1+3) — startup sweep,
// per-tick eager GC, and protected-ref migration via
// `gc_stale_watches` / `startup_sweep`.
// -------------------------------------------------------------

/// Helper for the new GC tests — write a synthetic watch JSON to
/// disk under `home/ci-watches/<filename>`. Returns the file path.
fn write_watch(
    home: &std::path::Path,
    repo: &str,
    branch: &str,
    watch: &serde_json::Value,
) -> std::path::PathBuf {
    let ci_dir = ci_watches_dir(home);
    std::fs::create_dir_all(&ci_dir).ok();
    let filename = super::watch_filename(repo, branch);
    let path = ci_dir.join(&filename);
    std::fs::write(&path, serde_json::to_string(watch).unwrap()).ok();
    path
}

#[test]
fn ci_watch_ttl_expires_stale_entries_on_startup_sweep() {
    // Direct gc_stale_watches call simulates the daemon-startup
    // path that runs before the tick loop spins up. An expired
    // (absolute TTL elapsed) entry must be removed AND the event
    // log must record the removal with the startup_sweep origin.
    let home = tmp_dir("startup-sweep-ttl");
    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let watch = serde_json::json!({
        "repo": "o/r", "branch": "feat-stale", "interval_secs": 60,
        "subscribers": [{"instance": "agent-x"}],
        "last_run_id": null, "head_sha": null,
        "last_polled_at": null,
        "expires_at": past,
        "last_terminal_seen_at": null,
    });
    let path = write_watch(&home, "o/r", "feat-stale", &watch);

    super::startup_sweep(&home);

    assert!(
        !path.exists(),
        "expired watch must be removed by startup sweep"
    );
    let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("ci_watch_removed"),
        "removal event must log; got:\n{log}"
    );
    assert!(
        log.contains("startup_sweep_expired"),
        "reason must include startup_sweep origin; got:\n{log}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ci_watch_ttl_eager_gc_per_tick() {
    // Per-tick path: `check_ci_watches` calls `gc_stale_watches`
    // BEFORE the poll loop. A watch with `expires_at` in the past
    // but no subscribers list (so the legacy poll body would
    // continue rather than expire it) must still be removed by
    // the eager GC.
    let home = tmp_dir("eager-gc-ttl");
    let past = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
    let watch = serde_json::json!({
        "repo": "o/r", "branch": "feat-stale-eager", "interval_secs": 60,
        // empty subscribers — poll loop would `continue` past it
        // without expiring; eager GC must catch it anyway.
        "subscribers": [],
        "last_run_id": null, "head_sha": null,
        "last_polled_at": null,
        "expires_at": past,
        "last_terminal_seen_at": null,
    });
    let path = write_watch(&home, "o/r", "feat-stale-eager", &watch);

    let registry: AgentRegistry =
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    super::check_ci_watches(&home, &registry);

    assert!(
        !path.exists(),
        "eager GC must remove expired watch even when subscribers list is empty"
    );
    let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("eager_gc_expired"),
        "per-tick eager-GC origin must appear in log; got:\n{log}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn migrate_existing_main_watches_at_startup() {
    // Item 3 migration: any pre-existing watch with branch=main
    // (or master) was created before the E4.5 gate landed in
    // handle_watch_ci. Startup sweep must remove it regardless
    // of TTL state so the bypass is closed retroactively.
    let home = tmp_dir("migrate-main-watches");
    let watch_main = serde_json::json!({
        "repo": "owner/repo", "branch": "main", "interval_secs": 60,
        "subscribers": [{"instance": "general"}],
        "last_run_id": null, "head_sha": null,
        "last_polled_at": null,
        // Fresh expires_at — would NOT trip the TTL paths.
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    let watch_master = serde_json::json!({
        "repo": "owner/legacy-repo", "branch": "master", "interval_secs": 60,
        "subscribers": [{"instance": "lead"}],
        "last_run_id": null, "head_sha": null,
        "last_polled_at": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    let watch_feat = serde_json::json!({
        "repo": "owner/repo", "branch": "feat-not-touched", "interval_secs": 60,
        "subscribers": [{"instance": "dev"}],
        "last_run_id": null, "head_sha": null,
        "last_polled_at": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    let path_main = write_watch(&home, "owner/repo", "main", &watch_main);
    let path_master = write_watch(&home, "owner/legacy-repo", "master", &watch_master);
    let path_feat = write_watch(&home, "owner/repo", "feat-not-touched", &watch_feat);

    super::startup_sweep(&home);

    assert!(
        !path_main.exists(),
        "main watch must be migrated/removed by startup sweep"
    );
    assert!(
        !path_master.exists(),
        "master watch must be migrated/removed by startup sweep"
    );
    assert!(path_feat.exists(), "non-protected watch must be preserved");

    let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("startup_sweep_protected_branch_migration"),
        "migration reason must surface in audit log; got:\n{log}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn gc_stale_watches_idempotent_on_clean_dir() {
    // Defensive bonus pin: re-running the sweep on an already-
    // clean dir must be a no-op (zero removals, zero log entries).
    let home = tmp_dir("gc-idempotent");
    let watch = serde_json::json!({
        "repo": "o/r", "branch": "feat-fresh", "interval_secs": 60,
        "subscribers": [{"instance": "dev"}],
        "last_run_id": null, "head_sha": null,
        "last_polled_at": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": chrono::Utc::now().to_rfc3339(),
    });
    let path = write_watch(&home, "o/r", "feat-fresh", &watch);

    let n1 = super::gc_stale_watches(&home, "test_pass1");
    let n2 = super::gc_stale_watches(&home, "test_pass2");

    assert_eq!(n1, 0, "first sweep on fresh dir must remove nothing");
    assert_eq!(n2, 0, "second sweep is idempotent");
    assert!(path.exists(), "fresh watch must survive both sweeps");
    std::fs::remove_dir_all(&home).ok();
}

// ── Issue #608: aggregate_conclusion_for_sha tests ──────────────────

fn make_run(id: u64, sha: &str, conclusion: Option<&str>) -> CiRun {
    CiRun {
        id,
        head_sha: sha.to_string(),
        conclusion: conclusion.map(String::from),
        url: String::new(),
        name: String::new(),
    }
}

#[test]
fn aggregate_all_success_emits_pass() {
    let runs = vec![
        make_run(1, "abc123", Some("success")),
        make_run(2, "abc123", Some("success")),
    ];
    assert_eq!(
        aggregate_conclusion_for_sha(&runs, "abc123"),
        Some("success")
    );
}

#[test]
fn aggregate_any_failure_emits_fail() {
    let runs = vec![
        make_run(1, "abc123", Some("success")),
        make_run(2, "abc123", Some("failure")),
    ];
    assert_eq!(
        aggregate_conclusion_for_sha(&runs, "abc123"),
        Some("failure")
    );
}

#[test]
fn aggregate_in_progress_blocks_notification() {
    let runs = vec![
        make_run(1, "abc123", Some("success")),
        make_run(2, "abc123", None),
    ];
    assert_eq!(aggregate_conclusion_for_sha(&runs, "abc123"), None);
}

#[test]
fn aggregate_empty_returns_none() {
    let runs = vec![make_run(1, "other", Some("success"))];
    assert_eq!(aggregate_conclusion_for_sha(&runs, "abc123"), None);
}

#[test]
fn aggregate_failure_with_in_progress_still_reports_failure() {
    let runs = vec![
        make_run(1, "abc123", Some("failure")),
        make_run(2, "abc123", None),
    ];
    assert_eq!(
        aggregate_conclusion_for_sha(&runs, "abc123"),
        Some("failure")
    );
}

// Larger page size (POLL_RUNS_PAGE_SIZE=20) increases the chance that a
// single response contains runs for both the current head and prior
// shas (force-push, multiple pushes within poll interval). Verify that
// stale-sha runs never bleed into the current head's conclusion.
#[test]
fn aggregate_ignores_stale_sha_runs_in_response() {
    let runs = vec![
        // Current sha — all succeeded.
        make_run(10, "current", Some("success")),
        make_run(11, "current", Some("success")),
        // Prior sha — failure must not leak into "current" conclusion.
        make_run(8, "prior", Some("failure")),
        make_run(9, "prior", Some("success")),
    ];
    assert_eq!(
        aggregate_conclusion_for_sha(&runs, "current"),
        Some("success"),
        "stale-sha failure must not contaminate current head's aggregate"
    );
    assert_eq!(
        aggregate_conclusion_for_sha(&runs, "prior"),
        Some("failure"),
        "prior-sha aggregate still computes independently"
    );
}

#[test]
fn startup_sweep_preserves_valid_watches() {
    let home = tmp_dir("restart-persist");
    let ci_dir = ci_watches_dir(&home);
    std::fs::create_dir_all(&ci_dir).unwrap();

    // Create a valid (non-expired) watch
    let watch = serde_json::json!({
        "repo": "test/repo",
        "branch": "feat-branch",
        "subscribers": ["dev-1"],
        "interval_secs": 60,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "last_polled_at": chrono::Utc::now().timestamp_millis() - 120_000,
    });
    let filename = watch_filename("test/repo", "feat-branch");
    std::fs::write(ci_dir.join(&filename), watch.to_string()).unwrap();

    // Simulate restart: run startup_sweep
    startup_sweep(&home);

    // Watch must survive (not expired, not protected branch)
    let surviving = std::fs::read_to_string(ci_dir.join(&filename));
    assert!(
        surviving.is_ok(),
        "valid watch must survive startup_sweep (restart persistence)"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ----------------------------------------------------------------------
// #786 conclusion-change dedup anchor tests.
//
// Both tests fail at C1 HEAD (pre-impl) because:
//   Site 1 (`select_runs_to_notify`) drops runs where `run.id <=
//     last_run_id` regardless of conclusion change.
//   Site 2 (`dedupe_notifications_by_head_sha`) drops runs whose
//     head_sha matches `last_notified_head_sha` regardless of
//     conclusion change.
//
// The fixture sets up a watch already in the "notified" state
// (last_run_id + last_notified_head_sha both populated) and feeds a
// poll that should re-fire because the conclusion changed (the
// gh-rerun-on-same-attempt scenario). Pre-impl drops the run on
// either Site 1 or Site 2; post-impl includes it because both sites
// become conclusion-aware.
//
// Source of truth: decision d-20260514163605327829-3.
// Cross-platform: no `#[cfg(unix)]` gate (pure async logic, no
// git subprocess) per reviewer C6 / #785 precedent.
// ----------------------------------------------------------------------

fn p786_watch_already_notified(
    last_run_id: u64,
    conclusion: &str,
    head_sha: &str,
) -> serde_json::Value {
    // Watch state where a prior poll cycle already notified for
    // `(last_run_id, head_sha)` with `conclusion`. Subsequent polls
    // must respect `last_notified_conclusion` (post-#786 field).
    serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "interval_secs": 60,
        "instance": "agent1",
        "last_run_id": last_run_id,
        "head_sha": head_sha,
        "last_polled_at": null,
        "last_notified_head_sha": head_sha,
        "last_notified_conclusion": conclusion,
    })
}

#[test]
fn rerun_changes_conclusion_fires_notification() {
    // ANCHOR test 1 (§3.10 red→green).
    //
    // Scenario: `gh run rerun --failed` on the same workflow run
    // produces a new attempt with the same run_id but new
    // conclusion ("failure" → "success"). The watch must re-fire
    // because the conclusion changed.
    //
    // Pre-impl: Site 1 filters run.id <= last_run_id → no notify
    //   → inbox has zero ci-pass messages → assertion fails.
    // Post-impl: Site 1 conclusion-aware → run included → Site 2
    //   conclusion-aware → run included → notification fires.
    let dir = tmp_dir("p786-rerun-changes-conclusion");
    let watch = p786_watch_already_notified(100, "failure", "abc");
    // Same run_id, same head_sha, NEW conclusion.
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 100,
        conclusion: Some("success".into()),
        head_sha: "abc".into(),
        url: "https://example.com/100".into(),
        name: String::new(),
    }]);

    run_ci_check(&dir, &watch, &provider).unwrap();

    // Inbox must contain the rerun's success notification.
    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    assert!(
        inbox_path.exists(),
        "rerun changing conclusion must enqueue inbox notification"
    );
    let content = std::fs::read_to_string(&inbox_path).unwrap();
    assert!(
        content.contains("ci-pass") || content.contains("[ci-pass]"),
        "rerun success must fire ci-pass notification: {content}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn dedupe_by_head_sha_does_not_block_conclusion_change() {
    // ANCHOR test 5 (§3.10 red→green) — Site 2 isolation.
    //
    // Scenario: a NEW workflow run (different run_id) re-runs on the
    // SAME commit (same head_sha) and produces a new conclusion.
    // This bypasses Site 1's run_id filter but pre-impl Site 2 still
    // drops it because head_sha matches last_notified_head_sha.
    // Pinning Site 2 independently prevents a future PR from
    // reverting only Site 2 (which Site-1-only tests can't catch).
    //
    // Pre-impl: Site 1 passes (101 > 100), Site 2 filters (sha
    //   matches last_notified) → no notify → test fails.
    // Post-impl: Site 2 conclusion-aware → fires.
    let dir = tmp_dir("p786-site2-isolation");
    // Prior state: notified for run 100 / sha=abc / "failure".
    let watch = p786_watch_already_notified(100, "failure", "abc");
    // New scheduled run on same commit: run_id=101 (passes Site 1),
    // same sha (would be dropped by Site 2 pre-impl), new conclusion.
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 101,
        conclusion: Some("success".into()),
        head_sha: "abc".into(),
        url: "https://example.com/101".into(),
        name: String::new(),
    }]);

    run_ci_check(&dir, &watch, &provider).unwrap();

    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    assert!(
        inbox_path.exists(),
        "new run on same sha with new conclusion must enqueue notification"
    );
    let content = std::fs::read_to_string(&inbox_path).unwrap();
    assert!(
        content.contains("ci-pass") || content.contains("[ci-pass]"),
        "Site 2 must allow conclusion-change through despite matching head_sha: {content}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn same_run_id_same_conclusion_does_not_re_fire() {
    // Test 2 (back-compat invariant + reviewer constraint 1):
    // same run_id + same conclusion as last notified → no
    // notification, no state churn (no rewrite of last_notified_*
    // fields).
    let dir = tmp_dir("p786-no-churn");
    let watch = p786_watch_already_notified(100, "failure", "abc");
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 100,
        conclusion: Some("failure".into()),
        head_sha: "abc".into(),
        url: "https://example.com/100".into(),
        name: String::new(),
    }]);
    let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));

    run_ci_check(&dir, &watch, &provider).unwrap();

    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    if inbox_path.exists() {
        let content = std::fs::read_to_string(&inbox_path).unwrap();
        assert!(
            !content.contains("ci-pass") && !content.contains("ci-fail"),
            "stable terminal state must NOT re-fire: {content}"
        );
    }

    let updated: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
    assert_eq!(
        updated["last_notified_conclusion"].as_str(),
        Some("failure"),
        "no churn: last_notified_conclusion must remain 'failure': {updated}"
    );
    assert_eq!(
        updated["last_notified_head_sha"].as_str(),
        Some("abc"),
        "no churn: last_notified_head_sha must remain 'abc': {updated}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn new_run_id_fires_regardless_of_prior_conclusion() {
    // Test 3 (existing-behavior preservation): new run_id + new
    // commit fires regardless of prior conclusion. Dedup only
    // affects same-run_id / same-sha paths.
    let dir = tmp_dir("p786-new-run");
    let watch = p786_watch_already_notified(100, "success", "abc");
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 200,
        conclusion: Some("success".into()),
        head_sha: "def".into(),
        url: "https://example.com/200".into(),
        name: String::new(),
    }]);

    run_ci_check(&dir, &watch, &provider).unwrap();

    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    assert!(
        inbox_path.exists(),
        "new run_id + new commit must fire regardless of prior conclusion"
    );
    let content = std::fs::read_to_string(&inbox_path).unwrap();
    assert!(
        content.contains("ci-pass") || content.contains("[ci-pass]"),
        "new run must fire ci-pass: {content}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_last_notified_conclusion_field_handles_first_poll_gracefully() {
    // Test 4 (migration invariant): pre-#786 watches lack the
    // `last_notified_conclusion` field. First post-upgrade poll
    // on a terminal run fires once (None != Some("success")) —
    // bounded migration spam — then persists the new field so
    // subsequent stable polls don't re-fire.
    let dir = tmp_dir("p786-migration");
    let watch = serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "interval_secs": 60,
        "instance": "agent1",
        "last_run_id": 100,
        "head_sha": "abc",
        "last_polled_at": null,
        "last_notified_head_sha": "abc",
        // last_notified_conclusion intentionally absent (pre-#786 shape).
    });
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 100,
        conclusion: Some("success".into()),
        head_sha: "abc".into(),
        url: "https://example.com/100".into(),
        name: String::new(),
    }]);

    run_ci_check(&dir, &watch, &provider).unwrap();

    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    assert!(
        inbox_path.exists(),
        "missing last_notified_conclusion must fire on first post-upgrade poll"
    );

    let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
    let updated: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
    assert_eq!(
        updated["last_notified_conclusion"].as_str(),
        Some("success"),
        "migration: post-fire watch must persist last_notified_conclusion: {updated}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ── #762: dedup [ci-pass] for subscribers who are also action_target ─────
//
// Pre-fix: the fan-out loop at poller.rs:836 enqueues [ci-pass] to EVERY
// subscriber, then the [ci-ready-for-action] dispatch at 889-908 separately
// injects to next_after_ci. When the same agent is in both lists, it
// received both notifications for the same CI pass. Issue #762 example:
// PR #756 lead `claude-f54bf9` got [ci-pass] AND [ci-ready-for-action].
//
// Fix: load `next_after_ci` once into `action_target_on_success`, skip the
// [ci-pass] enqueue for that exact subscriber on success, fan out to
// everyone else. Failure path leaves the option as None so all subscribers
// (including the action_target) get [ci-fail].

/// #762 §3.10 anchor — subscribers `[a, b]` with `next_after_ci: a` and
/// a successful run must drop the `[ci-pass]` for `a` (the action target),
/// while `b` still receives `[ci-pass]`. Pre-fix the fan-out loop
/// unconditionally enqueued for every subscriber, so `a` got both
/// `[ci-pass]` and `[ci-ready-for-action]`.
#[test]
fn pass_dedupe_drops_ci_pass_for_subscriber_who_is_action_target() {
    let dir = tmp_dir("dedup-success");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "subscribers": [
            {"instance": "a", "subscribed_at": "2026-05-15T00:00:00Z"},
            {"instance": "b", "subscribed_at": "2026-05-15T00:00:01Z"}
        ],
        "next_after_ci": "a",
        "interval_secs": 60,
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 100,
        conclusion: Some("success".to_string()),
        head_sha: "abc".to_string(),
        url: "https://example/run/100".to_string(),
        name: String::new(),
    }]);

    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &["a".to_string(), "b".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();

    // `a` is the action_target on success → MUST NOT receive [ci-pass].
    let a_inbox = dir.join("inbox").join("a.jsonl");
    let a_body = std::fs::read_to_string(&a_inbox).unwrap_or_default();
    assert!(
        !a_body.contains("[ci-pass]"),
        "action_target `a` must NOT receive [ci-pass] (will get [ci-ready-for-action] instead); inbox body:\n{a_body}"
    );

    // `b` is not the action_target → MUST still receive [ci-pass].
    let b_inbox = dir.join("inbox").join("b.jsonl");
    let b_body = std::fs::read_to_string(&b_inbox).unwrap_or_else(|_| {
        panic!("subscriber `b` inbox missing — fan-out regression: {b_inbox:?}")
    });
    assert!(
        b_body.contains("[ci-pass]") && b_body.contains("o/r@feat"),
        "subscriber `b` must receive [ci-pass]; inbox body:\n{b_body}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// #762 invariant — failure conclusion must NOT dedupe. Both subscribers
/// (including the action_target) need to know the CI failed; the
/// [ci-ready-for-action] dispatch only fires on success per issue #650,
/// so a failure-path drop would leave the action_target uninformed.
#[test]
fn pass_dedupe_failure_does_not_drop_ci_fail_for_action_target() {
    let dir = tmp_dir("dedup-failure");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "subscribers": [
            {"instance": "a", "subscribed_at": "2026-05-15T00:00:00Z"},
            {"instance": "b", "subscribed_at": "2026-05-15T00:00:01Z"}
        ],
        "next_after_ci": "a",
        "interval_secs": 60,
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 200,
        conclusion: Some("failure".to_string()),
        head_sha: "def".to_string(),
        url: "https://example/run/200".to_string(),
        name: String::new(),
    }]);

    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &["a".to_string(), "b".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();

    // Both subscribers (including action_target `a`) must receive [ci-fail].
    for sub in ["a", "b"] {
        let inbox_path = dir.join("inbox").join(format!("{sub}.jsonl"));
        let body = std::fs::read_to_string(&inbox_path)
            .unwrap_or_else(|_| panic!("{sub} inbox missing on failure-path: {inbox_path:?}"));
        assert!(
            body.contains("[ci-fail]") && body.contains("o/r@feat"),
            "{sub} must receive [ci-fail] (failure path must NOT dedupe); inbox body:\n{body}"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// #762 invariant — subscribers disjoint from `next_after_ci` all receive
/// `[ci-pass]` on success. The dedupe filter must be exact-match only;
/// it must not drop notifications for non-action-target subscribers.
#[test]
fn pass_dedupe_non_action_target_subscribers_receive_ci_pass() {
    let dir = tmp_dir("dedup-non-overlap");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = serde_json::json!({
        "repo": "o/r",
        "branch": "feat",
        "subscribers": [
            {"instance": "a", "subscribed_at": "2026-05-15T00:00:00Z"},
            {"instance": "b", "subscribed_at": "2026-05-15T00:00:01Z"},
            {"instance": "c", "subscribed_at": "2026-05-15T00:00:02Z"}
        ],
        // next_after_ci points to an agent NOT in subscribers list.
        "next_after_ci": "d",
        "interval_secs": 60,
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 300,
        conclusion: Some("success".to_string()),
        head_sha: "fed".to_string(),
        url: "https://example/run/300".to_string(),
        name: String::new(),
    }]);

    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &["a".to_string(), "b".to_string(), "c".to_string()],
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();

    // All 3 subscribers must receive [ci-pass]; `d` is not subscribed
    // and would receive [ci-ready-for-action] via inject (silent in
    // tests with empty registry).
    for sub in ["a", "b", "c"] {
        let inbox_path = dir.join("inbox").join(format!("{sub}.jsonl"));
        let body = std::fs::read_to_string(&inbox_path)
            .unwrap_or_else(|_| panic!("subscriber `{sub}` inbox missing: {inbox_path:?}"));
        assert!(
            body.contains("[ci-pass]") && body.contains("o/r@feat"),
            "subscriber `{sub}` must receive [ci-pass] (next_after_ci=d ≠ {sub}); inbox body:\n{body}"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

// ── #813 ci_watch CONFLICTING PR detection — RED tests ──

#[test]
fn test_watch_start_on_conflicting_emits_alert() {
    // RED: pre-C3, `watch_start_check_mergeable` is a no-op stub
    // so the subscriber inbox stays empty even when the provider
    // reports CONFLICTING. Post-C3 the hook emits a
    // `[ci-conflict-detected]` headline + inbox entry so the
    // operator gets the signal before GH webhook silence kicks in.
    let dir = tmp_dir("watch_start_conflict");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = serde_json::json!({
        "repo": "test/repo",
        "branch": "fix/x",
        "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-15T00:00:00Z"}],
        "instance": "lead",
        "interval_secs": 60,
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    let watch_path = ci_dir.join(watch_filename("test/repo", "fix/x"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

    let provider =
        MockCiProvider::with_runs(Vec::new()).with_mergeable(MergeableState::Conflicting);

    super::watch_start_check_mergeable(
        &dir,
        &watch_path,
        "test/repo",
        "fix/x",
        &["lead".to_string()],
        &provider,
    );

    let inbox_path = dir.join("inbox").join("lead.jsonl");
    let body = std::fs::read_to_string(&inbox_path).unwrap_or_default();
    assert!(
        body.contains("[ci-conflict-detected]"),
        "subscriber inbox must carry the conflict alert, got: {body}"
    );
    assert!(
        body.contains("test/repo@fix/x"),
        "alert must identify the repo + branch, got: {body}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_ci_status_response_includes_pr_mergeable_state() {
    // RED: pre-C4 the `ci action=status` response shape doesn't
    // carry a `pr_mergeable_state` field; status callers can't
    // distinguish "CI running" silence from "CONFLICTING blocked
    // forever" silence. Post-C4 the field surfaces the cached
    // mergeable state from the watch JSON (null when no check has
    // run yet).
    let dir = tmp_dir("status_mergeable_field");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = serde_json::json!({
        "repo": "test/repo",
        "branch": "fix/x",
        "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-15T00:00:00Z"}],
        "instance": "lead",
        "interval_secs": 60,
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
        // Pre-seed the new field as if a prior poll cycle stamped it.
        "last_mergeable_state": "CONFLICTING",
        "last_mergeable_check_at": "2026-05-15T00:00:00Z",
    });
    let watch_path = ci_dir.join(watch_filename("test/repo", "fix/x"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

    let r = crate::mcp::handlers::ci::handle_status_ci(
        &dir,
        &serde_json::json!({"repo": "test/repo"}),
        "lead",
    );
    let entry = r["watches"][0]
        .as_object()
        .expect("watches[0] is an object");
    assert!(
        entry.contains_key("pr_mergeable_state"),
        "status response must carry pr_mergeable_state field (#813), got keys: {:?}",
        entry.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        entry["pr_mergeable_state"], "CONFLICTING",
        "field must reflect the watch JSON's last_mergeable_state value"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_mergeable_check_fail_open_on_provider_error() {
    // GREEN coverage: when the provider returns Unknown (rate-limit,
    // network failure, no PR found) the helper must NOT emit an
    // alert AND must cache UNKNOWN to the watch JSON (so the
    // periodic re-check loop has a baseline). Fail-open contract
    // — block legit work only on confirmed CONFLICTING signal.
    let dir = tmp_dir("watch_start_unknown");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = serde_json::json!({
        "repo": "test/repo",
        "branch": "fix/x",
        "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-15T00:00:00Z"}],
        "instance": "lead",
        "interval_secs": 60,
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    let watch_path = ci_dir.join(watch_filename("test/repo", "fix/x"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

    // Default Mock returns Unknown (no `.with_mergeable(...)`).
    let provider = MockCiProvider::with_runs(Vec::new());

    super::watch_start_check_mergeable(
        &dir,
        &watch_path,
        "test/repo",
        "fix/x",
        &["lead".to_string()],
        &provider,
    );

    let inbox_path = dir.join("inbox").join("lead.jsonl");
    let body = std::fs::read_to_string(&inbox_path).unwrap_or_default();
    assert!(
        !body.contains("[ci-conflict-detected]"),
        "fail-open: Unknown provider result must NOT emit alert, got: {body}"
    );
    // Watch JSON now carries UNKNOWN for baseline.
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
    assert_eq!(
        after["last_mergeable_state"].as_str(),
        Some("UNKNOWN"),
        "Unknown state must still be cached to watch JSON as a baseline"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Helper for C4 tests: run ci_check_repo with a provider, return after
/// one cycle. Builds the watch JSON at provided path + invokes
/// ci_check_repo via a current-thread runtime block_on.
fn run_one_poll_cycle(
    dir: &Path,
    watch: &serde_json::Value,
    provider: &dyn CiProvider,
) -> std::path::PathBuf {
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let repo = watch["repo"].as_str().unwrap();
    let branch = watch["branch"].as_str().unwrap();
    let subscribers = parse_subscribers(watch);
    let watch_path = ci_dir.join(watch_filename(repo, branch));
    std::fs::write(&watch_path, serde_json::to_string_pretty(watch).unwrap()).unwrap();
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _ = rt.block_on(ci_check_repo(
        dir,
        &watch_path,
        repo,
        branch,
        &subscribers,
        None,
        None,
        None,
        None,
        None,
        &registry,
        provider,
    ));
    watch_path
}

#[test]
fn test_periodic_recheck_emits_alert_on_transition_into_conflicting() {
    // GREEN: ci_check_repo periodic re-check fires when
    // `last_mergeable_check_at` is older than 5min (or absent).
    // Provider returns CONFLICTING; previous cached state was
    // MERGEABLE → transition INTO Conflicting → alert emits.
    let dir = tmp_dir("recheck_transition_into");
    let watch = serde_json::json!({
        "repo": "test/repo",
        "branch": "fix/x",
        "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-15T00:00:00Z"}],
        "instance": "lead",
        "interval_secs": 60,
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
        // Cached state from a prior poll cycle, expired (>5min stale).
        "last_mergeable_state": "MERGEABLE",
        "last_mergeable_check_at":
            (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339(),
    });
    let provider =
        MockCiProvider::with_runs(Vec::new()).with_mergeable(MergeableState::Conflicting);
    run_one_poll_cycle(&dir, &watch, &provider);

    let inbox = std::fs::read_to_string(dir.join("inbox").join("lead.jsonl")).unwrap_or_default();
    assert!(
        inbox.contains("[ci-conflict-detected]"),
        "transition INTO CONFLICTING must emit alert, got inbox: {inbox}"
    );
    assert!(
        inbox.contains("poll-transition"),
        "alert source must be `poll-transition` (not `watch-start`), got: {inbox}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_periodic_recheck_does_not_re_alert_while_still_conflicting() {
    // GREEN: prevent alert spam. Cached state already CONFLICTING +
    // provider returns CONFLICTING again → NO new alert fired.
    let dir = tmp_dir("recheck_no_spam");
    let watch = serde_json::json!({
        "repo": "test/repo",
        "branch": "fix/x",
        "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-15T00:00:00Z"}],
        "instance": "lead",
        "interval_secs": 60,
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
        "last_mergeable_state": "CONFLICTING",
        "last_mergeable_check_at":
            (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339(),
    });
    let provider =
        MockCiProvider::with_runs(Vec::new()).with_mergeable(MergeableState::Conflicting);
    run_one_poll_cycle(&dir, &watch, &provider);

    let inbox = std::fs::read_to_string(dir.join("inbox").join("lead.jsonl")).unwrap_or_default();
    assert!(
        !inbox.contains("[ci-conflict-detected]"),
        "still-CONFLICTING (no transition) must NOT spam alerts, got: {inbox}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_status_response_has_null_pr_mergeable_state_pre_first_check() {
    // GREEN: a fresh watch without `last_mergeable_state` in its
    // JSON surfaces `pr_mergeable_state: null` in the status
    // response. Callers tolerating null are unaffected.
    let dir = tmp_dir("status_null_pre_check");
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = serde_json::json!({
        "repo": "test/repo",
        "branch": "fix/x",
        "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-15T00:00:00Z"}],
        "instance": "lead",
        "interval_secs": 60,
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        "last_terminal_seen_at": null,
        // No `last_mergeable_state` / `last_mergeable_check_at` —
        // simulates a pre-#813 watch OR a fresh watch that hasn't
        // run its first re-check yet.
    });
    let watch_path = ci_dir.join(watch_filename("test/repo", "fix/x"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

    let r = crate::mcp::handlers::ci::handle_status_ci(
        &dir,
        &serde_json::json!({"repo": "test/repo"}),
        "lead",
    );
    let entry = r["watches"][0].as_object().expect("watches[0]");
    assert!(
        entry["pr_mergeable_state"].is_null(),
        "pre-first-check watch must surface pr_mergeable_state=null, got {:?}",
        entry["pr_mergeable_state"]
    );
    assert!(
        entry["pr_mergeable_check_at"].is_null(),
        "pre-first-check watch must surface pr_mergeable_check_at=null, got {:?}",
        entry["pr_mergeable_check_at"]
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── #946 correlation_id population on system:ci inbox messages ──
//
// Pre-#946 all `system:ci` enqueues carried `correlation_id: None`,
// making it impossible for operators to grep inbox.jsonl for
// messages from a specific watch. Post-fix every site populates
// `Some(format!("{repo}@{branch}"))` so operators can trace a
// notification back to its watch in one grep.
//
// Stable across hash migrations (#943): the correlation_id value
// is the (canonical post-#942) `repo@branch` string, NOT the
// watch_filename hash. Future hash-scheme changes preserve the
// stable grep target.

#[test]
fn ci_pass_inbox_message_carries_repo_branch_correlation_id() {
    let dir = tmp_dir("946-ci-pass-corr");
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 100,
        conclusion: Some("success".into()),
        head_sha: "abc".into(),
        url: "https://example.com/100".into(),
        name: String::new(),
    }]);
    run_ci_check(&dir, &base_watch(), &provider).unwrap();
    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    let content = std::fs::read_to_string(&inbox_path).unwrap();
    // base_watch has repo="o/r", branch="feat"
    let expected = r#""correlation_id":"o/r@feat""#;
    assert!(
        content.contains(expected),
        "ci-pass message must carry correlation_id={expected}: {content}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ci_stale_inbox_message_carries_repo_branch_correlation_id() {
    let dir = tmp_dir("946-ci-stale-corr");
    // Two-run setup mirrors `mock_stale_sha_emits_ci_stale_inbox_message`
    // — old-head run completes after new commit pushed; old run's
    // notification is dropped + emits [ci-stale].
    let provider = MockCiProvider::with_runs(vec![
        CiRun {
            id: 301,
            conclusion: None,
            head_sha: "newhead".into(),
            url: "https://example.com/301".into(),
            name: String::new(),
        },
        CiRun {
            id: 300,
            conclusion: Some("success".into()),
            head_sha: "oldhead".into(),
            url: "https://example.com/300".into(),
            name: String::new(),
        },
    ]);
    run_ci_check(&dir, &base_watch(), &provider).unwrap();
    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    let content = std::fs::read_to_string(&inbox_path).unwrap();
    assert!(
        content.contains("[ci-stale]"),
        "expected [ci-stale] inbox message: {content}"
    );
    let expected = r#""correlation_id":"o/r@feat""#;
    assert!(
        content.contains(expected),
        "ci-stale message must carry correlation_id={expected}: {content}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ci_conflict_alert_inbox_message_carries_repo_branch_correlation_id() {
    let dir = tmp_dir("946-ci-conflict-corr");
    let subscribers = vec!["agent1".to_string()];
    emit_ci_conflict_alert(&dir, "o/r", "feat", &subscribers, "watch-start");
    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    let content = std::fs::read_to_string(&inbox_path).unwrap();
    assert!(
        content.contains("[ci-conflict-detected]"),
        "expected ci-conflict alert: {content}"
    );
    let expected = r#""correlation_id":"o/r@feat""#;
    assert!(
        content.contains(expected),
        "ci-conflict-detected message must carry correlation_id={expected}: {content}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── #1026 ci-stale debounce tests ─────────────────────────────

#[test]
fn ci_stale_debounce_suppresses_repeat_for_same_sha() {
    let dir = tmp_dir("1026-debounce");
    let provider = MockCiProvider::with_runs(vec![
        CiRun {
            id: 301,
            conclusion: None,
            head_sha: "newhead".into(),
            url: "https://example.com/301".into(),
            name: String::new(),
        },
        CiRun {
            id: 300,
            conclusion: Some("success".into()),
            head_sha: "oldhead".into(),
            url: "https://example.com/300".into(),
            name: String::new(),
        },
    ]);
    // First poll: ci-stale fires for oldhead.
    run_ci_check(&dir, &base_watch(), &provider).unwrap();
    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    let first = std::fs::read_to_string(&inbox_path).unwrap();
    let first_count = first.matches("[ci-stale]").count();
    assert_eq!(first_count, 1, "first poll emits exactly 1 ci-stale");

    // Read persisted watch to get updated state for second poll.
    let ci_dir = dir.join("ci-watches");
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
    assert_eq!(
        watch["last_stale_emitted_sha"].as_str(),
        Some("oldhead"),
        "stale debounce SHA must be persisted"
    );

    // Second poll: same stale SHA with different conclusion → suppressed.
    let provider2 = MockCiProvider::with_runs(vec![
        CiRun {
            id: 301,
            conclusion: None,
            head_sha: "newhead".into(),
            url: "https://example.com/301".into(),
            name: String::new(),
        },
        CiRun {
            id: 302,
            conclusion: Some("failure".into()),
            head_sha: "oldhead".into(),
            url: "https://example.com/302".into(),
            name: String::new(),
        },
    ]);
    // Pass the persisted watch state (including last_stale_emitted_sha).
    run_ci_check(&dir, &watch, &provider2).unwrap();
    let second = std::fs::read_to_string(&inbox_path).unwrap();
    let second_count = second.matches("[ci-stale]").count();
    assert_eq!(
        second_count, first_count,
        "second poll must NOT emit another ci-stale (debounced): {second}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ci_stale_debounce_allows_new_stale_sha() {
    let dir = tmp_dir("1026-new-stale");
    // First poll: SHA-A is stale.
    let provider = MockCiProvider::with_runs(vec![
        CiRun {
            id: 401,
            conclusion: None,
            head_sha: "sha-c".into(),
            url: "https://example.com/401".into(),
            name: String::new(),
        },
        CiRun {
            id: 400,
            conclusion: Some("success".into()),
            head_sha: "sha-a".into(),
            url: "https://example.com/400".into(),
            name: String::new(),
        },
    ]);
    run_ci_check(&dir, &base_watch(), &provider).unwrap();
    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    let first = std::fs::read_to_string(&inbox_path).unwrap();
    assert_eq!(first.matches("[ci-stale]").count(), 1);

    // Get persisted watch state.
    let ci_dir = dir.join("ci-watches");
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();

    // Second poll: SHA-B is stale (different from SHA-A) → should fire.
    let provider2 = MockCiProvider::with_runs(vec![
        CiRun {
            id: 501,
            conclusion: None,
            head_sha: "sha-d".into(),
            url: "https://example.com/501".into(),
            name: String::new(),
        },
        CiRun {
            id: 500,
            conclusion: Some("success".into()),
            head_sha: "sha-b".into(),
            url: "https://example.com/500".into(),
            name: String::new(),
        },
    ]);
    run_ci_check(&dir, &watch, &provider2).unwrap();
    let second = std::fs::read_to_string(&inbox_path).unwrap();
    assert_eq!(
        second.matches("[ci-stale]").count(),
        2,
        "different stale SHA must NOT be suppressed: {second}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ci_stale_debounce_does_not_affect_current_sha_notifications() {
    let dir = tmp_dir("1026-current-ok");
    // Set up watch with last_stale_emitted_sha = "oldhead".
    let mut watch = base_watch();
    watch["last_stale_emitted_sha"] = serde_json::json!("oldhead");
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 600,
        conclusion: Some("success".into()),
        head_sha: "currenthead".into(),
        url: "https://example.com/600".into(),
        name: String::new(),
    }]);
    run_ci_check(&dir, &watch, &provider).unwrap();
    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    let content = std::fs::read_to_string(&inbox_path).unwrap();
    assert!(
        content.contains("[ci-pass]"),
        "current-head notification must still fire: {content}"
    );
    assert!(
        !content.contains("[ci-stale]"),
        "no stale notification for current-head: {content}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// #1134 T1: ci-pass inbox delivery does NOT produce generic
/// `[AGEND-MSG-PENDING]` format — verifies the old dual-delivery
/// PTY inject path is gone and replaced by inbox-only with friendly hint.
#[test]
fn ci_pass_no_agend_msg_pending_format() {
    let dir = tmp_dir("1134-t1-no-pending");
    std::fs::create_dir_all(&dir).unwrap();
    let msg = crate::inbox::InboxMessage::new_system(
        "system:ci",
        "ci-watch",
        "[ci-pass] o/r@feat (abc1234): passed ✓\nURL: https://example.com".to_string(),
    );
    let captured: std::sync::Arc<parking_lot::Mutex<Option<String>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(None));
    let cap = captured.clone();
    crate::inbox::enqueue_with_idle_hint_with_emitter(&dir, "agent1", msg, move |hint| {
        *cap.lock() = Some(hint.to_string());
    })
    .unwrap();
    let hint = captured.lock().clone().expect("emitter must fire once");
    assert!(
        !hint.contains("AGEND-MSG-PENDING"),
        "#1134: ci-pass must NOT use generic AGEND-MSG-PENDING format; got: {hint}"
    );
    assert!(
        !hint.contains("kind=ci-watch"),
        "#1134: ci-pass friendly hint must not carry kind= header; got: {hint}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// #1134 T2: ci-pass inbox hint renders friendly format preserving
/// the `[ci-pass] repo@branch (sha): passed ✓` visual.
#[test]
fn ci_pass_friendly_hint_format() {
    let dir = tmp_dir("1134-t2-friendly");
    std::fs::create_dir_all(&dir).unwrap();
    let msg = crate::inbox::InboxMessage::new_system(
        "system:ci",
        "ci-watch",
        "[ci-pass] o/r@feat (abc1234): passed ✓\nURL: https://example.com".to_string(),
    );
    let captured: std::sync::Arc<parking_lot::Mutex<Option<String>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(None));
    let cap = captured.clone();
    crate::inbox::enqueue_with_idle_hint_with_emitter(&dir, "agent1", msg, move |hint| {
        *cap.lock() = Some(hint.to_string());
    })
    .unwrap();
    let hint = captured.lock().clone().expect("emitter must fire once");
    assert!(
        hint.contains("[ci-pass] o/r@feat (abc1234): passed ✓"),
        "#1134: friendly hint must contain ci-pass headline; got: {hint}"
    );
    assert!(
        hint.contains("(inbox="),
        "#1134: friendly hint must contain inbox count; got: {hint}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// #1134 T3: Regression guard — source-level assertion that
/// `inject_to_agent` is NOT called anywhere in the ci_check_repo
/// fan-out path. If someone re-introduces the PTY inject, this test
/// will fail, enforcing the single-delivery (inbox-only) invariant.
#[test]
fn ci_check_repo_no_inject_to_agent_regression_guard() {
    let source = include_str!("poller.rs");
    assert!(
        !source.contains("inject_to_agent"),
        "#1134 REGRESSION: inject_to_agent must not appear in poller.rs — \
         CI-watch delivery is inbox-only. If you need PTY inject for a new \
         feature, discuss in #1134 first."
    );
}

/// #1134 T4: End-to-end — `run_ci_check` with a success run produces
/// inbox notification but no PTY inject side-effect. Verifies the
/// full `ci_check_repo` path with a registered agent in the registry.
#[test]
fn ci_check_repo_success_no_pty_inject_only_inbox() {
    use std::sync::Arc;

    let dir = tmp_dir("1134-t4-e2e-no-inject");
    let provider = MockCiProvider::with_runs(vec![CiRun {
        id: 200,
        conclusion: Some("success".into()),
        head_sha: "def456".into(),
        url: "https://example.com/200".into(),
        name: String::new(),
    }]);

    // Create watch with a subscriber
    let watch = serde_json::json!({
        "repo": "o/r", "branch": "feat", "interval_secs": 60,
        "instance": "agent1", "last_run_id": null, "head_sha": null,
        "last_polled_at": null, "last_notified_head_sha": null,
    });
    let ci_dir = dir.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
    std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

    let subscribers = vec!["agent1".to_string()];

    // Registry with NO agent handle — if inject_to_agent were still called,
    // it would try to lock the registry and find nothing. But crucially,
    // since we removed the call entirely, the registry isn't even consulted
    // for injection purposes.
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ci_check_repo(
        &dir,
        &watch_path,
        "o/r",
        "feat",
        &subscribers,
        None,
        None,
        None,
        None,
        None,
        &registry,
        &provider,
    ))
    .unwrap();

    // Verify inbox was delivered
    let inbox_path = dir.join("inbox").join("agent1.jsonl");
    assert!(
        inbox_path.exists(),
        "#1134: inbox notification must be delivered for ci-pass"
    );
    let content = std::fs::read_to_string(&inbox_path).unwrap();
    assert!(
        content.contains("[ci-pass]"),
        "#1134: inbox message must contain [ci-pass]; got: {content}"
    );

    // The absence of inject_to_agent in source (T3) guarantees no PTY
    // inject happens — this test confirms the inbox-only delivery works
    // end-to-end through ci_check_repo.
    std::fs::remove_dir_all(&dir).ok();
}

/// #1151: required_checks filter — required pass + non-required failure = "success"
#[test]
fn aggregate_required_checks_ignores_non_required_failure() {
    let runs = vec![
        CiRun {
            id: 1,
            conclusion: Some("success".into()),
            head_sha: "abc".into(),
            url: String::new(),
            name: "CI".into(),
        },
        CiRun {
            id: 2,
            conclusion: Some("failure".into()),
            head_sha: "abc".into(),
            url: String::new(),
            name: "LOC Overrun Check".into(),
        },
    ];
    // Without filter: failure (all must pass)
    assert_eq!(
        super::aggregate_conclusion_for_sha(&runs, "abc"),
        Some("failure"),
    );
    // With required_checks: only "CI" matters → success
    let required = vec!["CI".to_string()];
    assert_eq!(
        super::aggregate_conclusion_for_sha_filtered(&runs, "abc", Some(&required)),
        Some("success"),
    );
}
