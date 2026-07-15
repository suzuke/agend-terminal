//! #2782 slice 1: orchestrator-authorized exact review-assignment revoke tests.
//!
//! Re-homed from `assignment_authority.rs` to keep that file under the
//! `src_file_size_invariant` anti-monolith ceiling — same sibling-file
//! precedent as `mcp::handlers::instance_964_tests`.

use crate::daemon::assignment_authority::*;
use crate::daemon::pr_state::ReviewClass;
use crate::identity::Sender;
use crate::mcp::handlers::comms_gates::ReviewAuthor;
use crate::mcp::handlers::review_assignment::handle_revoke_review_assignment;
use std::path::{Path, PathBuf};

fn tmp_home(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let id = C.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("agend-asgn-{}-{}-{}", std::process::id(), tag, id));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn mk_record(
    repo: &str,
    branch: &str,
    target: &str,
    pr: u64,
    created_at: &str,
) -> ActiveAssignment {
    ActiveAssignment::new_pending(
        repo,
        branch,
        target,
        pr,
        "lead",
        "t-orig-1",
        ReviewClass::Dual,
        ReviewAuthor::External("octocat".into()),
        "Please review PR",
        Some("thr-1".into()),
        Some("par-1".into()),
        created_at,
    )
}

/// Seed a minimal fleet.yaml with ONE team so `resolve_team_by_source_repo`
/// can authorize the handler's orchestrator check.
fn seed_team_fleet(home: &Path, orchestrator: &str, source_repo: &str) {
    let yaml = format!(
        "instances:\n  {orchestrator}:\n    backend: claude\n    id: 11111111-1111-4111-8111-111111111111\n\
         teams:\n  t-revoke:\n    orchestrator: {orchestrator}\n    members:\n      - {orchestrator}\n    source_repo: {source_repo}\n"
    );
    std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
}

/// t40: the team's SOLE CURRENT orchestrator revokes by exact `assignment_id` ⇒
/// success, the record is gone via `get`, and `redrive_reserved` leaves no
/// reserved assignment behind (merge readiness recomputed).
#[test]
fn t40_revoke_by_assignment_id_authorized_orchestrator() {
    let home = tmp_home("t40-authorized");
    seed_team_fleet(&home, "lead", "o/r");
    let rec = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-15T00:00:00Z");
    persist(&home, &rec).unwrap();

    let mut state = crate::daemon::pr_state::new_for_branch(
        "o/r",
        "feat/x",
        "a".repeat(40).as_str(),
        ReviewClass::Dual,
    );
    state.pr_number = 42;
    state.reserved_assignments = vec![crate::daemon::pr_state::ReservedAssignment {
        target: "reviewer".to_string(),
        review_author: ReviewAuthor::External("octocat".into()),
        assignment_id: rec.assignment_id,
    }];
    crate::daemon::pr_state::save(&home, &state).unwrap();

    let sender = Some(Sender::new("lead").unwrap());
    let args = serde_json::json!({"assignment_id": rec.assignment_id.to_string()});
    let result = handle_revoke_review_assignment(&home, &args, &sender);

    assert_eq!(
        result["ok"], true,
        "authorized revoke must succeed: {result}"
    );
    assert_eq!(
        result["revoked"], true,
        "the live record must be retired: {result}"
    );
    assert!(
        get(&home, "o/r", "feat/x", "reviewer").is_none(),
        "record must be gone after revoke"
    );
    let reloaded = crate::daemon::pr_state::load(&home, "o/r", "feat/x")
        .expect("pr_state must still exist after redrive");
    assert!(
        reloaded.reserved_assignments.is_empty(),
        "redrive_reserved must clear the reserved entry after the assignment is revoked: {reloaded:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// t41: a sender that is NOT the team's orchestrator ⇒ denied, and the
/// assignment must still be present (no partial mutation on rejection).
#[test]
fn t41_revoke_by_assignment_id_unauthorized_agent_denied() {
    let home = tmp_home("t41-unauthorized");
    seed_team_fleet(&home, "lead", "o/r");
    let rec = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-15T00:00:00Z");
    persist(&home, &rec).unwrap();

    let sender = Some(Sender::new("intruder").unwrap());
    let args = serde_json::json!({"assignment_id": rec.assignment_id.to_string()});
    let result = handle_revoke_review_assignment(&home, &args, &sender);

    assert_eq!(
        result["code"], "revoke_assignment_not_authorized",
        "non-orchestrator sender must be denied: {result}"
    );
    assert!(
        get(&home, "o/r", "feat/x", "reviewer").is_some(),
        "the assignment must survive an unauthorized revoke attempt"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// t42: revoking the SAME `assignment_id` a second time (after it already
/// succeeded) is a safe idempotent no-op — no panic, no error.
#[test]
fn t42_revoke_by_assignment_id_idempotent_retry() {
    let home = tmp_home("t42-idempotent");
    seed_team_fleet(&home, "lead", "o/r");
    let rec = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-15T00:00:00Z");
    persist(&home, &rec).unwrap();

    let sender = Some(Sender::new("lead").unwrap());
    let args = serde_json::json!({"assignment_id": rec.assignment_id.to_string()});

    let first = handle_revoke_review_assignment(&home, &args, &sender);
    assert_eq!(first["ok"], true, "first revoke must succeed: {first}");

    let second = handle_revoke_review_assignment(&home, &args, &sender);
    assert_eq!(
        second["ok"], true,
        "second revoke of the same assignment_id must still be ok: {second}"
    );
    assert!(
        second["already_absent"] == true || second["revoked"] == false,
        "second revoke must report either already_absent or revoked:false: {second}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// t43: revoking assignment A (reviewer-1) on a branch must NOT touch a
/// co-resident assignment B (reviewer-2) on the SAME (repo,branch).
#[test]
fn t43_revoke_does_not_affect_other_assignment() {
    let home = tmp_home("t43-isolation");
    seed_team_fleet(&home, "lead", "o/r");
    let rec_a = mk_record("o/r", "feat/x", "reviewer-1", 42, "2026-07-15T00:00:00Z");
    let rec_b = mk_record("o/r", "feat/x", "reviewer-2", 42, "2026-07-15T00:00:00Z");
    persist(&home, &rec_a).unwrap();
    persist(&home, &rec_b).unwrap();

    let sender = Some(Sender::new("lead").unwrap());
    let args = serde_json::json!({"assignment_id": rec_a.assignment_id.to_string()});
    let result = handle_revoke_review_assignment(&home, &args, &sender);

    assert_eq!(result["ok"], true, "revoking A must succeed: {result}");
    assert!(
        get(&home, "o/r", "feat/x", "reviewer-1").is_none(),
        "A must be gone"
    );
    assert!(
        get(&home, "o/r", "feat/x", "reviewer-2").is_some(),
        "B must be untouched by A's revoke"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// t44: a STALE `assignment_id` (from an already-revoked generation) must NOT
/// retire a NEW assignment persisted afterward for the SAME target.
#[test]
fn t44_revoke_stale_generation_safe() {
    let home = tmp_home("t44-stale-gen");
    seed_team_fleet(&home, "lead", "o/r");
    let rec_a = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-15T00:00:00Z");
    persist(&home, &rec_a).unwrap();

    let sender = Some(Sender::new("lead").unwrap());
    let revoke_a_args = serde_json::json!({"assignment_id": rec_a.assignment_id.to_string()});
    let revoke_a = handle_revoke_review_assignment(&home, &revoke_a_args, &sender);
    assert_eq!(revoke_a["ok"], true, "revoking A must succeed: {revoke_a}");

    let rec_b = mk_record("o/r", "feat/x", "reviewer", 43, "2026-07-15T00:05:00Z");
    persist(&home, &rec_b).unwrap();

    let retry_a = handle_revoke_review_assignment(&home, &revoke_a_args, &sender);
    assert_eq!(
        retry_a["ok"], true,
        "stale retry must still be ok: {retry_a}"
    );
    let still_there = get(&home, "o/r", "feat/x", "reviewer").expect("B must survive");
    assert_eq!(
        still_there.assignment_id, rec_b.assignment_id,
        "B (the new generation) must be untouched by a stale A revoke retry"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// t45: operator-direct (no sender identity) has full revoke authority.
#[test]
fn t45_revoke_by_operator_no_sender_allowed() {
    let home = tmp_home("t45-operator");
    seed_team_fleet(&home, "lead", "o/r");
    let rec = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-15T00:00:00Z");
    persist(&home, &rec).unwrap();

    let args = serde_json::json!({"assignment_id": rec.assignment_id.to_string()});
    let result = handle_revoke_review_assignment(&home, &args, &None);

    assert_eq!(
        result["ok"], true,
        "operator-direct revoke must succeed: {result}"
    );
    assert_eq!(result["revoked"], true, "{result}");
    assert!(
        get(&home, "o/r", "feat/x", "reviewer").is_none(),
        "record must be gone after operator revoke"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #2782 slice 1 r2: fail-closed integrity tests ──

/// t46: a LIVE matching assignment coexists with an UNRELATED corrupt record
/// file on the same branch. The strict lookup must surface the corruption
/// as a store integrity error, not silently collapse it into `already_absent`.
#[test]
fn t46_revoke_corrupt_coexisting_row_fails_closed() {
    let home = tmp_home("t46-corrupt");
    seed_team_fleet(&home, "lead", "o/r");
    let rec = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-15T00:00:00Z");
    persist(&home, &rec).unwrap();

    let bdir = branch_dir(&home, "o/r", "feat/x");
    let corrupt_path = bdir.join("corrupt--aaaa.json");
    std::fs::write(&corrupt_path, b"NOT VALID JSON").unwrap();

    let sender = Some(Sender::new("lead").unwrap());
    let args = serde_json::json!({"assignment_id": rec.assignment_id.to_string()});
    let result = handle_revoke_review_assignment(&home, &args, &sender);

    assert_eq!(
        result["code"], "revoke_assignment_store_integrity",
        "corrupt coexisting row must fail closed: {result}"
    );
    assert!(
        get(&home, "o/r", "feat/x", "reviewer").is_some(),
        "the live assignment must be preserved when revoke fails closed"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// t47: duplicate assignment_id across two record files must fail closed.
#[test]
fn t47_revoke_duplicate_uuid_fails_closed() {
    let home = tmp_home("t47-dup-uuid");
    seed_team_fleet(&home, "lead", "o/r");
    let rec = mk_record("o/r", "feat/x", "reviewer-1", 42, "2026-07-15T00:00:00Z");
    persist(&home, &rec).unwrap();

    let mut dup = rec.clone();
    dup.target = "reviewer-2".to_string();
    persist(&home, &dup).unwrap();

    let sender = Some(Sender::new("lead").unwrap());
    let args = serde_json::json!({"assignment_id": rec.assignment_id.to_string()});
    let result = handle_revoke_review_assignment(&home, &args, &sender);

    assert_eq!(
        result["code"], "revoke_assignment_store_integrity",
        "duplicate UUID must fail closed: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// t48: missing assignment_id (never persisted) returns idempotent success,
/// and a stale/terminal assignment_id also returns idempotent success —
/// confirming the boundary between "absent → ok" and "corrupt → error".
#[test]
fn t48_revoke_missing_and_terminal_idempotent() {
    let home = tmp_home("t48-missing-terminal");
    seed_team_fleet(&home, "lead", "o/r");

    let sender = Some(Sender::new("lead").unwrap());
    let unknown_id = uuid::Uuid::new_v4();
    let args = serde_json::json!({"assignment_id": unknown_id.to_string()});
    let result = handle_revoke_review_assignment(&home, &args, &sender);
    assert_eq!(
        result["ok"], true,
        "unknown UUID must be idempotent success: {result}"
    );
    assert_eq!(result["already_absent"], true, "{result}");

    let rec = mk_record("o/r", "feat/y", "reviewer", 42, "2026-07-15T00:00:00Z");
    persist(&home, &rec).unwrap();
    record_terminal(&home, "o/r", "feat/y", 42, TerminalKind::Merged).unwrap();

    let args2 = serde_json::json!({"assignment_id": rec.assignment_id.to_string()});
    let result2 = handle_revoke_review_assignment(&home, &args2, &sender);
    assert_eq!(
        result2["ok"], true,
        "terminal generation must be idempotent success: {result2}"
    );
    assert_eq!(result2["already_absent"], true, "{result2}");

    std::fs::remove_dir_all(&home).ok();
}
