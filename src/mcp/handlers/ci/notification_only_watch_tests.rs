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

/// Terminal removal of a notification-only watch must consume the receipt.
/// After terminal: both watch + receipt absent. Stale replay after terminal
/// must be rejected with notification_only_no_receipt.
#[test]
fn notification_only_terminal_removal_consumes_receipt() {
    let home = tmp_home("terminal-consume");
    let sha = "d".repeat(40);
    seed_fleet(&home, &["dev"]);
    seed_binding(&home, "dev", "t-terminal");
    seed_receipt(&home, REPO, &sha, "t-terminal", "dev");

    // Arm the notification-only watch.
    let args = json!({
        "repository": REPO, "branch": "main",
        "head_sha": sha, "task_id": "t-terminal",
        "notification_only": true,
    });
    let r = handle_watch_ci(&home, &args, "dev");
    assert!(r.get("error").is_none(), "arm must succeed: {r}");

    // Verify both watch + receipt exist before terminal.
    assert!(
        crate::merge_receipt::find(&home, REPO, &sha, "t-terminal").is_some(),
        "receipt must exist before terminal"
    );
    let watch_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
    let watch_count = std::fs::read_dir(&watch_dir)
        .map(|e| {
            e.flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                .count()
        })
        .unwrap_or(0);
    assert!(watch_count > 0, "watch must exist before terminal");

    // Simulate terminal removal via shared remove_watch.
    for entry in std::fs::read_dir(&watch_dir).unwrap().flatten() {
        let p = entry.path();
        if p.extension().and_then(|x| x.to_str()) == Some("json") {
            crate::daemon::ci_watch::remove_watch(
                &home,
                &p,
                "dev",
                REPO,
                "main",
                "exact_head_terminal",
            );
        }
    }

    // After terminal: both watch + receipt must be absent.
    assert!(
        crate::merge_receipt::find(&home, REPO, &sha, "t-terminal").is_none(),
        "receipt must be consumed by terminal removal"
    );
    let post_count = std::fs::read_dir(&watch_dir)
        .map(|e| {
            e.flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(post_count, 0, "watch must be removed by terminal");

    // Stale replay after terminal must fail — receipt is gone.
    let stale = handle_watch_ci(&home, &args, "dev");
    assert!(
        stale.get("error").is_some(),
        "stale replay after terminal must be rejected: {stale}"
    );
    assert_eq!(
        stale["code"], "notification_only_no_receipt",
        "rejection reason must be no_receipt"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Pre-terminal repeat remains idempotent (receipt still live).
#[test]
fn notification_only_pre_terminal_repeat_idempotent() {
    let home = tmp_home("pre-terminal-idem");
    let sha = "e".repeat(40);
    seed_fleet(&home, &["dev"]);
    seed_binding(&home, "dev", "t-idem");
    seed_receipt(&home, REPO, &sha, "t-idem", "dev");

    let args = json!({
        "repository": REPO, "branch": "main",
        "head_sha": sha, "task_id": "t-idem",
        "notification_only": true,
    });
    let r1 = handle_watch_ci(&home, &args, "dev");
    assert!(r1.get("error").is_none(), "first arm: {r1}");

    let r2 = handle_watch_ci(&home, &args, "dev");
    assert!(r2.get("error").is_none(), "pre-terminal repeat: {r2}");

    // Receipt still exists (not consumed yet — no terminal removal).
    assert!(
        crate::merge_receipt::find(&home, REPO, &sha, "t-idem").is_some(),
        "receipt must survive pre-terminal repeat"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// notification_only on a non-protected branch must be rejected.
#[test]
fn notification_only_non_protected_ref_rejected() {
    let home = tmp_home("non-protected");
    seed_fleet(&home, &["dev"]);
    let sha = "a".repeat(40);
    seed_binding(&home, "dev", "t-np");
    seed_receipt(&home, REPO, &sha, "t-np", "dev");
    let r = handle_watch_ci(
        &home,
        &json!({
            "repository": REPO,
            "branch": "feat/not-protected",
            "head_sha": sha,
            "task_id": "t-np",
            "notification_only": true,
        }),
        "dev",
    );
    assert!(
        r.get("error").is_some(),
        "non-protected must be rejected: {r}"
    );
    assert_eq!(
        r["code"].as_str().unwrap_or(""),
        "notification_only_non_protected",
        "expected notification_only_non_protected code: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Empty/operator caller must not bypass notification_only assignee authority.
#[test]
fn notification_only_empty_caller_rejected() {
    let home = tmp_home("empty-caller");
    seed_fleet(&home, &["dev"]);
    let sha = "b".repeat(40);
    seed_binding(&home, "dev", "t-ec");
    seed_receipt(&home, REPO, &sha, "t-ec", "dev");
    let r = handle_watch_ci(
        &home,
        &json!({
            "repository": REPO,
            "branch": "main",
            "head_sha": sha,
            "task_id": "t-ec",
            "notification_only": true,
        }),
        "",
    );
    assert!(
        r.get("error").is_some(),
        "empty caller must be rejected: {r}"
    );
    assert_eq!(
        r["code"].as_str().unwrap_or(""),
        "notification_only_empty_caller",
        "expected notification_only_empty_caller code: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// notification_only arming must clear stale next_after_ci from persisted watch.
#[test]
fn notification_only_clears_stale_next_after_ci() {
    let home = tmp_home("clear-nac");
    seed_fleet(&home, &["dev", "reviewer"]);
    let sha = "c".repeat(40);
    seed_binding(&home, "dev", "t-cn");
    seed_receipt(&home, REPO, &sha, "t-cn", "dev");

    // First arm a notification_only watch.
    let r = handle_watch_ci(
        &home,
        &json!({
            "repository": REPO,
            "branch": "main",
            "head_sha": sha,
            "task_id": "t-cn",
            "notification_only": true,
        }),
        "dev",
    );
    assert!(r.get("error").is_none(), "arm must succeed: {r}");

    // Read the persisted watch and verify next_after_ci is absent/null.
    let watch_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
    let mut found_nac = false;
    if let Ok(entries) = std::fs::read_dir(&watch_dir) {
        for entry in entries.flatten() {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                if content.contains("notification_only") {
                    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
                    let nac = parsed.get("next_after_ci");
                    assert!(
                        nac.is_none()
                            || nac.unwrap().is_null()
                            || nac.unwrap().as_array().is_some_and(|a| a.is_empty()),
                        "notification_only watch must have empty/null next_after_ci: {parsed}"
                    );
                    found_nac = true;
                }
            }
        }
    }
    assert!(found_nac, "notification_only watch must exist in watch dir");
    std::fs::remove_dir_all(&home).ok();
}

/// Strengthened: first arm a privileged watch with non-empty next_after_ci,
/// then re-arm as notification_only — persisted next_after_ci must be gone.
#[test]
fn notification_only_rearm_clears_preexisting_next_after_ci() {
    let home = tmp_home("rearm-clears-nac");
    seed_fleet(&home, &["dev", "reviewer"]);
    let sha = "d".repeat(40);
    seed_binding(&home, "dev", "t-rearm");
    seed_receipt(&home, REPO, &sha, "t-rearm", "dev");

    // First arm a privileged exact-head watch WITH next_after_ci.
    let r1 = handle_watch_ci(
        &home,
        &json!({
            "repository": REPO,
            "branch": "main",
            "head_sha": sha,
            "task_id": "t-rearm",
            "next_after_ci": "reviewer",
        }),
        "", // operator/orchestrator
    );
    assert!(
        r1.get("error").is_none(),
        "privileged arm must succeed: {r1}"
    );

    // Verify next_after_ci is set.
    let watch_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
    let watch_file = find_watch_with_sha(&watch_dir, &sha);
    assert!(
        watch_file.is_some(),
        "watch must exist after privileged arm"
    );
    let content = std::fs::read_to_string(watch_file.as_ref().unwrap()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert!(
        parsed.get("next_after_ci").is_some_and(|v| !v.is_null()),
        "privileged watch must have next_after_ci: {parsed}"
    );

    // Re-arm as notification_only.
    let r2 = handle_watch_ci(
        &home,
        &json!({
            "repository": REPO,
            "branch": "main",
            "head_sha": sha,
            "task_id": "t-rearm",
            "notification_only": true,
        }),
        "dev",
    );
    assert!(
        r2.get("error").is_none(),
        "notification_only re-arm must succeed: {r2}"
    );

    // next_after_ci must now be absent/null.
    let content2 = std::fs::read_to_string(watch_file.as_ref().unwrap()).unwrap();
    let parsed2: serde_json::Value = serde_json::from_str(&content2).unwrap();
    let nac = parsed2.get("next_after_ci");
    assert!(
        nac.is_none()
            || nac.unwrap().is_null()
            || nac.unwrap().as_array().is_some_and(|a| a.is_empty()),
        "notification_only re-arm must clear pre-existing next_after_ci: {parsed2}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Poller defense: a WatchState with notification_only=true + stale
/// next_after_ci cannot enqueue any privileged handoff.
#[test]
fn poller_ignores_stale_next_after_ci_when_notification_only() {
    use crate::daemon::ci_watch::watch_state::WatchState;

    // notification_only=true + stale target → actionable returns empty
    let state: WatchState = serde_json::from_value(json!({
        "repo": REPO,
        "branch": "main",
        "interval_secs": 60,
        "subscribers": [],
        "target_head_sha": "e".repeat(40),
        "notification_only": true,
        "next_after_ci": ["reviewer"],
        "task_id": "t-poller-defense",
        "expires_at": (chrono::Utc::now() + chrono::TimeDelta::try_hours(1).unwrap()).to_rfc3339(),
    }))
    .unwrap();
    assert!(
        state.actionable_next_after_ci_targets().is_empty(),
        "notification_only=true must suppress all next_after_ci targets"
    );
    assert_eq!(
        state.next_after_ci_targets(),
        vec!["reviewer"],
        "raw next_after_ci_targets must still return the stored value"
    );

    // ordinary mode + target → actionable returns the target (regression)
    let ordinary: WatchState = serde_json::from_value(json!({
        "repo": REPO,
        "branch": "main",
        "interval_secs": 60,
        "subscribers": [],
        "next_after_ci": ["reviewer"],
    }))
    .unwrap();
    assert_eq!(
        ordinary.actionable_next_after_ci_targets(),
        vec!["reviewer"],
        "ordinary mode must return next_after_ci targets"
    );
}

/// Privileged reverse mode change: re-arming the same watch as privileged
/// (notification_only=false) with authorized next_after_ci must CLEAR the
/// old notification_only flag and allow the target.
#[test]
fn privileged_rearm_clears_notification_only_flag() {
    let home = tmp_home("reverse-rearm");
    seed_fleet(&home, &["dev", "reviewer"]);
    let sha = "f".repeat(40);
    seed_binding(&home, "dev", "t-reverse");
    seed_receipt(&home, REPO, &sha, "t-reverse", "dev");

    // First arm as notification_only.
    let r1 = handle_watch_ci(
        &home,
        &json!({
            "repository": REPO,
            "branch": "main",
            "head_sha": sha,
            "task_id": "t-reverse",
            "notification_only": true,
        }),
        "dev",
    );
    assert!(
        r1.get("error").is_none(),
        "notification_only arm must succeed: {r1}"
    );

    // Verify notification_only is set.
    let watch_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
    let watch_file = find_watch_with_sha(&watch_dir, &sha);
    let content = std::fs::read_to_string(watch_file.as_ref().unwrap()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(parsed["notification_only"], true);

    // Re-arm as privileged with next_after_ci.
    let r2 = handle_watch_ci(
        &home,
        &json!({
            "repository": REPO,
            "branch": "main",
            "head_sha": sha,
            "task_id": "t-reverse",
            "next_after_ci": "reviewer",
        }),
        "", // operator/orchestrator
    );
    assert!(
        r2.get("error").is_none(),
        "privileged re-arm must succeed: {r2}"
    );

    // notification_only must now be false/absent.
    let content2 = std::fs::read_to_string(watch_file.as_ref().unwrap()).unwrap();
    let parsed2: serde_json::Value = serde_json::from_str(&content2).unwrap();
    let no = parsed2.get("notification_only");
    assert!(
        no.is_none() || no.unwrap().is_null() || no.unwrap() == false,
        "privileged re-arm must clear notification_only flag: {parsed2}"
    );
    assert!(
        parsed2.get("next_after_ci").is_some_and(|v| !v.is_null()),
        "privileged re-arm must set next_after_ci: {parsed2}"
    );
    std::fs::remove_dir_all(&home).ok();
}

fn find_watch_with_sha(watch_dir: &std::path::Path, sha: &str) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(watch_dir).ok()?;
    for entry in entries.flatten() {
        if let Ok(content) = std::fs::read_to_string(entry.path()) {
            if content.contains(sha) {
                return Some(entry.path());
            }
        }
    }
    None
}
