//! S1 exact-head protected-main watch — handler gate tests (test-first).
//!
//! Contract (decision d-20260712033954660984-4): a watch on a protected ref
//! (`main`/`master`) is accepted ONLY as an exact-head post-merge watch —
//! full immutable `head_sha` + `task_id` + explicit `next_after_ci`, created
//! by the target team orchestrator or operator, on a GitHub repo. A generic
//! protected watch stays E4.5-rejected. These pin the handler gate; the
//! poller-freshness + sweep behaviors are pinned in their own modules.

use super::watch::handle_watch_ci;
use serde_json::json;

fn tmp_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let h = std::env::temp_dir().join(format!(
        "agend-exact-head-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    std::fs::create_dir_all(&h).unwrap();
    h
}

/// Seed a team where `orchestrator` orchestrates `member`, so
/// `teams::is_orchestrator_of(home, orchestrator, member)` is true.
fn seed_team(home: &std::path::Path, orchestrator: &str, member: &str) {
    let yaml = format!(
        "teams:\n  post-merge-team:\n    members:\n      - {member}\n    orchestrator: {orchestrator}\n    created_at: \"2026-01-01T00:00:00Z\"\n"
    );
    std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
}

const FULL_SHA: &str = "c4206950c4206950c4206950c4206950c4206950"; // 40-hex

fn exact_head_args(head_sha: &str) -> serde_json::Value {
    json!({
        "repository": "suzuke/agend-terminal",
        "branch": "main",
        "head_sha": head_sha,
        "task_id": "t-1",
        "next_after_ci": ["reviewer-x"],
    })
}

/// Generic protected watch (no head_sha) stays E4.5-rejected — the exact-head
/// path must NOT open a generic bypass.
#[test]
fn generic_main_watch_still_e4_5_rejected() {
    let home = tmp_home("generic-reject");
    seed_team(&home, "lead", "reviewer-x");
    let r = handle_watch_ci(
        &home,
        &json!({"repository": "suzuke/agend-terminal", "branch": "main"}),
        "lead",
    );
    assert_eq!(
        r["code"].as_str(),
        Some("e4_5_protected_branch"),
        "generic main (no head_sha) must stay E4.5-rejected: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Authorized orchestrator + full SHA + task_id + next_after_ci → accepted.
/// RED today (E4.5 rejects everything on main); GREEN after the gate lands.
#[test]
fn exact_head_main_accepted_for_orchestrator() {
    let home = tmp_home("accept-orch");
    seed_team(&home, "lead", "reviewer-x");
    let r = handle_watch_ci(&home, &exact_head_args(FULL_SHA), "lead");
    assert_eq!(
        r["watching"].as_bool(),
        Some(true),
        "authorized exact-head main watch must be accepted: {r}"
    );
    assert!(r.get("error").is_none(), "no error on accept: {r}");
    std::fs::remove_dir_all(&home).ok();
}

/// Operator (empty caller) bypasses the orchestrator check but still needs the
/// full triple (SHA + task_id + next_after_ci).
#[test]
fn exact_head_main_accepted_for_operator() {
    let home = tmp_home("accept-op");
    // No team seeded — operator authority is caller-identity, not membership.
    let r = handle_watch_ci(&home, &exact_head_args(FULL_SHA), "");
    assert_eq!(
        r["watching"].as_bool(),
        Some(true),
        "operator exact-head main watch must be accepted: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// A caller who is NOT the target team orchestrator (nor operator) is rejected
/// on AUTHORITY — distinct from the generic E4.5 rejection.
#[test]
fn exact_head_main_rejected_for_unauthorized_caller() {
    let home = tmp_home("reject-unauth");
    seed_team(&home, "lead", "reviewer-x");
    let r = handle_watch_ci(&home, &exact_head_args(FULL_SHA), "dev");
    assert_eq!(
        r["code"].as_str(),
        Some("protected_watch_unauthorized"),
        "a non-orchestrator caller must be rejected on authority: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Missing task_id → rejected (exact-head requires the close-loop triple).
#[test]
fn exact_head_main_rejected_without_task_id() {
    let home = tmp_home("reject-no-task");
    seed_team(&home, "lead", "reviewer-x");
    let mut args = exact_head_args(FULL_SHA);
    args.as_object_mut().unwrap().remove("task_id");
    let r = handle_watch_ci(&home, &args, "lead");
    assert_eq!(
        r["code"].as_str(),
        Some("protected_watch_missing_requirements"),
        "exact-head without task_id must be rejected: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Missing next_after_ci → rejected.
#[test]
fn exact_head_main_rejected_without_next_after_ci() {
    let home = tmp_home("reject-no-next");
    seed_team(&home, "lead", "reviewer-x");
    let mut args = exact_head_args(FULL_SHA);
    args.as_object_mut().unwrap().remove("next_after_ci");
    let r = handle_watch_ci(&home, &args, "lead");
    assert_eq!(
        r["code"].as_str(),
        Some("protected_watch_missing_requirements"),
        "exact-head without next_after_ci must be rejected: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Abbreviated / non-full SHA → rejected (immutable-target requirement).
#[test]
fn exact_head_main_rejected_for_abbreviated_sha() {
    let home = tmp_home("reject-shortsha");
    seed_team(&home, "lead", "reviewer-x");
    let r = handle_watch_ci(&home, &exact_head_args("c420695"), "lead"); // 7-hex
    assert_eq!(
        r["code"].as_str(),
        Some("protected_watch_invalid_sha"),
        "abbreviated SHA must be rejected: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// A non-GitHub repository can never arm an exact-head watch. Repo resolution
/// (`canonicalize_repo_slug` / `derive_repo_from_remote`) already rejects every
/// non-GitHub remote before the gate (the daemon only polls GitHub Actions), so
/// the observable contract is "no exact-head watch for a non-GitHub repo". The
/// handler's `detect_provider_from_remote != github` check is a backstop for the
/// binding-derived edge. (Reported to codex: the upstream layer makes the backstop
/// effectively unreachable — candidate for removal per KISS.)
#[test]
fn exact_head_main_rejected_for_non_github_repo() {
    let home = tmp_home("reject-nongh");
    seed_team(&home, "lead", "reviewer-x");
    let mut args = exact_head_args(FULL_SHA);
    args.as_object_mut().unwrap().insert(
        "repository".to_string(),
        json!("https://gitlab.com/suzuke/agend-terminal"),
    );
    let r = handle_watch_ci(&home, &args, "lead");
    assert_ne!(
        r["watching"].as_bool(),
        Some(true),
        "a non-GitHub repo must not arm an exact-head watch: {r}"
    );
    assert!(
        r.get("error").is_some() && r.get("code").is_some(),
        "non-GitHub repo must reject with a structured error+code: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// A non-protected branch is unaffected by the exact-head gate: `head_sha` is
/// simply ignored and a normal branch watch is created.
#[test]
fn non_protected_branch_watch_unaffected_by_head_sha() {
    let home = tmp_home("nonprot");
    let r = handle_watch_ci(
        &home,
        &json!({"repository": "suzuke/agend-terminal", "branch": "feat/x", "head_sha": FULL_SHA}),
        "dev",
    );
    assert_eq!(
        r["watching"].as_bool(),
        Some(true),
        "non-protected branch watch must still work: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Auto-watch guard (decision point 3): `head_sha` is INERT on a non-protected
/// branch — `target_head_sha` is only ever persisted behind the protected-ref
/// exact-head gate. The dispatch auto-arm path arms feature branches (never
/// main/master — E4.5 rejects that at dispatch) and passes no head_sha, so it can
/// never mint an exact-head watch; this pins that even a leaked head_sha stays inert.
#[test]
fn head_sha_inert_on_non_protected_branch_no_target_persisted() {
    let home = tmp_home("nonprot-no-persist");
    let r = handle_watch_ci(
        &home,
        &json!({
            "repository": "suzuke/agend-terminal", "branch": "feat/x",
            "head_sha": FULL_SHA, "task_id": "t-1", "next_after_ci": ["reviewer-x"],
        }),
        "dev",
    );
    assert_eq!(r["watching"].as_bool(), Some(true), "{r}");
    // The single persisted watch file must NOT carry target_head_sha.
    let ci_dir = home.join("ci-watches");
    let entry = std::fs::read_dir(&ci_dir)
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().is_some_and(|x| x == "json"))
        .expect("a watch file was written");
    let watch: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(entry.path()).unwrap()).unwrap();
    assert!(
        watch.get("target_head_sha").is_none(),
        "a non-protected-branch watch must never carry target_head_sha: {watch}"
    );
    std::fs::remove_dir_all(&home).ok();
}
