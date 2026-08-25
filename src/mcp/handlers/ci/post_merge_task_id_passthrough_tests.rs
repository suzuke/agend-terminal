//! Task-id passthrough for the post-merge watch resolve — the author-directed
//! rework of the release-before-merge ordering fix (supersedes the rejected
//! board-scan fallback).
//!
//! Contract: `repo(action=merge, ..., task_id)` hands the post-merge hook its
//! linkage directly. With `task_id` present, the assignee is read from that
//! ONE task via the strict router — zero search, zero ambiguity; a missing /
//! wrong-branch / terminal / assignee-less task fails closed with the SPECIFIC
//! reason surfaced in the MCP response (`{"skipped": "task_id '...' ..."}`) —
//! the same silent-degradation failure mode #3341 was about, so the cause must
//! be visible at the call site, not buried in the daemon log. Absent → the
//! legacy live-binding scan runs byte-identical to main (and its own skip
//! wording — that one genuinely is about bindings).

use serde_json::json;

fn tmp_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let h = std::env::temp_dir().join(format!(
        "agend-pmw-taskid-pass-{}-{}-{}",
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

/// Seed an assigned open-work task via the public tasks API.
fn seed_task(home: &std::path::Path, title: &str, branch: &str, assignee: &str) -> String {
    let created = crate::tasks::handle(
        home,
        "lead",
        &json!({"action": "create", "title": title, "branch": branch, "assignee": assignee}),
    );
    let id = created["task"]["id"]
        .as_str()
        .or_else(|| created["id"].as_str())
        .expect("created task id")
        .to_string();
    crate::tasks::handle(home, assignee, &json!({"action": "claim", "task_id": id}));
    id
}

// ── AC1: explicit task_id → receipt persist + watch armed ──

#[test]
fn post_merge_task_id_passthrough_arms_watch() {
    let home = tmp_home("taskid-hit");
    let sha = "a".repeat(40);
    // No bindings at all — passthrough must not need them.
    seed_fleet(&home, &["lead", "dev"]);
    let task_id = seed_task(&home, "impl work", "fix/passthrough-x", "dev");

    let diag = super::merge::post_merge_receipt_and_watch(
        &home,
        "suzuke/agend-terminal",
        &sha,
        7,
        "fix/passthrough-x",
        "lead",
        Some(&task_id),
    );

    assert_eq!(
        diag["receipt"], "persisted",
        "passthrough must persist the receipt: {diag}"
    );
    assert_eq!(
        diag["assignee"], "dev",
        "assignee read from the task: {diag}"
    );
    assert_eq!(diag["watch"], "armed", "watch must be armed: {diag}");

    let watch_path =
        home.join("ci-watches")
            .join(crate::daemon::ci_watch::watch_filename_exact_head(
                "suzuke/agend-terminal",
                "main",
                &sha,
            ));
    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(watch_path).unwrap()).unwrap();
    assert_eq!(watch["task_id"], task_id);
    assert_eq!(
        watch["next_after_ci"], "lead",
        "named merge authority is the post-CI handoff target"
    );
    let receipt = crate::merge_receipt::find(&home, "suzuke/agend-terminal", &sha, &task_id);
    assert!(receipt.is_some(), "receipt findable on disk");
    let r = receipt.unwrap();
    assert_eq!(r.task_assignee, "dev");
    assert_eq!(r.merge_authority, "lead");
    assert_eq!(r.pr_number, 7);
    std::fs::remove_dir_all(&home).ok();
}

// ── AC2: absent task_id → legacy stage-1 behavior unchanged ──

#[test]
fn post_merge_absent_task_id_keeps_legacy_resolve() {
    let home = tmp_home("taskid-absent-legacy");
    let sha = "b".repeat(40);
    seed_fleet(&home, &["lead"]);
    // No binding, no task → same skip as main.
    let diag_none = super::merge::post_merge_receipt_and_watch(
        &home,
        "suzuke/agend-terminal",
        &sha,
        8,
        "fix/legacy-x",
        "lead",
        None,
    );
    assert!(
        diag_none["skipped"].as_str().is_some(),
        "absent task_id + no binding → legacy skip: {diag_none}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── AC3a: unknown task_id → fail-closed skip ──

#[test]
fn post_merge_unknown_task_id_fails_closed() {
    let home = tmp_home("taskid-missing");
    let sha = "c".repeat(40);
    seed_fleet(&home, &["lead"]);

    let diag = super::merge::post_merge_receipt_and_watch(
        &home,
        "suzuke/agend-terminal",
        &sha,
        9,
        "fix/whatever",
        "lead",
        Some("t-20990101000000-999999-999"),
    );

    assert!(
        diag["skipped"].as_str().is_some(),
        "unknown task_id → fail-closed skip: {diag}"
    );
    let skipped = diag["skipped"].as_str().unwrap();
    assert_eq!(
        skipped, "task_id 't-20990101000000-999999-999' not found",
        "skip message must name the specific failure: {diag}"
    );
    assert!(
        crate::merge_receipt::find(
            &home,
            "suzuke/agend-terminal",
            &sha,
            "t-20990101000000-999999-999"
        )
        .is_none(),
        "no receipt persisted for a failed resolve"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── AC3b: terminal task → fail-closed skip ──

#[test]
fn post_merge_terminal_task_fails_closed() {
    let home = tmp_home("taskid-terminal");
    let sha = "d".repeat(40);
    seed_fleet(&home, &["lead", "dev"]);
    let task_id = seed_task(&home, "finished work", "fix/done-y", "dev");
    // The task owner (assignee) cancels — the update ACL rejects a non-owner.
    crate::tasks::handle(
        &home,
        "dev",
        &json!({"action": "update", "task_id": task_id, "status": "cancelled"}),
    );

    let diag = super::merge::post_merge_receipt_and_watch(
        &home,
        "suzuke/agend-terminal",
        &sha,
        10,
        "fix/done-y",
        "lead",
        Some(&task_id),
    );

    assert!(
        diag["skipped"].as_str().is_some(),
        "terminal task → fail-closed skip: {diag}"
    );
    let skipped = diag["skipped"].as_str().unwrap();
    assert!(
        skipped.starts_with(&format!("task_id '{task_id}' is terminal (")),
        "skip message must name the task and its terminal status: {diag}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── AC3c: unassigned task → fail-closed skip ──

#[test]
fn post_merge_unassigned_task_fails_closed() {
    let home = tmp_home("taskid-unassigned");
    let sha = "e".repeat(40);
    seed_fleet(&home, &["lead"]);
    // Created WITHOUT an assignee and never claimed.
    let created = crate::tasks::handle(
        &home,
        "lead",
        &json!({"action": "create", "title": "orphan work", "branch": "fix/no-owner"}),
    );
    let task_id = created["task"]["id"]
        .as_str()
        .expect("created task id")
        .to_string();

    let diag = super::merge::post_merge_receipt_and_watch(
        &home,
        "suzuke/agend-terminal",
        &sha,
        11,
        "fix/no-owner",
        "lead",
        Some(&task_id),
    );

    assert!(
        diag["skipped"].as_str().is_some(),
        "assignee-less task → fail-closed skip: {diag}"
    );
    assert_eq!(
        diag["skipped"].as_str().unwrap(),
        format!("task_id '{task_id}' has no assignee"),
        "skip message must name the specific failure: {diag}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── AC3d: task_id naming a DIFFERENT healthy task → fail-closed skip ──
// The passed-in id is verified against the branch being merged, not trusted:
// a copy-paste / multi-PR mixup must not arm a watch for someone else's work.

#[test]
fn post_merge_wrong_task_for_branch_fails_closed() {
    let home = tmp_home("taskid-wrong-task");
    let sha = "f".repeat(40);
    seed_fleet(&home, &["lead", "dev1", "dev2"]);
    let _task_a = seed_task(&home, "work A", "fix/branch-a", "dev1");
    let task_b = seed_task(&home, "work B", "fix/branch-b", "dev2");

    // Merging branch-a but passing B's id — both tasks are routable,
    // non-terminal, and assigned, so ONLY the branch check catches this.
    let diag = super::merge::post_merge_receipt_and_watch(
        &home,
        "suzuke/agend-terminal",
        &sha,
        12,
        "fix/branch-a",
        "lead",
        Some(&task_b),
    );

    assert!(
        diag["skipped"].as_str().is_some(),
        "task_id naming another branch's task → fail-closed skip (no fallback to the \
         binding scan — a mismatched hint means the caller is buggy): {diag}"
    );
    let skipped = diag["skipped"].as_str().unwrap();
    assert_eq!(
        skipped,
        format!("task_id '{task_b}' names branch 'fix/branch-b', not 'fix/branch-a'"),
        "wrong-task skip must show BOTH branches so a copy-paste mixup is \
         immediately diagnosable: {diag}"
    );
    assert!(
        crate::merge_receipt::find(&home, "suzuke/agend-terminal", &sha, &task_b).is_none(),
        "no receipt persisted for the wrong task"
    );
    std::fs::remove_dir_all(&home).ok();
}
