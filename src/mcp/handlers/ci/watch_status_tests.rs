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
    let filename =
        crate::daemon::ci_watch::watch_filename_exact_head("o/r", "main", &pinned_sha);
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
    let filename =
        crate::daemon::ci_watch::watch_filename_exact_head("o/r", "main", &pinned_sha);
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
