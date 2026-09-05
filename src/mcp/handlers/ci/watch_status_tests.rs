use serde_json::{json, Value};
use std::path::Path;

fn write_sidecar(home: &Path, filename: &str, value: &Value) {
    let ci_dir = home.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).unwrap();
    crate::store::atomic_write(
        &ci_dir.join(filename),
        serde_json::to_string_pretty(value).unwrap().as_bytes(),
    )
    .unwrap();
}

#[test]
fn exact_head_status_exposes_target_head_sha() {
    let home = std::env::temp_dir().join(format!(
        "agend-target-head-status-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let pinned_sha = "a".repeat(40);
    let filename = crate::daemon::ci_watch::watch_filename_exact_head("o/r", "main", &pinned_sha);
    write_sidecar(
        &home,
        &filename,
        &json!({
            "repo": "o/r",
            "branch": "main",
            "interval_secs": 60,
            "head_sha": null,
            "target_head_sha": pinned_sha,
            "subscribers": [{"instance": "agent-a"}],
            "expires_at": "2099-01-01T00:00:00Z",
        }),
    );

    let resp = super::watch::handle_status_ci(
        &home,
        &json!({"repository": "o/r", "branch": "main"}),
        "agent-a",
    );
    let watches = resp["watches"].as_array().unwrap();
    assert_eq!(watches.len(), 1);
    assert_eq!(
        watches[0]["target_head_sha"].as_str(),
        Some(pinned_sha.as_str()),
        "exact-head status must expose target_head_sha: {watches:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn generic_watch_status_has_no_target_head_sha() {
    let home = std::env::temp_dir().join(format!(
        "agend-generic-no-target-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let filename = crate::daemon::ci_watch::watch_filename("o/r", "feat/x");
    write_sidecar(
        &home,
        &filename,
        &json!({
            "repo": "o/r",
            "branch": "feat/x",
            "interval_secs": 60,
            "head_sha": null,
            "subscribers": [{"instance": "agent-b"}],
            "expires_at": "2099-01-01T00:00:00Z",
        }),
    );

    let resp = super::watch::handle_status_ci(
        &home,
        &json!({"repository": "o/r", "branch": "feat/x"}),
        "agent-b",
    );
    let watches = resp["watches"].as_array().unwrap();
    assert_eq!(watches.len(), 1);
    assert!(
        watches[0]["target_head_sha"].is_null(),
        "generic watch must not have target_head_sha: {watches:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn exact_head_status_head_sha_remains_null_before_poll() {
    let home = std::env::temp_dir().join(format!(
        "agend-head-sha-null-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let pinned_sha = "b".repeat(40);
    let filename = crate::daemon::ci_watch::watch_filename_exact_head("o/r", "main", &pinned_sha);
    write_sidecar(
        &home,
        &filename,
        &json!({
            "repo": "o/r",
            "branch": "main",
            "interval_secs": 60,
            "head_sha": null,
            "target_head_sha": pinned_sha,
            "subscribers": [{"instance": "agent-c"}],
            "expires_at": "2099-01-01T00:00:00Z",
        }),
    );

    let resp = super::watch::handle_status_ci(
        &home,
        &json!({"repository": "o/r", "branch": "main"}),
        "agent-c",
    );
    let watches = resp["watches"].as_array().unwrap();
    assert_eq!(watches.len(), 1);
    assert!(
        watches[0]["head_sha"].is_null(),
        "poll-observed head_sha must remain null before first poll: {watches:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// t-…-67: `ci status` is SUBSCRIBER-scoped (a named caller sees only watches
/// it subscribes to), and that scoping was invisible — four "watches: []"
/// readings in two days were made by an orchestrator who was simply not a
/// subscriber, each followed by a needless manual re-arm. The response must
/// SAY its scope and how many watches on the requested filter it hid, and tell
/// the reader what actually changes state.
#[test]
fn status_reports_scope_and_hidden_watch_count_t67() {
    let home = std::env::temp_dir().join(format!(
        "agend-status-scope-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    for (branch, subscriber) in [("feat/a", "agent-a"), ("feat/b", "agent-b")] {
        write_sidecar(
            &home,
            &crate::daemon::ci_watch::watch_filename("o/r", branch),
            &json!({
                "repo": "o/r",
                "branch": branch,
                "interval_secs": 60,
                "head_sha": null,
                "subscribers": [{"instance": subscriber}],
                "expires_at": "2099-01-01T00:00:00Z",
            }),
        );
    }

    // A named caller subscribed to ONE of the two watches.
    let resp = super::watch::handle_status_ci(&home, &json!({}), "agent-a");
    assert_eq!(
        resp["watches"].as_array().map(|w| w.len()),
        Some(1),
        "caller-scoping itself is unchanged: {resp}"
    );
    assert_eq!(
        resp["scope"].as_str(),
        Some("subscriber:agent-a"),
        "the response must state its scope: {resp}"
    );
    assert_eq!(
        resp["hidden_watches"].as_u64(),
        Some(1),
        "the other subscriber's watch on the same filter is hidden, and counted: {resp}"
    );
    let hint = resp["hint"]
        .as_str()
        .unwrap_or_else(|| panic!("hidden > 0 must carry a hint: {resp}"));
    assert!(
        hint.starts_with("1 watch(es)")
            && hint.contains("not subscribed")
            && hint.contains("review_class=")
            && hint.contains("readiness recompute"),
        "the hint must say what is hidden and what actually changes state: {hint}"
    );

    // The anonymous CLI sees everything and hides nothing.
    let all = super::watch::handle_status_ci(&home, &json!({}), "");
    assert_eq!(all["watches"].as_array().map(|w| w.len()), Some(2), "{all}");
    assert_eq!(all["scope"].as_str(), Some("all"), "{all}");
    assert_eq!(all["hidden_watches"].as_u64(), Some(0), "{all}");
    assert!(all["hint"].is_null(), "nothing hidden ⟹ no hint: {all}");
    std::fs::remove_dir_all(&home).ok();
}

/// t-…-67: `hidden_watches` counts only watches on the REQUESTED repo/branch
/// filter — a watch the filter already excludes is not "hidden", it is out of
/// scope — so the count answers exactly "is there a watch here I cannot see?".
#[test]
fn status_hidden_count_respects_repo_branch_filter_t67() {
    let home = std::env::temp_dir().join(format!(
        "agend-status-hidden-filter-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    for (repo, branch, subscriber) in [
        ("o/r", "feat/a", "agent-a"),
        ("o/r", "feat/b", "agent-b"),
        ("x/y", "feat/a", "agent-b"),
    ] {
        write_sidecar(
            &home,
            &crate::daemon::ci_watch::watch_filename(repo, branch),
            &json!({
                "repo": repo,
                "branch": branch,
                "interval_secs": 60,
                "head_sha": null,
                "subscribers": [{"instance": subscriber}],
                "expires_at": "2099-01-01T00:00:00Z",
            }),
        );
    }
    let status = |args: serde_json::Value| super::watch::handle_status_ci(&home, &args, "agent-a");

    // Filter on a branch the caller does NOT subscribe to: nothing visible, one hidden.
    let r = status(json!({"repository": "o/r", "branch": "feat/b"}));
    assert_eq!(r["watches"].as_array().map(|w| w.len()), Some(0), "{r}");
    assert_eq!(r["hidden_watches"].as_u64(), Some(1), "{r}");
    assert!(r["hint"].is_string(), "{r}");

    // Filter on the caller's own branch: visible, nothing hidden, no hint.
    let r = status(json!({"repository": "o/r", "branch": "feat/a"}));
    assert_eq!(r["watches"].as_array().map(|w| w.len()), Some(1), "{r}");
    assert_eq!(r["hidden_watches"].as_u64(), Some(0), "{r}");
    assert!(r["hint"].is_null(), "{r}");

    // Repo filter: only that repo's watches can be hidden.
    let r = status(json!({"repository": "x/y"}));
    assert_eq!(r["watches"].as_array().map(|w| w.len()), Some(0), "{r}");
    assert_eq!(r["hidden_watches"].as_u64(), Some(1), "{r}");

    // A filter matching no watch at all hides nothing.
    let r = status(json!({"repository": "nope/none"}));
    assert_eq!(r["watches"].as_array().map(|w| w.len()), Some(0), "{r}");
    assert_eq!(r["hidden_watches"].as_u64(), Some(0), "{r}");
    assert!(r["hint"].is_null(), "{r}");
    std::fs::remove_dir_all(&home).ok();
}
