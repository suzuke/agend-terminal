//! #2812 notification-only watch — guard and contract tests.
//!
//! Contract (decision d-20260718081633645174-4): a `notification_only=true`
//! watch on a protected ref is accepted only with a full immutable
//! `head_sha` + `task_id` + matching merge receipt + caller is the task
//! assignee. `next_after_ci` must be absent/empty. Guards: no receipt →
//! reject; mismatched SHA → reject; non-assignee → reject; next_after_ci
//! present → reject. Idempotent replay succeeds.

use super::watch::handle_watch_ci;
use serde_json::json;

fn tmp_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let h = std::env::temp_dir().join(format!(
        "agend-notif-only-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    let _ = std::fs::remove_dir_all(&h);
    std::fs::create_dir_all(&h).unwrap();
    h
}

fn seed_fleet(home: &std::path::Path, instances: &[&str]) {
    let yaml = format!(
        "instances:\n{}\n",
        instances
            .iter()
            .map(|i| format!("  {i}:\n    backend: claude\n"))
            .collect::<String>()
    );
    std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
}

fn make_source_repo(home: &std::path::Path) -> std::path::PathBuf {
    let source_repo = home.join("source-repo");
    std::fs::create_dir_all(&source_repo).unwrap();
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&source_repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{REPO}.git"),
        ])
        .current_dir(&source_repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    source_repo
}

fn seed_binding(home: &std::path::Path, agent: &str, task_id: &str) {
    let source_repo = make_source_repo(home);
    seed_binding_with_source(home, agent, task_id, "fix/test", &source_repo);
}

fn seed_binding_with_source(
    home: &std::path::Path,
    agent: &str,
    task_id: &str,
    branch: &str,
    source_repo: &std::path::Path,
) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    let binding = json!({
        "task_id": task_id,
        "branch": branch,
        "issued_at": "2026-07-18T00:00:00Z",
        "worktree": "/tmp/fake-wt",
        "source_repo": source_repo.display().to_string(),
    });
    std::fs::write(
        dir.join("binding.json"),
        serde_json::to_vec_pretty(&binding).unwrap(),
    )
    .unwrap();
}

fn seed_receipt(home: &std::path::Path, repo: &str, sha: &str, task_id: &str, agent: &str) {
    let expiry = chrono::Utc::now() + chrono::TimeDelta::try_hours(1).unwrap();
    crate::merge_receipt::persist(
        home,
        &crate::merge_receipt::MergeReceipt {
            repo: repo.into(),
            merge_sha: sha.into(),
            task_id: task_id.into(),
            task_assignee: agent.into(),
            merge_authority: "lead".into(),
            pr_number: 42,
            created_at: chrono::Utc::now().to_rfc3339(),
            expires_at: expiry.to_rfc3339(),
        },
    )
    .unwrap();
}

fn seed_expired_receipt(home: &std::path::Path, repo: &str, sha: &str, task_id: &str, agent: &str) {
    let past = chrono::Utc::now() - chrono::TimeDelta::try_hours(2).unwrap();
    crate::merge_receipt::persist(
        home,
        &crate::merge_receipt::MergeReceipt {
            repo: repo.into(),
            merge_sha: sha.into(),
            task_id: task_id.into(),
            task_assignee: agent.into(),
            merge_authority: "lead".into(),
            pr_number: 42,
            created_at: past.to_rfc3339(),
            expires_at: (past + chrono::TimeDelta::try_hours(1).unwrap()).to_rfc3339(),
        },
    )
    .unwrap();
}

const REPO: &str = "suzuke/agend-terminal";

/// RED: notification_only watch without merge receipt → rejected.
#[test]
fn notification_only_without_receipt_rejected() {
    let home = tmp_home("no-receipt");
    let sha = "a".repeat(40);
    seed_fleet(&home, &["dev"]);
    seed_binding(&home, "dev", "t-1");

    let r = handle_watch_ci(
        &home,
        &json!({
            "repository": REPO, "branch": "main",
            "head_sha": sha, "task_id": "t-1",
            "notification_only": true,
        }),
        "dev",
    );
    assert!(r.get("error").is_some(), "no receipt → reject: {r}");
    std::fs::remove_dir_all(&home).ok();
}

/// RED: notification_only with next_after_ci → rejected (no privileged continuation).
#[test]
fn notification_only_with_next_after_ci_rejected() {
    let home = tmp_home("next-after");
    let sha = "b".repeat(40);
    seed_fleet(&home, &["dev", "reviewer"]);
    seed_binding(&home, "dev", "t-2");
    seed_receipt(&home, REPO, &sha, "t-2", "dev");

    let r = handle_watch_ci(
        &home,
        &json!({
            "repository": REPO, "branch": "main",
            "head_sha": sha, "task_id": "t-2",
            "notification_only": true,
            "next_after_ci": "reviewer",
        }),
        "dev",
    );
    assert!(r.get("error").is_some(), "next_after_ci → reject: {r}");
    std::fs::remove_dir_all(&home).ok();
}

/// RED: notification_only from non-assignee → rejected.
#[test]
fn notification_only_non_assignee_rejected() {
    let home = tmp_home("non-assign");
    let sha = "c".repeat(40);
    seed_fleet(&home, &["dev", "other"]);
    seed_binding(&home, "other", "t-3");
    seed_receipt(&home, REPO, &sha, "t-3", "dev");

    let r = handle_watch_ci(
        &home,
        &json!({
            "repository": REPO, "branch": "main",
            "head_sha": sha, "task_id": "t-3",
            "notification_only": true,
        }),
        "other",
    );
    assert!(r.get("error").is_some(), "non-assignee → reject: {r}");
    std::fs::remove_dir_all(&home).ok();
}

/// RED: expired receipt → rejected (no indefinite replay).
#[test]
fn notification_only_expired_receipt_rejected() {
    let home = tmp_home("expired");
    let sha = "e1".repeat(20);
    seed_fleet(&home, &["dev"]);
    seed_binding(&home, "dev", "t-exp");
    seed_expired_receipt(&home, REPO, &sha, "t-exp", "dev");

    let r = handle_watch_ci(
        &home,
        &json!({
            "repository": REPO, "branch": "main",
            "head_sha": sha, "task_id": "t-exp",
            "notification_only": true,
        }),
        "dev",
    );
    assert!(r.get("error").is_some(), "expired receipt → reject: {r}");
    std::fs::remove_dir_all(&home).ok();
}

/// RED: notification_only with SHA mismatch vs receipt → rejected.
#[test]
fn notification_only_sha_mismatch_rejected() {
    let home = tmp_home("sha-mm");
    let receipt_sha = "d".repeat(40);
    let watch_sha = "e".repeat(40);
    seed_fleet(&home, &["dev"]);
    seed_binding(&home, "dev", "t-4");
    seed_receipt(&home, REPO, &receipt_sha, "t-4", "dev");

    let r = handle_watch_ci(
        &home,
        &json!({
            "repository": REPO, "branch": "main",
            "head_sha": watch_sha, "task_id": "t-4",
            "notification_only": true,
        }),
        "dev",
    );
    assert!(r.get("error").is_some(), "SHA mismatch → reject: {r}");
    std::fs::remove_dir_all(&home).ok();
}

/// RED: notification_only valid → succeeds + idempotent replay.
#[test]
fn notification_only_valid_succeeds_and_idempotent() {
    let home = tmp_home("valid");
    let sha = "f".repeat(40);
    seed_fleet(&home, &["dev"]);
    seed_binding(&home, "dev", "t-5");
    seed_receipt(&home, REPO, &sha, "t-5", "dev");

    let args = json!({
        "repository": REPO, "branch": "main",
        "head_sha": sha, "task_id": "t-5",
        "notification_only": true,
    });
    let r1 = handle_watch_ci(&home, &args, "dev");
    assert!(r1.get("error").is_none(), "valid → succeed: {r1}");

    let r2 = handle_watch_ci(&home, &args, "dev");
    assert!(r2.get("error").is_none(), "replay → succeed: {r2}");

    std::fs::remove_dir_all(&home).ok();
}

// ── Production-seam tests for post_merge_receipt_and_watch ──

/// Production seam: REAL TOPOLOGY — unbound orchestrator merges, developer
/// is separately bound to the PR branch → receipt + watch armed for developer.
#[test]
fn post_merge_orchestrator_merge_developer_bound_arms_watch() {
    let home = tmp_home("post-merge-topology");
    let sha = "1".repeat(40);
    seed_fleet(&home, &["lead", "dev"]);
    let source_repo = make_source_repo(&home);
    seed_binding_with_source(&home, "dev", "t-merge", "fix/feature-x", &source_repo);
    // Orchestrator (lead) has NO binding.

    // Orchestrator calls merge → post_merge resolves developer from PR branch.
    let diag =
        super::merge::post_merge_receipt_and_watch(&home, REPO, &sha, 99, "fix/feature-x", "lead");

    assert_eq!(
        diag["receipt"], "persisted",
        "receipt must be persisted: {diag}"
    );
    assert_eq!(
        diag["assignee"], "dev",
        "assignee must be the developer: {diag}"
    );
    assert_eq!(
        diag["watch"], "armed",
        "watch must be armed for developer: {diag}"
    );
    let receipt = crate::merge_receipt::find(&home, REPO, &sha, "t-merge");
    assert!(receipt.is_some(), "receipt must be findable on disk");
    let r = receipt.unwrap();
    assert_eq!(r.task_assignee, "dev");
    assert_eq!(r.merge_authority, "lead");
    assert_eq!(r.pr_number, 99);
    std::fs::remove_dir_all(&home).ok();
}

/// Production seam: no binding matches the PR branch → gracefully skipped.
#[test]
fn post_merge_no_branch_binding_skips_gracefully() {
    let home = tmp_home("post-merge-nobound");
    let sha = "2".repeat(40);
    seed_fleet(&home, &["lead"]);
    // No one is bound to any branch.

    let diag = super::merge::post_merge_receipt_and_watch(
        &home,
        REPO,
        &sha,
        100,
        "fix/nobody-bound",
        "lead",
    );

    assert!(
        diag["skipped"].as_str().is_some(),
        "no binding → must skip: {diag}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Production seam: ambiguous (multiple agents bound to same PR branch) → skipped.
#[test]
fn post_merge_ambiguous_binding_skips() {
    let home = tmp_home("post-merge-ambig");
    let sha = "a1".repeat(20);
    seed_fleet(&home, &["lead", "dev1", "dev2"]);
    // Two developers bound to the same branch.
    let source_repo = make_source_repo(&home);
    for agent in ["dev1", "dev2"] {
        seed_binding_with_source(
            &home,
            agent,
            &format!("t-{agent}"),
            "fix/shared",
            &source_repo,
        );
    }

    let diag =
        super::merge::post_merge_receipt_and_watch(&home, REPO, &sha, 101, "fix/shared", "lead");

    assert!(
        diag["skipped"].as_str().is_some(),
        "ambiguity → must skip: {diag}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Production seam: merge success remains truthful even if watch arm fails.
#[test]
fn post_merge_watch_failure_still_persists_receipt() {
    let home = tmp_home("post-merge-watchfail");
    let sha = "3".repeat(40);
    seed_fleet(&home, &["lead", "dev"]);
    seed_binding(&home, "dev", "t-watchfail");

    // Make the watch arm fail by blocking the ci-watches dir as a file.
    let watches_dir = home.join("ci-watches");
    std::fs::write(&watches_dir, b"blocker").unwrap();

    let diag =
        super::merge::post_merge_receipt_and_watch(&home, REPO, &sha, 102, "fix/test", "lead");

    assert_eq!(
        diag["receipt"], "persisted",
        "receipt persisted despite watch fail: {diag}"
    );
    assert!(
        diag["watch_error"].is_string(),
        "watch error must be surfaced: {diag}"
    );
    let receipt = crate::merge_receipt::find(&home, REPO, &sha, "t-watchfail");
    assert!(receipt.is_some(), "receipt survives watch failure");
    std::fs::remove_dir_all(&home).ok();
}

/// Binding mismatch on manual notification_only → rejected.
#[test]
fn notification_only_binding_task_mismatch_rejected() {
    let home = tmp_home("bind-mm");
    let sha = "4".repeat(40);
    seed_fleet(&home, &["dev"]);
    seed_binding(&home, "dev", "t-different");
    seed_receipt(&home, REPO, &sha, "t-bind-mm", "dev");

    let r = handle_watch_ci(
        &home,
        &json!({
            "repository": REPO, "branch": "main",
            "head_sha": sha, "task_id": "t-bind-mm",
            "notification_only": true,
        }),
        "dev",
    );
    assert!(
        r.get("error").is_some(),
        "binding task mismatch → reject: {r}"
    );
    assert_eq!(r["code"], "notification_only_binding_mismatch");
    std::fs::remove_dir_all(&home).ok();
}
