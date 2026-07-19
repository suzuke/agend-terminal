//! Sprint 55 P0-C — `dispatch_should_skip_auto_bind` helper tests.
//!
//! Located in this sibling file (loaded via `#[path]` from comms.rs) to
//! keep src/mcp/handlers/comms.rs under the file_size_invariant 700 LOC
//! ceiling. Same module layout pattern as the
//! `instance_state::lifecycle` split.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::dispatch_should_skip_auto_bind;
use crate::mcp::handlers::dispatch::RuntimeContext;
use serde_json::json;

fn minimal_runtime() -> RuntimeContext {
    RuntimeContext {
        registry: std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        configs: Default::default(),
        externals: std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: None,
        notifier: None,
        shutdown: None,
    }
}

#[test]
fn skip_auto_bind_when_bind_false() {
    let args = json!({"bind": false, "branch": "feat/x"});
    assert!(dispatch_should_skip_auto_bind(&args));
}

#[test]
fn proceed_auto_bind_when_bind_true() {
    let args = json!({"bind": true, "branch": "feat/x"});
    assert!(!dispatch_should_skip_auto_bind(&args));
}

#[test]
fn proceed_auto_bind_when_bind_absent() {
    // Backward-compat: 50+ existing dispatch sites omit `bind`; must
    // continue to auto-bind exactly as pre-P0-C.
    let args = json!({"branch": "feat/x"});
    assert!(!dispatch_should_skip_auto_bind(&args));
}

/// #35896-11 ⑤ (Q2 vet): a kind=report carrying a correlation_id auto-settles the
/// SENDER's own delivering dispatch row EVEN WITHOUT `ack_inbox` — the gate was
/// removed so poll-reminder stops nagging a dispatch the reporter already
/// answered. End-to-end through `handle_report_result` with a minimal
/// `RuntimeContext` → `execute_send` delivers via the neutral service →
/// `is_ok_result` true → the settle block runs. Sender-scoped, so only the
/// reporter's row is touched (isolation unit-tested in
/// `inbox::tests::ack_by_correlation_isolates_across_agents_35896_11`).
#[test]
fn report_with_correlation_auto_settles_dispatch_row_without_ack_inbox_35896_11() {
    let home = std::env::temp_dir().join(format!(
        "agend-35896-report-autosettle-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    // Minimal fleet so the neutral service's target validation resolves `lead`.
    let reporter = "reporter-35896";
    std::fs::write(
        home.join("fleet.yaml"),
        format!("instances:\n  lead:\n    backend: claude\n  {reporter}:\n    backend: claude\n"),
    )
    .unwrap();

    // Reporter received a task dispatch (task_id=t-x) and drained it → delivering.
    crate::inbox::enqueue(
        &home,
        reporter,
        crate::inbox::InboxMessage {
            schema_version: 1,
            id: Some("m-dispatch".into()),
            from: "lead".into(),
            text: "[task] do the thing".into(),
            kind: Some("task".into()),
            task_id: Some("t-x".into()),
            timestamp: "2026-07-07T00:00:00Z".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        crate::inbox::drain(&home, reporter).len(),
        1,
        "dispatch drained → delivering"
    );

    // Report back WITHOUT ack_inbox — ⑤ must still settle the reporter's row.
    let sender = crate::identity::Sender::new(reporter);
    let rt = minimal_runtime();
    let result = super::handle_report_result(
        &home,
        &json!({
            "instance": "lead",
            "summary": "done",
            "correlation_id": "t-x"
        }),
        &sender,
        Some(&rt),
    );
    assert_eq!(
        result["inbox_settled"], 1,
        "report+correlation must auto-settle the sender's dispatch row without ack_inbox: {result}"
    );

    // The settled dispatch row is read → poll-reminder no longer sees it unread.
    assert!(
        crate::inbox::drain(&home, reporter).is_empty(),
        "the settled dispatch row must not re-surface as unread"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// task66: verdict-looking analysis text remains an ordinary report even when a
/// PR URL appears only in artifacts. No semantic SHA/PR gate may classify it as
/// a code review.
#[test]
fn analysis_report_with_pr_artifact_does_not_enter_review_gate_2760() {
    let home = std::env::temp_dir().join(format!(
        "agend-78445-3-artifacts-url-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let reporter = "reviewer-78445";
    std::fs::write(
        home.join("fleet.yaml"),
        format!("instances:\n  lead:\n    backend: claude\n  {reporter}:\n    backend: claude\n"),
    )
    .unwrap();

    let sender = crate::identity::Sender::new(reporter);
    let rt = minimal_runtime();
    let result = super::handle_report_result(
        &home,
        &json!({
            "instance": "lead",
            "summary": "VERIFIED — looks correct",
            "artifacts": "PR: https://github.com/nonexistent-org-xyz/nonexistent-repo/pull/1",
            "reviewed_head": "abc1234def5678",
            "report_purpose": "analysis_decision",
        }),
        &sender,
        Some(&rt),
    );
    assert!(
        result.get("error").is_none(),
        "analysis report must deliver without review inference: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// A bare `#N` plus verdict-looking text also remains ordinary analysis. The
/// purpose enum, not prose or reviewed_head, selects the review validator.
#[test]
fn analysis_report_with_bare_pr_number_is_not_semantic_review_2760() {
    let home = std::env::temp_dir().join(format!(
        "agend-78445-3-bare-num-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let reporter = "reviewer-78445b";
    std::fs::write(
        home.join("fleet.yaml"),
        format!("instances:\n  lead:\n    backend: claude\n  {reporter}:\n    backend: claude\n"),
    )
    .unwrap();

    let sender = crate::identity::Sender::new(reporter);
    let rt = minimal_runtime();
    let result = super::handle_report_result(
        &home,
        &json!({
            "instance": "lead",
            "summary": "VERIFIED — PR #2674 looks good",
            "artifacts": "ran: cargo test -> ok",
            "reviewed_head": "abc1234def5678",
            "report_purpose": "analysis_decision",
        }),
        &sender,
        Some(&rt),
    );
    assert!(
        result.get("error").is_none(),
        "analysis report must not be routed into the old SHA gate: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ─────────────────── t-…-17 C9: authenticated correlated ACK ───────────────────

use crate::daemon::assignment_authority as authority;

/// Seed one ACTIVE reviewer assignment for `target` on `branch` with `task_id`.
fn seed_assignment(home: &std::path::Path, branch: &str, target: &str, pr: u64, task_id: &str) {
    let rec = authority::ActiveAssignment::new_pending(
        "o/r",
        branch,
        target,
        pr,
        "lead",
        task_id,
        crate::daemon::pr_state::ReviewClass::Dual,
        crate::mcp::handlers::comms_gates::ReviewAuthor::External("octocat".into()),
        "Please review PR",
        None,
        None,
        "2026-07-13T00:00:00Z",
    );
    authority::persist(home, &rec).unwrap();
}

fn seed_home(tag: &str, reporter: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-c9-{}-{}-{}",
        tag,
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("fleet.yaml"),
        format!("instances:\n  lead:\n    backend: claude\n  {reporter}:\n    backend: claude\n"),
    )
    .unwrap();
    home
}

/// task66: an ordinary correlated report from the assignment target must not
/// write assignment evidence. Only the API-sink validated code-review receipt
/// can engage or satisfy an assignment.
#[test]
fn ordinary_correlated_report_does_not_ack_assignment_2760() {
    let reporter = "reviewer-c9a";
    let home = seed_home("t14", reporter);
    seed_assignment(&home, "feat/x", reporter, 42, "t-x");

    // Report FROM the target, correlated to the assignment's task_id.
    let sender = crate::identity::Sender::new(reporter);
    super::handle_report_result(
        &home,
        &json!({"instance": "lead", "summary": "done", "correlation_id": "t-x"}),
        &sender,
        None,
    );

    let rec = authority::get(&home, "o/r", "feat/x", reporter).expect("assignment present");
    assert!(
        rec.acked_at.is_none(),
        "ordinary report must not set acked_at"
    );
    let ps = crate::daemon::pr_state::new_for_branch(
        "o/r",
        "feat/x",
        "sha-1",
        crate::daemon::pr_state::ReviewClass::Dual,
    );
    assert_eq!(
        authority::classify_assignment(&rec, Some(&ps)),
        authority::AssignmentEvidence::Unengaged,
        "ordinary report is not assignment evidence"
    );
    std::fs::remove_dir_all(&home).ok();

    // A report from a DIFFERENT sender does NOT ack the target's record.
    let other = "not-the-reviewer-c9a";
    let home = seed_home("t14-neg", other);
    seed_assignment(&home, "feat/x", "reviewer-c9a", 42, "t-x");
    let sender = crate::identity::Sender::new(other);
    super::handle_report_result(
        &home,
        &json!({"instance": "lead", "summary": "done", "correlation_id": "t-x"}),
        &sender,
        None,
    );
    assert!(
        authority::get(&home, "o/r", "feat/x", "reviewer-c9a")
            .unwrap()
            .acked_at
            .is_none(),
        "a report from a non-target sender must NOT ack the target's assignment"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// C9 (T15 fail-closed): TWO active assignments with the SAME (target,task_id) on
/// DIFFERENT branches ⇒ an authenticated correlated report acks NEITHER.
#[test]
fn c9_ack_ambiguity_fails_closed_end_to_end_t15() {
    let reporter = "reviewer-c9b";
    let home = seed_home("t15", reporter);
    seed_assignment(&home, "feat/a", reporter, 42, "t-x");
    seed_assignment(&home, "feat/b", reporter, 43, "t-x");

    let sender = crate::identity::Sender::new(reporter);
    super::handle_report_result(
        &home,
        &json!({"instance": "lead", "summary": "done", "correlation_id": "t-x"}),
        &sender,
        None,
    );

    assert!(
        authority::get(&home, "o/r", "feat/a", reporter)
            .unwrap()
            .acked_at
            .is_none(),
        "ambiguous (target,task_id) ⇒ branch a NOT acked (fail closed)"
    );
    assert!(
        authority::get(&home, "o/r", "feat/b", reporter)
            .unwrap()
            .acked_at
            .is_none(),
        "ambiguous (target,task_id) ⇒ branch b NOT acked (fail closed)"
    );
    std::fs::remove_dir_all(&home).ok();
}
