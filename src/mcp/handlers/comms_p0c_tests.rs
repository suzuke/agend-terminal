//! Sprint 55 P0-C — `dispatch_should_skip_auto_bind` helper tests.
//!
//! Located in this sibling file (loaded via `#[path]` from comms.rs) to
//! keep src/mcp/handlers/comms.rs under the file_size_invariant 700 LOC
//! ceiling. Same module layout pattern as the
//! `instance_state::lifecycle` split.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::dispatch_should_skip_auto_bind;
use serde_json::json;

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

// #1024 (closes #1002 ROOT 2): the reviewed_head-forwarding regression is now a
// BEHAVIORAL test — `send_envelope::tests::reviewed_head_from_args_reaches_send_params_1024`
// (plus the fixed-gap fallback pin `to_inbox_message_carries_full_directive_set_fixed_gap_1024_1833`)
// — replacing this brittle source-text grep, which broke on the smells#2
// SendEnvelope refactor though the behavior was preserved (source-grep tests
// are themselves a flagged de2eb8 smell / Pattern A).

/// #35896-11 ⑤ (Q2 vet): a kind=report carrying a correlation_id auto-settles the
/// SENDER's own delivering dispatch row EVEN WITHOUT `ack_inbox` — the gate was
/// removed so poll-reminder stops nagging a dispatch the reporter already
/// answered. End-to-end through the real `handle_report_result`: `api::call(SEND)`
/// fails in-test (no daemon) → `fallback_deliver` returns a no-error result →
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
    // Minimal fleet so the report's in-test fallback delivery to `lead` resolves
    // (api::call has no daemon → fallback_deliver, which validates the target
    // against fleet.yaml; without this the send returns an error result and the
    // settle block is correctly skipped).
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
    let result = super::handle_report_result(
        &home,
        &json!({
            "instance": "lead",
            "summary": "done",
            "correlation_id": "t-x"
        }),
        &sender,
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

/// #t-78445-3: the SHA-staleness gate must scan `summary + artifacts` for the PR
/// URL (not `summary` alone). A reviewer whose verdict carries the URL in the
/// `artifacts` field was FALSE-REJECTED with "no GitHub PR URL" → verdict lost to
/// fallback (root cause of reviewer4's #2674/#2611 fallbacks). RED on pre-fix code
/// (gate scanned summary only), GREEN after the shared summary+artifacts scan.
#[test]
fn sha_gate_scans_artifacts_for_pr_url_78445_3() {
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
    // Verdict prefix in `summary` (no URL); the PR URL lives ONLY in `artifacts`.
    let result = super::handle_report_result(
        &home,
        &json!({
            "instance": "lead",
            "summary": "VERIFIED — looks correct",
            "artifacts": "PR: https://github.com/nonexistent-org-xyz/nonexistent-repo/pull/1",
            "reviewed_head": "abc1234def5678",
        }),
        &sender,
    );
    // The URL IS present in the envelope (artifacts) → it must NOT be rejected as
    // missing. (Post-fix the gate then attempts a real fetch which fails for the
    // fake repo — a DIFFERENT error; the point is the "no URL" false-reject is gone.)
    let err = result["error"].as_str().unwrap_or("");
    assert!(
        !err.contains("no GitHub PR URL"),
        "PR URL in artifacts must NOT be false-rejected as missing: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #t-78445-3 companion: a bare `#N` reference with NO full URL in EITHER field is
/// still rejected (the daemon must not guess the repo — anti-forgery), but the
/// reject message must be one-shot actionable so a degraded model can fix + resend
/// instead of looping (the fugu re-enter degradation this bug triggered). No fetch
/// runs (deterministic).
#[test]
fn sha_gate_bare_pr_number_still_rejected_actionable_78445_3() {
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
    let result = super::handle_report_result(
        &home,
        &json!({
            "instance": "lead",
            "summary": "VERIFIED — PR #2674 looks good",
            "artifacts": "ran: cargo test -> ok",
            "reviewed_head": "abc1234def5678",
        }),
        &sender,
    );
    let err = result["error"].as_str().unwrap_or("");
    assert!(
        err.contains("no GitHub PR URL"),
        "a bare #N with no full URL must still reject: {result}"
    );
    assert!(
        err.contains("PR: https://github.com/<owner>/<repo>/pull/<N>"),
        "reject must name the exact FULL-URL line to add: {err}"
    );
    std::fs::remove_dir_all(&home).ok();
}
