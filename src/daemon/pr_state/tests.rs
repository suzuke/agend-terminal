use super::*;

/// #2059: the dir-scan predicate accepts per-branch `*.json` PrState files
/// and rejects the dotfile companions (`.emitted-terminal.json` ledger,
/// `*.lock`) that share the pr-state dir — parsing those as PrState spammed a
/// `missing field 'repo'` WARN every ~10 s.
#[test]
fn is_pr_state_file_skips_dotfiles_2059() {
    assert!(is_pr_state_file(Path::new(
        "/h/pr-state/suzuke_agend-terminal__feat_x.json"
    )));
    assert!(
        !is_pr_state_file(Path::new("/h/pr-state/.emitted-terminal.json")),
        "the terminal-latch ledger is a dotfile .json — must be skipped"
    );
    assert!(!is_pr_state_file(Path::new(
        "/h/pr-state/suzuke_agend-terminal__feat_x.lock"
    )));
    assert!(!is_pr_state_file(Path::new("/h/pr-state/.something.lock")));
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// #2749: stamp a VALID, FRESH deterministic-ancestry tuple on `s` — three
/// heads agree at the current `head_sha`, checked base == observed base, no
/// error, both timestamps `now`, behind_by == 0 — so the read-only freshness
/// gate returns `Fresh` and `[pr-ready-for-merge]` may emit. This is the state a
/// genuinely up-to-date PR has once the off-tick populator has run; tests about
/// routing / dedup / threshold (NOT ancestry) call it to satisfy the new gate
/// precondition without exercising the populator.
fn stamp_fresh_ancestry(s: &mut PrState) {
    let head = s.head_sha.clone();
    let ts = now();
    s.observed_head_sha = Some(head.clone());
    s.observed_base_sha = Some("main-base".to_string());
    s.observed_at = Some(ts.clone());
    s.observed_error = false;
    s.freshness_checked_head_sha = Some(head);
    s.freshness_checked_base_sha = Some("main-base".to_string());
    s.freshness_checked_at = Some(ts);
    s.freshness_behind_by = Some(0);
    s.freshness_error = false;
}

fn new_state(head: &str, class: ReviewClass) -> PrState {
    PrState {
        repo: "owner/repo".to_string(),
        pr_number: 100,
        branch: "feat/test".to_string(),
        head_sha: head.to_string(),
        pr_author: "dev".to_string(),
        subscribers: vec!["dev".to_string()],
        ci_state: CiState::Pending,
        verdict_state: VerdictState::None,
        validated_review_receipts: Vec::new(),
        merge_state: MergeState::NotReady,
        draft_state: DraftState::Ready,
        review_class: class,
        ready_emitted_for_sha: None,
        diagnostic_emitted_for_sha: None,
        auto_armed: false,
        auto_armed_for_sha: None,
        auto_armed_at: None,
        last_gh_poll_at: None,
        gh_poll_failures: 0,
        last_gh_state: None,
        closed_unmerged_pending: false,
        freshness_checked_head_sha: None,
        freshness_checked_base_sha: None,
        freshness_checked_at: None,
        freshness_behind_by: None,
        freshness_error: false,
        freshness_retry_after: None,
        observed_head_sha: None,
        observed_base_sha: None,
        observed_at: None,
        observed_error: false,
        reserved_assignments: Vec::new(),
        authority_unknown: false,
        created_at: now(),
        updated_at: now(),
    }
}

/// Seed the server-validated receipt view used by the production merge gate.
/// Tests that only exercise the pure PR-state reducer do not need to construct
/// an assignment-authority store, but they must not accidentally regain
/// authority through the legacy name+SHA `VerdictState` projection.
fn observe_typed_verdict(
    state: &mut PrState,
    reviewer: &str,
    reviewed_head: &str,
    verdict: crate::review_receipt::ReviewVerdict,
) {
    let existing = state
        .validated_review_receipts
        .iter()
        .find(|receipt| receipt.reviewer_name == reviewer)
        .cloned();
    let slot = existing
        .as_ref()
        .map(|receipt| receipt.slot)
        .unwrap_or_else(|| {
            if state
                .validated_review_receipts
                .iter()
                .any(|receipt| receipt.slot == crate::review_receipt::ReviewSlot::Primary)
            {
                crate::review_receipt::ReviewSlot::Secondary
            } else {
                crate::review_receipt::ReviewSlot::Primary
            }
        });
    let reviewer_instance_id = existing
        .as_ref()
        .map(|receipt| receipt.reviewer_instance_id)
        .unwrap_or_else(crate::types::InstanceId::new);
    let assignment_id = existing
        .as_ref()
        .map(|receipt| receipt.assignment_id)
        .unwrap_or_else(uuid::Uuid::new_v4);
    let source_id = format!("test-source-{}", uuid::Uuid::new_v4());
    let receipt = crate::review_receipt::ReviewReceiptSummary {
        receipt_id: format!("review-receipt:{source_id}"),
        source_id,
        evidence_digest: "a".repeat(64),
        assignment_id,
        reviewer_instance_id,
        reviewer_name: reviewer.to_string(),
        repo: state.repo.clone(),
        pr_number: state.pr_number,
        branch: state.branch.clone(),
        task_id: "t-test-review".to_string(),
        reviewed_head: reviewed_head.to_string(),
        review_class: state.review_class,
        slot,
        verdict,
    };
    super::apply_receipt_to_state(state, receipt);
}

fn observe_typed_verified(state: &mut PrState, reviewer: &str, reviewed_head: &str) {
    observe_typed_verdict(
        state,
        reviewer,
        reviewed_head,
        crate::review_receipt::ReviewVerdict::Verified,
    );
}

fn observe_assignment_verdict(
    state: &mut PrState,
    assignment: &crate::daemon::assignment_authority::ActiveAssignment,
    verdict: crate::review_receipt::ReviewVerdict,
) {
    let source_id = format!("test-source-{}", uuid::Uuid::new_v4());
    let receipt = crate::review_receipt::ReviewReceiptSummary {
        receipt_id: format!("review-receipt:{source_id}"),
        source_id,
        evidence_digest: "b".repeat(64),
        assignment_id: assignment.assignment_id,
        reviewer_instance_id: assignment.target_instance_id.unwrap(),
        reviewer_name: assignment.target.clone(),
        repo: assignment.repo.clone(),
        pr_number: assignment.pr_number,
        branch: assignment.branch.clone(),
        task_id: assignment.task_id.clone(),
        reviewed_head: assignment.reviewed_head.clone().unwrap(),
        review_class: assignment.review_class,
        slot: assignment.review_slot.unwrap(),
        verdict,
    };
    super::apply_receipt_to_state(state, receipt);
}

/// task66/A1: receipt validation can race the PR-state file becoming absent and
/// therefore enter the typed buffer. Replay must re-check the generation; a
/// revoke between buffer and state creation makes the buffered receipt inert.
#[test]
fn typed_buffer_replay_revalidates_active_assignment_2760() {
    let home = std::env::temp_dir().join(format!(
        "agend-prstate-buffer-revoke-2760-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let head = "c".repeat(40);
    let reviewer_id = crate::types::InstanceId::new();
    let assignment = crate::daemon::assignment_authority::ActiveAssignment::new_pending_typed(
        "owner/repo",
        "fix/buffered",
        "reviewer",
        reviewer_id,
        42,
        &head,
        crate::review_receipt::ReviewSlot::Primary,
        "lead",
        "t-buffered-review",
        ReviewClass::Single,
        crate::mcp::handlers::comms_gates::ReviewAuthor::External("octocat".into()),
        "review",
        None,
        None,
        "2026-07-14T00:00:00Z",
    );
    crate::daemon::assignment_authority::persist(&home, &assignment).unwrap();
    let receipt = crate::review_receipt::ValidatedCodeReviewReceipt::for_test(
        crate::review_receipt::ReviewReceiptSummary {
            receipt_id: "review-receipt:m-buffered".into(),
            source_id: "m-buffered".into(),
            evidence_digest: "a".repeat(64),
            assignment_id: assignment.assignment_id,
            reviewer_instance_id: reviewer_id,
            reviewer_name: "reviewer".into(),
            repo: "owner/repo".into(),
            pr_number: 42,
            branch: "fix/buffered".into(),
            task_id: "t-buffered-review".into(),
            reviewed_head: head.clone(),
            review_class: ReviewClass::Single,
            slot: crate::review_receipt::ReviewSlot::Primary,
            verdict: crate::review_receipt::ReviewVerdict::Verified,
        },
    );
    assert!(
        super::record_validated_receipt(&home, &receipt),
        "validated receipt is buffered when its exact PR state is momentarily absent"
    );
    crate::daemon::assignment_authority::revoke(
        &home,
        "owner/repo",
        "fix/buffered",
        "reviewer",
        "2026-07-14T00:00:01Z",
    )
    .unwrap();

    super::with_pr_state_or_create(
        &home,
        "owner/repo",
        "fix/buffered",
        || {
            let mut state =
                super::new_for_branch("owner/repo", "fix/buffered", &head, ReviewClass::Single);
            state.pr_number = 42;
            state
        },
        |_| {},
    )
    .unwrap();
    super::record_ci_result(
        &home,
        "owner/repo",
        "fix/buffered",
        &head,
        super::CiConclusion::Green,
        vec!["lead".into()],
        ReviewClass::Single,
    );
    let state = super::load(&home, "owner/repo", "fix/buffered").unwrap();
    assert!(state.validated_review_receipts.is_empty());
    assert!(!super::is_merge_ready(&state));
    assert!(
        super::verdict_buffer::drain_validated_for_subject(
            &home,
            "owner/repo",
            "fix/buffered",
            42,
            &head,
        )
        .is_empty(),
        "revoked buffered receipt is consumed/quarantined, never replayed"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// task66/A1: a destructive typed-buffer drain is forbidden unless the
/// assignment lock is held. A transient lock failure leaves the receipt parked;
/// the next successful observation revalidates it, removes the reservation, and
/// applies it exactly once.
#[test]
fn typed_buffer_lock_failure_retries_without_losing_receipt_2760() {
    let home = std::env::temp_dir().join(format!(
        "agend-prstate-buffer-lock-2760-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let head = "d".repeat(40);
    let reviewer_id = crate::types::InstanceId::new();
    let assignment = crate::daemon::assignment_authority::ActiveAssignment::new_pending_typed(
        "owner/repo",
        "fix/buffer-lock",
        "reviewer",
        reviewer_id,
        43,
        &head,
        crate::review_receipt::ReviewSlot::Primary,
        "lead",
        "t-buffer-lock-review",
        ReviewClass::Single,
        crate::mcp::handlers::comms_gates::ReviewAuthor::External("octocat".into()),
        "review",
        None,
        None,
        "2026-07-14T00:00:00Z",
    );
    crate::daemon::assignment_authority::persist(&home, &assignment).unwrap();
    let receipt = crate::review_receipt::ReviewReceiptSummary {
        receipt_id: "review-receipt:m-buffer-lock".into(),
        source_id: "m-buffer-lock".into(),
        evidence_digest: "a".repeat(64),
        assignment_id: assignment.assignment_id,
        reviewer_instance_id: reviewer_id,
        reviewer_name: "reviewer".into(),
        repo: "owner/repo".into(),
        pr_number: 43,
        branch: "fix/buffer-lock".into(),
        task_id: "t-buffer-lock-review".into(),
        reviewed_head: head.clone(),
        review_class: ReviewClass::Single,
        slot: crate::review_receipt::ReviewSlot::Primary,
        verdict: crate::review_receipt::ReviewVerdict::Verified,
    };
    assert!(super::verdict_buffer::buffer_validated(&home, &receipt));
    let mut state =
        super::new_for_branch("owner/repo", "fix/buffer-lock", &head, ReviewClass::Single);
    state.pr_number = 43;
    super::save(&home, &state).unwrap();

    let lock_path = crate::daemon::assignment_authority::branch_lock_path_for_test(
        &home,
        "owner/repo",
        "fix/buffer-lock",
    );
    std::fs::remove_file(&lock_path).ok();
    std::fs::create_dir_all(&lock_path).unwrap();
    super::record_ci_result(
        &home,
        "owner/repo",
        "fix/buffer-lock",
        &head,
        super::CiConclusion::Green,
        vec!["lead".into()],
        ReviewClass::Single,
    );
    let after_failed_lock = super::load(&home, "owner/repo", "fix/buffer-lock").unwrap();
    assert!(after_failed_lock.validated_review_receipts.is_empty());
    assert!(after_failed_lock.authority_unknown);

    std::fs::remove_dir(&lock_path).unwrap();
    super::record_ci_result(
        &home,
        "owner/repo",
        "fix/buffer-lock",
        &head,
        super::CiConclusion::Green,
        vec!["lead".into()],
        ReviewClass::Single,
    );
    let replayed = super::load(&home, "owner/repo", "fix/buffer-lock").unwrap();
    assert_eq!(replayed.validated_review_receipts, vec![receipt]);
    assert!(replayed.reserved_assignments.is_empty());
    assert!(!replayed.authority_unknown);
    assert!(super::is_merge_ready(&replayed));
    assert!(
        super::verdict_buffer::drain_validated_for_subject(
            &home,
            "owner/repo",
            "fix/buffer-lock",
            43,
            &head,
        )
        .is_empty(),
        "successful retry consumed the buffered receipt exactly once"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// task66/root-review RED: selecting a typed buffered receipt is not a commit.
/// If the subsequent atomic PR-state save fails, the durable candidate must
/// remain parked so a later observation can revalidate and apply it exactly
/// once. This exercises the post-selection failure window that the assignment-
/// lock test above does not reach.
#[test]
fn typed_buffer_save_failure_after_selection_remains_retryable_2760() {
    let home = std::env::temp_dir().join(format!(
        "agend-prstate-buffer-save-2760-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let head = "e".repeat(40);
    let reviewer_id = crate::types::InstanceId::new();
    let assignment = crate::daemon::assignment_authority::ActiveAssignment::new_pending_typed(
        "owner/repo",
        "fix/buffer-save",
        "reviewer",
        reviewer_id,
        44,
        &head,
        crate::review_receipt::ReviewSlot::Primary,
        "lead",
        "t-buffer-save-review",
        ReviewClass::Single,
        crate::mcp::handlers::comms_gates::ReviewAuthor::External("octocat".into()),
        "review",
        None,
        None,
        "2026-07-14T00:00:00Z",
    );
    crate::daemon::assignment_authority::persist(&home, &assignment).unwrap();
    let receipt = crate::review_receipt::ReviewReceiptSummary {
        receipt_id: "review-receipt:m-buffer-save".into(),
        source_id: "m-buffer-save".into(),
        evidence_digest: "a".repeat(64),
        assignment_id: assignment.assignment_id,
        reviewer_instance_id: reviewer_id,
        reviewer_name: "reviewer".into(),
        repo: "owner/repo".into(),
        pr_number: 44,
        branch: "fix/buffer-save".into(),
        task_id: "t-buffer-save-review".into(),
        reviewed_head: head.clone(),
        review_class: ReviewClass::Single,
        slot: crate::review_receipt::ReviewSlot::Primary,
        verdict: crate::review_receipt::ReviewVerdict::Verified,
    };
    assert!(super::verdict_buffer::buffer_validated(&home, &receipt));
    let mut state =
        super::new_for_branch("owner/repo", "fix/buffer-save", &head, ReviewClass::Single);
    state.pr_number = 44;
    super::save(&home, &state).unwrap();
    let state_path =
        super::pr_state_dir(&home).join(super::pr_state_filename("owner/repo", "fix/buffer-save"));
    crate::store::fail_next_atomic_write_for_test(&state_path);

    super::record_ci_result(
        &home,
        "owner/repo",
        "fix/buffer-save",
        &head,
        super::CiConclusion::Green,
        vec!["lead".into()],
        ReviewClass::Single,
    );
    let after_failed_save = super::load(&home, "owner/repo", "fix/buffer-save").unwrap();
    assert!(after_failed_save.validated_review_receipts.is_empty());
    assert!(
        super::verdict_buffer::has_validated_subject_hint(
            &home,
            "owner/repo",
            "fix/buffer-save",
            &head,
        ),
        "failed PR-state persistence must leave the selected receipt durable"
    );

    super::record_ci_result(
        &home,
        "owner/repo",
        "fix/buffer-save",
        &head,
        super::CiConclusion::Green,
        vec!["lead".into()],
        ReviewClass::Single,
    );
    let replayed = super::load(&home, "owner/repo", "fix/buffer-save").unwrap();
    assert_eq!(replayed.validated_review_receipts, vec![receipt]);
    assert!(super::is_merge_ready(&replayed));
    assert!(
        !super::verdict_buffer::has_validated_subject_hint(
            &home,
            "owner/repo",
            "fix/buffer-save",
            &head,
        ),
        "successful persistence commits the exact buffered candidate"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2749 (Fable mandatory evidence): a PrState JSON written BEFORE #2749 added the
/// freshness/observed fields — and a nested last_gh_state GhPrMetadata written
/// before the atomic-OID fields — MUST load with every new field defaulted, never
/// a `missing field` deserialize error. `#[serde(default)]` on each new field is
/// what keeps pre-existing on-disk state files loadable across the upgrade.
#[test]
fn pre_2749_state_json_loads_with_defaulted_freshness_and_oid_fields() {
    // Populate a state (incl. a last_gh_state), serialize, then STRIP the
    // #2749-added keys to simulate a file written before they existed.
    let mut s = new_state("sha-A", ReviewClass::Single);
    s.observed_head_sha = Some("h".into());
    s.observed_error = true;
    s.freshness_behind_by = Some(3);
    s.last_gh_state = Some(GhPrMetadata {
        number: 1,
        author_login: "dev".into(),
        head_ref: "feat/test".into(),
        is_cross_repository: false,
        is_draft: false,
        state: GhPrState::Open,
        merged_at: None,
        head_ref_oid: Some("oid".into()),
        base_ref_oid: Some("oid2".into()),
    });
    let mut v = serde_json::to_value(&s).unwrap();
    let obj = v.as_object_mut().unwrap();
    for k in [
        "observed_head_sha",
        "observed_base_sha",
        "observed_at",
        "observed_error",
        "freshness_checked_head_sha",
        "freshness_checked_base_sha",
        "freshness_checked_at",
        "freshness_behind_by",
        "freshness_error",
        "freshness_retry_after",
    ] {
        obj.remove(k);
    }
    if let Some(gh) = obj.get_mut("last_gh_state").and_then(|g| g.as_object_mut()) {
        gh.remove("head_ref_oid");
        gh.remove("base_ref_oid");
    }

    let loaded: PrState = serde_json::from_value(v).expect("pre-#2749 json must load, not error");
    assert!(
        loaded.observed_head_sha.is_none(),
        "observed_head_sha ⇒ None"
    );
    assert!(loaded.observed_base_sha.is_none());
    assert!(loaded.observed_at.is_none());
    assert!(!loaded.observed_error, "observed_error ⇒ false");
    assert!(loaded.freshness_checked_head_sha.is_none());
    assert!(loaded.freshness_checked_base_sha.is_none());
    assert!(loaded.freshness_checked_at.is_none());
    assert!(
        loaded.freshness_behind_by.is_none(),
        "freshness_behind_by ⇒ None"
    );
    assert!(!loaded.freshness_error);
    assert!(
        loaded.freshness_retry_after.is_none(),
        "freshness_retry_after ⇒ None"
    );
    let gh = loaded.last_gh_state.expect("last_gh_state survives");
    assert!(
        gh.head_ref_oid.is_none(),
        "GhPrMetadata head_ref_oid ⇒ None"
    );
    assert!(gh.base_ref_oid.is_none());
}

/// T1: CI green at head_sha + Verified at same head_sha → MergeReady.
#[test]
fn t1_ci_then_verdict_at_same_sha_yields_merge_ready() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-A",
            conclusion: CiConclusion::Green,
            observed_at: now(),
        },
    );
    observe_typed_verified(&mut s, "rev-1", "sha-A");
    assert_eq!(s.merge_state, MergeState::MergeReady);
}

/// T2 / Reviewer must-have T_sha_mismatch: CI at sha-A, verdict
/// at sha-B (b != a) → NotReady. §4.2 invariant.
#[test]
fn t2_sha_mismatch_refuses_merge_ready() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-A",
            conclusion: CiConclusion::Green,
            observed_at: now(),
        },
    );
    apply(
        &mut s,
        Event::VerdictObserved {
            reviewer: "rev-1",
            reviewed_head: "sha-B-OLD",
            kind: VerdictKind::Verified,
        },
    );
    assert_eq!(s.merge_state, MergeState::NotReady);
    assert!(!is_merge_ready(&s));
}

/// T3 / Reviewer must-have T_force_push: Verified at sha-A; head
/// advances to sha-B; verdict invalidated; back to NotReady.
#[test]
fn t3_head_advance_invalidates_verdict() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-A",
            conclusion: CiConclusion::Green,
            observed_at: now(),
        },
    );
    observe_typed_verified(&mut s, "rev-1", "sha-A");
    assert_eq!(s.merge_state, MergeState::MergeReady);
    // Head advances (force-push).
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-B",
            conclusion: CiConclusion::Pending,
            observed_at: now(),
        },
    );
    assert_eq!(s.head_sha, "sha-B");
    assert_eq!(s.merge_state, MergeState::NotReady);
    // Auto-armed (if any) cleared.
    assert!(!s.auto_armed);
    assert_eq!(s.ready_emitted_for_sha, None);
    // Verdict cleared (was for sha-A).
    assert_eq!(s.verdict_state, VerdictState::Pending);
}

/// T4: idempotent debounce — once `ready_emitted_for_sha` matches
/// head_sha, the reducer doesn't mutate ready_emitted_for_sha
/// (that field is updated by the emitter, not the reducer). The
/// reducer can still recompute MergeReady on every event; the
/// emitter is responsible for one-fire-per-sha.
#[test]
fn t4_reducer_recomputes_merge_ready_every_event() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-A",
            conclusion: CiConclusion::Green,
            observed_at: now(),
        },
    );
    observe_typed_verified(&mut s, "rev-1", "sha-A");
    assert_eq!(s.merge_state, MergeState::MergeReady);
    // No-op event (re-record same CI). MergeReady should stay.
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-A",
            conclusion: CiConclusion::Green,
            observed_at: now(),
        },
    );
    assert_eq!(s.merge_state, MergeState::MergeReady);
}

/// T5 / Reviewer must-have T_dual_review: Dual review_class
/// requires 2 VERIFIED at the same head_sha.
#[test]
fn t5_dual_review_requires_two_verified() {
    let mut s = new_state("sha-A", ReviewClass::Dual);
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-A",
            conclusion: CiConclusion::Green,
            observed_at: now(),
        },
    );
    observe_typed_verified(&mut s, "rev-1", "sha-A");
    // Only 1 of 2 — not ready.
    assert_eq!(s.merge_state, MergeState::NotReady);
    observe_typed_verified(&mut s, "rev-2", "sha-A");
    assert_eq!(s.merge_state, MergeState::MergeReady);
}

/// T6 / Reviewer must-have T_reject: Rejected verdict at any sha
/// → NotReady. (No MergeReady possible from Rejected variant.)
#[test]
fn t6_rejected_keeps_not_ready() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-A",
            conclusion: CiConclusion::Green,
            observed_at: now(),
        },
    );
    apply(
        &mut s,
        Event::VerdictObserved {
            reviewer: "rev-1",
            reviewed_head: "sha-A",
            kind: VerdictKind::Rejected {
                reason: Some("LGTM after addressing X"),
            },
        },
    );
    assert_eq!(s.merge_state, MergeState::NotReady);
    assert!(matches!(s.verdict_state, VerdictState::Rejected { .. }));
}

/// T7 / Reviewer must-have T_unverified: Unverified verdict → NotReady.
#[test]
fn t7_unverified_keeps_not_ready() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-A",
            conclusion: CiConclusion::Green,
            observed_at: now(),
        },
    );
    apply(
        &mut s,
        Event::VerdictObserved {
            reviewer: "rev-1",
            reviewed_head: "sha-A",
            kind: VerdictKind::Unverified,
        },
    );
    assert_eq!(s.merge_state, MergeState::NotReady);
    assert!(matches!(s.verdict_state, VerdictState::Unverified { .. }));
}

/// T8 / dev-2 T_draft: Draft state refuses MergeReady regardless
/// of CI + verdict.
#[test]
fn t8_draft_refuses_merge_ready() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    apply(&mut s, Event::DraftTransition { is_draft: true });
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-A",
            conclusion: CiConclusion::Green,
            observed_at: now(),
        },
    );
    observe_typed_verified(&mut s, "rev-1", "sha-A");
    assert_eq!(s.merge_state, MergeState::NotReady);
    // Draft → Ready transition unblocks.
    apply(&mut s, Event::DraftTransition { is_draft: false });
    assert_eq!(s.merge_state, MergeState::MergeReady);
}

/// T9 / dev-2 T_invalidate: MergeReady → head_sha advance →
/// MergeReady cleared + auto_armed cleared.
#[test]
fn t9_post_merge_ready_force_push_invalidates() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-A",
            conclusion: CiConclusion::Green,
            observed_at: now(),
        },
    );
    observe_typed_verified(&mut s, "rev-1", "sha-A");
    assert_eq!(s.merge_state, MergeState::MergeReady);
    // Simulate implementer armed --auto.
    s.auto_armed = true;
    s.auto_armed_for_sha = Some("sha-A".to_string());
    s.auto_armed_at = Some(now());
    // Force-push.
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-B",
            conclusion: CiConclusion::Pending,
            observed_at: now(),
        },
    );
    assert!(!s.auto_armed);
    assert_eq!(s.auto_armed_for_sha, None);
    assert_eq!(s.merge_state, MergeState::NotReady);
}

/// T10 / dev-2 T_closed_unmerged: ClosedUnmerged is sticky and
/// does not get downgraded by subsequent CI/verdict events.
#[test]
fn t10_closed_unmerged_is_sticky() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    apply(&mut s, Event::ClosedUnmergedObserved { closed_at: now() });
    assert!(matches!(s.merge_state, MergeState::ClosedUnmerged { .. }));
    // Subsequent CI/verdict noise must not flip back to MergeReady.
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-A",
            conclusion: CiConclusion::Green,
            observed_at: now(),
        },
    );
    observe_typed_verified(&mut s, "rev-1", "sha-A");
    assert!(matches!(s.merge_state, MergeState::ClosedUnmerged { .. }));
}

/// T11: Merged is sticky.
#[test]
fn t11_merged_is_sticky() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    apply(
        &mut s,
        Event::MergedObserved {
            merge_commit: "merge-sha",
            merged_at: now(),
        },
    );
    assert!(matches!(s.merge_state, MergeState::Merged { .. }));
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-B",
            conclusion: CiConclusion::Pending,
            observed_at: now(),
        },
    );
    assert!(matches!(s.merge_state, MergeState::Merged { .. }));
}

/// T12: CI Failed → NotReady. Subsequent VERIFIED at same sha
/// is honored but merge_state stays NotReady (ci is failed).
#[test]
fn t12_ci_failed_blocks_merge_ready() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-A",
            conclusion: CiConclusion::Failed {
                conclusion: "failure",
            },
            observed_at: now(),
        },
    );
    apply(
        &mut s,
        Event::VerdictObserved {
            reviewer: "rev-1",
            reviewed_head: "sha-A",
            kind: VerdictKind::Verified,
        },
    );
    assert_eq!(s.merge_state, MergeState::NotReady);
}

/// T13: same reviewer reporting Verified twice doesn't double-count
/// (dual-review requires DISTINCT reviewers).
#[test]
fn t13_same_reviewer_twice_counts_as_one() {
    let mut s = new_state("sha-A", ReviewClass::Dual);
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-A",
            conclusion: CiConclusion::Green,
            observed_at: now(),
        },
    );
    apply(
        &mut s,
        Event::VerdictObserved {
            reviewer: "rev-1",
            reviewed_head: "sha-A",
            kind: VerdictKind::Verified,
        },
    );
    // Same reviewer re-reports — must NOT bump to 2.
    apply(
        &mut s,
        Event::VerdictObserved {
            reviewer: "rev-1",
            reviewed_head: "sha-A",
            kind: VerdictKind::Verified,
        },
    );
    assert_eq!(s.merge_state, MergeState::NotReady);
    if let VerdictState::Verified { reviewers } = &s.verdict_state {
        assert_eq!(reviewers.len(), 1, "dedup by reviewer name");
    } else {
        panic!("expected Verified state, got {:?}", s.verdict_state);
    }
}

/// T14: storage round-trip — serialize, deserialize, structural eq.
#[test]
fn t14_storage_roundtrip() {
    let dir = std::env::temp_dir().join(format!("agend-972-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s = new_state("sha-A", ReviewClass::Single);
    save(&dir, &s).unwrap();
    let loaded = load(&dir, &s.repo, &s.branch).expect("reload");
    assert_eq!(loaded, s);
    // Remove leaves no file.
    remove(&dir, &s.repo, &s.branch).unwrap();
    assert!(load(&dir, &s.repo, &s.branch).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// T15: resolve_author chain — explicit pr_author wins, then
/// subscribers[0], then "fixup-lead" fallback.
#[test]
fn t15_resolve_author_chain() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    // Default state has pr_author="dev" + subscribers=[dev]
    assert_eq!(resolve_author(&s), "dev");
    s.pr_author = String::new();
    assert_eq!(resolve_author(&s), "dev"); // falls to subscribers[0]
    s.subscribers.clear();
    assert_eq!(resolve_author(&s), "fixup-lead"); // last-resort fallback
}

/// T16: format_ready_body uses pr_number when known, branch otherwise.
#[test]
fn t16_format_ready_body_with_and_without_pr_number() {
    let mut s = new_state("sha-A0001234567890", ReviewClass::Single);
    s.verdict_state = VerdictState::Verified {
        reviewers: vec![("rev-1".to_string(), "sha-A0001234567890".to_string())],
    };
    s.pr_number = 0;
    let body = format_ready_body(&s);
    assert!(body.contains("owner/repo@feat/test"), "branch form: {body}");
    s.pr_number = 970;
    let body = format_ready_body(&s);
    assert!(body.contains("owner/repo#970"), "pr-number form: {body}");
    assert!(body.contains("sha-A000"), "sha short: {body}");
    assert!(body.contains("rev-1"), "reviewers: {body}");
}

/// T_integration: scan_and_emit fires `[pr-ready-for-merge]` to
/// author's inbox once per MergeReady transition. Subsequent
/// scans (same head_sha) do NOT re-emit (debounce). Hits
/// production scanner + inbox enqueue path end-to-end (no
/// network, no ci_watch — synthetic PrState on disk).
#[test]
fn t18_scan_and_emit_fires_once_per_sha() {
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    let dir = std::env::temp_dir().join(format!("agend-972-scan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Inbox needs the inbox dir to exist.
    std::fs::create_dir_all(dir.join("inbox")).ok();
    // #2059-#3: ready events route to the team orchestrator (merge
    // authority). "dev" (the pr_author) is a team member; "lead-w" merges.
    write_team_fleet(&dir, "lead-w", &["dev"]);

    // Build a MergeReady state on disk.
    let mut s = new_state("sha-A", ReviewClass::Single);
    s.ci_state = CiState::Green {
        sha: "sha-A".to_string(),
        observed_at: now(),
    };
    observe_typed_verified(&mut s, "rev-1", "sha-A");
    assert_eq!(s.merge_state, MergeState::MergeReady);
    s.pr_author = "dev".to_string();
    // #2749: this test pins once-per-sha dedup, not ancestry — stamp a fresh
    // tuple so the read-only freshness gate admits the emission.
    stamp_fresh_ancestry(&mut s);
    save(&dir, &s).unwrap();

    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    // #2749 3a: a SUCCESSFUL gh poll (open PR, no OIDs) so apply_gh_observations
    // leaves the stamped observed_* intact. CliGhPoller errors in tests, and the
    // #2749 failure arm now sets observed_error — which would close the freshness
    // gate. One response per scan (both open).
    let poller = MockGhPoller::new(vec![
        Ok(vec![gh_meta_open(100, "feat/test", "dev")]),
        Ok(vec![gh_meta_open(100, "feat/test", "dev")]),
    ]);

    // First scan: emit.
    scan_and_emit_with(&dir, &registry, &poller);
    let inbox_msgs = crate::inbox::drain(&dir, "lead-w");
    assert_eq!(inbox_msgs.len(), 1, "expected one [pr-ready-for-merge]");
    assert_eq!(inbox_msgs[0].kind.as_deref(), Some("pr-ready-for-merge"));
    // Default fixture has pr_number=100 — body uses `owner/repo#100` form.
    assert!(
        inbox_msgs[0].text.contains("owner/repo#100"),
        "body shape: {}",
        inbox_msgs[0].text
    );
    assert!(
        inbox_msgs[0].text.contains("rev-1"),
        "reviewer surfacing: {}",
        inbox_msgs[0].text
    );
    // #946 correlation_id grep target.
    assert_eq!(
        inbox_msgs[0].correlation_id.as_deref(),
        Some("owner/repo@feat/test")
    );
    assert_eq!(inbox_msgs[0].reviewed_head.as_deref(), Some("sha-A"));

    // Second scan: must NOT re-emit (debounce per ready_emitted_for_sha).
    scan_and_emit_with(&dir, &registry, &poller);
    let inbox_msgs = crate::inbox::drain(&dir, "lead-w");
    assert!(
        inbox_msgs.is_empty(),
        "second scan must not re-emit; got {} message(s)",
        inbox_msgs.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #2745: scan_and_emit surfaces a `[review-class-unresolved]` re-arm
/// diagnostic (NOT a premature pr-ready) when a CI-green ∧ VERIFIED state
/// carries an `Unresolved` review_class — the legacy-None inventory. Debounced
/// once per head_sha via `diagnostic_emitted_for_sha`.
#[test]
fn review_class_unresolved_emits_rearm_diagnostic_not_ready_2745() {
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    let dir = std::env::temp_dir().join(format!("agend-2745-diag-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(dir.join("inbox")).ok();
    write_team_fleet(&dir, "lead-w", &["dev"]);

    // CI-green + VERIFIED at head, but review_class is Unresolved (a legacy
    // `None` watch): would-be-ready-but-for-the-unresolved-class.
    let mut s = new_state("sha-A", ReviewClass::Unresolved);
    s.ci_state = CiState::Green {
        sha: "sha-A".to_string(),
        observed_at: now(),
    };
    s.verdict_state = VerdictState::Verified {
        reviewers: vec![("rev-1".to_string(), "sha-A".to_string())],
    };
    s.merge_state = MergeState::NotReady;
    s.pr_author = "dev".to_string();
    save(&dir, &s).unwrap();

    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));

    scan_and_emit_with(
        &dir,
        &registry,
        &crate::daemon::pr_state::gh_poll::CliGhPoller,
    );
    let msgs = crate::inbox::drain(&dir, "lead-w");
    assert_eq!(msgs.len(), 1, "expected exactly one re-arm diagnostic");
    assert_eq!(
        msgs[0].kind.as_deref(),
        Some("review-class-unresolved"),
        "must be the re-arm diagnostic, never a premature [pr-ready-for-merge]"
    );
    assert!(
        msgs[0].text.contains("review_class=single|dual"),
        "diagnostic must carry the actionable re-arm instruction: {}",
        msgs[0].text
    );

    // Second scan: debounced per `diagnostic_emitted_for_sha` — no re-emit.
    scan_and_emit_with(
        &dir,
        &registry,
        &crate::daemon::pr_state::gh_poll::CliGhPoller,
    );
    assert!(
        crate::inbox::drain(&dir, "lead-w").is_empty(),
        "second scan must not re-emit the diagnostic at the same head_sha"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// T17: record_ci_result creates the file on first observation;
/// subsequent calls update in-place.
#[test]
fn t17_record_ci_result_creates_then_updates() {
    let dir = std::env::temp_dir().join(format!("agend-972-rcr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // First observation: file did not exist.
    assert!(load(&dir, "owner/repo", "feat/x").is_none());
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/x",
        "sha-A",
        CiConclusion::Pending,
        vec!["dev".to_string()],
        ReviewClass::Single,
    );
    let s = load(&dir, "owner/repo", "feat/x").expect("created");
    assert_eq!(s.head_sha, "sha-A");
    assert_eq!(s.subscribers, vec!["dev".to_string()]);

    // Second observation: file updates to Green.
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/x",
        "sha-A",
        CiConclusion::Green,
        vec![],
        ReviewClass::Single,
    );
    let s = load(&dir, "owner/repo", "feat/x").expect("reloaded");
    assert!(matches!(s.ci_state, CiState::Green { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}

// ── task66 legacy-containment regression cases ────────────────────────
// `record_verdict` remains test-only so old durable name+SHA behavior can be
// pinned fail-closed. It may update the display projection or legacy sidecar,
// but neither can create a validated receipt or open the merge gate.

fn vbuf_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "agend-2059c-{}-{}-{}",
        tag,
        std::process::id(),
        C.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A legacy verdict that precedes PR state may remain in its TTL sidecar, but
/// observing CI must never replay it into typed review authority.
#[test]
fn legacy_buffered_verdict_never_replays_as_authority_2760() {
    let dir = vbuf_home("replay");
    // Verdict arrives first — no pr-state exists yet (gate E / #2058).
    record_verdict(
        &dir,
        "t-review-task",
        "reviewer-1",
        Some("sha-A"),
        VerdictKind::Verified,
    );
    assert!(
        load(&dir, "owner/repo", "feat/x").is_none(),
        "a verdict must NOT itself create a pr-state — it buffers"
    );
    // CI observes sha-A green → creates the state + drains/replays the buffer.
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/x",
        "sha-A",
        CiConclusion::Green,
        vec!["dev".to_string()],
        ReviewClass::Single,
    );
    let s = load(&dir, "owner/repo", "feat/x").expect("created");
    assert!(
        !is_merge_ready(&s),
        "legacy buffered VERIFIED must stay inert even when CI observes the same SHA"
    );
    assert!(s.validated_review_receipts.is_empty());
    // The production typed drain did not consume the legacy entry. Drain it
    // explicitly through the test-only helper for a deterministic cleanup pin.
    assert!(
        !verdict_buffer::drain_for_head(&dir, "sha-A").is_empty(),
        "legacy sidecar may await TTL cleanup, but is never replayed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A legacy SHA/name verdict cannot authorize either the matching state or a
/// same-named fork at a different SHA.
#[test]
fn legacy_verdict_cannot_authorize_same_branch_fork_2760() {
    let dir = vbuf_home("fork");
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/x",
        "sha-A",
        CiConclusion::Green,
        vec!["dev".into()],
        ReviewClass::Single,
    );
    // A fork's PR: identical branch name, different repo + head SHA.
    record_ci_result(
        &dir,
        "fork/repo",
        "feat/x",
        "sha-B",
        CiConclusion::Green,
        vec!["dev".into()],
        ReviewClass::Single,
    );
    // Verdict for sha-A.
    record_verdict(
        &dir,
        "t",
        "reviewer-1",
        Some("sha-A"),
        VerdictKind::Verified,
    );
    let original = load(&dir, "owner/repo", "feat/x").unwrap();
    let fork = load(&dir, "fork/repo", "feat/x").unwrap();
    assert!(!is_merge_ready(&original));
    assert!(!is_merge_ready(&fork));
    assert!(original.validated_review_receipts.is_empty());
    assert!(fork.validated_review_receipts.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Even two distinct legacy names cannot manufacture a Dual typed quorum.
#[test]
fn legacy_multi_reviewer_names_cannot_form_typed_quorum_2760() {
    let dir = vbuf_home("multi");
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/d",
        "sha-A",
        CiConclusion::Green,
        vec!["dev".into()],
        ReviewClass::Dual,
    );
    record_verdict(
        &dir,
        "t",
        "reviewer-1",
        Some("sha-A"),
        VerdictKind::Verified,
    );
    assert!(
        !is_merge_ready(&load(&dir, "owner/repo", "feat/d").unwrap()),
        "1 of 2 verified — not yet merge-ready under dual class"
    );
    record_verdict(
        &dir,
        "t",
        "reviewer-2",
        Some("sha-A"),
        VerdictKind::Verified,
    );
    assert!(
        !is_merge_ready(&load(&dir, "owner/repo", "feat/d").unwrap()),
        "two legacy names still provide zero typed assignment receipts"
    );
    assert!(load(&dir, "owner/repo", "feat/d")
        .unwrap()
        .validated_review_receipts
        .is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A legacy verdict is inert before and after a force-push; a late stale-SHA
/// row cannot resurrect review authority.
#[test]
fn legacy_forcepush_verdict_never_flips_gate_2760() {
    let dir = vbuf_home("stale");
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/x",
        "sha-A",
        CiConclusion::Green,
        vec!["dev".into()],
        ReviewClass::Single,
    );
    record_verdict(
        &dir,
        "t",
        "reviewer-1",
        Some("sha-A"),
        VerdictKind::Verified,
    );
    assert!(
        !is_merge_ready(&load(&dir, "owner/repo", "feat/x").unwrap()),
        "legacy VERIFIED is display-only at the original head"
    );
    // Force-push: CI now observes a NEW head sha-B. CiObserved clears the
    // accumulated sha-A verdict.
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/x",
        "sha-B",
        CiConclusion::Green,
        vec![],
        ReviewClass::Single,
    );
    assert!(
        !is_merge_ready(&load(&dir, "owner/repo", "feat/x").unwrap()),
        "head advanced to sha-B; the sha-A verdict no longer applies"
    );
    // A LATE verdict still asserting the stale sha-A: no state is at sha-A
    // now → it buffers, and is never drained for sha-B → can't resurrect.
    record_verdict(
        &dir,
        "t",
        "reviewer-1",
        Some("sha-A"),
        VerdictKind::Verified,
    );
    assert!(
        !is_merge_ready(&load(&dir, "owner/repo", "feat/x").unwrap()),
        "a stale-SHA verdict must not flip the advanced head merge-ready"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// T19 (reviewer-rejection fix coverage): `record_ci_result` honors
/// the `review_class` argument on FIRST observation, persists it to
/// the pr_state file. This is the production code path that was
/// missing pre-#972-rejection-fix — without it the pr_state file
/// always defaulted to Single regardless of ci-watch's
/// `review_class` field.
#[test]
fn t19_record_ci_result_propagates_review_class_dual() {
    let dir = std::env::temp_dir().join(format!("agend-972-dual-prop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // First observation with Dual. File MUST have Dual.
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/dual",
        "sha-A",
        CiConclusion::Pending,
        vec!["dev".to_string()],
        ReviewClass::Dual,
    );
    let s = load(&dir, "owner/repo", "feat/dual").expect("created");
    assert_eq!(
        s.review_class,
        ReviewClass::Dual,
        "first-observation review_class must propagate from ci-watch"
    );

    // #2745 R2 no-weaken: a subsequent observation feeding a WEAKER class
    // (Single) must NOT downgrade the persisted Dual — a stale or accidental
    // second watch feed can never silently relax a 2-distinct-reviewer gate.
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/dual",
        "sha-A",
        CiConclusion::Green,
        vec![],
        ReviewClass::Single,
    );
    let s = load(&dir, "owner/repo", "feat/dual").expect("reloaded");
    assert_eq!(
        s.review_class,
        ReviewClass::Dual,
        "a stale/weaker watch feed must not downgrade a persisted Dual (no-weaken)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// #2745 R2 (a) — root R1 finding: a persisted `Unresolved` pr-state RECOVERS
/// when the operator re-arms with an explicit class. The next CI observation
/// (production `record_ci_result`) reconciles the state to `Dual`, and merge
/// readiness then requires TWO distinct VERIFIED. Before R2 this loop could
/// never close (review_class was create-only → stranded Unresolved forever).
#[test]
fn r2a_persisted_unresolved_recovers_on_rearm_dual_2745() {
    let dir = std::env::temp_dir().join(format!("agend-2745-r2a-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A legacy/typo watch first persists an Unresolved state at sha-A.
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/x",
        "sha-A",
        CiConclusion::Green,
        vec!["dev".to_string()],
        ReviewClass::Unresolved,
    );
    let s = load(&dir, "owner/repo", "feat/x").expect("created");
    assert_eq!(s.review_class, ReviewClass::Unresolved, "starts Unresolved");
    assert!(!is_merge_ready(&s), "Unresolved never ready");

    // Operator re-arms `review_class=dual`; the next poll feeds Dual.
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/x",
        "sha-A",
        CiConclusion::Green,
        vec![],
        ReviewClass::Dual,
    );
    let mut s = load(&dir, "owner/repo", "feat/x").expect("reloaded");
    assert_eq!(
        s.review_class,
        ReviewClass::Dual,
        "re-arm must recover the persisted class (loop closes)"
    );
    assert_eq!(
        s.diagnostic_emitted_for_sha, None,
        "diagnostic debounce cleared on leaving Unresolved"
    );

    // Readiness now enforces the 2-distinct-VERIFIED Dual threshold.
    observe_typed_verified(&mut s, "rev-1", "sha-A");
    assert!(
        !is_merge_ready(&s),
        "one VERIFIED is not enough for recovered Dual"
    );
    observe_typed_verified(&mut s, "rev-2", "sha-A");
    assert!(
        is_merge_ready(&s),
        "two distinct VERIFIED open the recovered Dual gate"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// #2745 R2 (b) — root R1 finding: a persisted legacy `Single` state (the
/// pre-#2745 poller-collapse class) is INVENTORIED when the current watch
/// resolves `Unresolved` (legacy None / typo). The next production
/// `record_ci_result` refreshes it to `Unresolved` → `is_merge_ready` closes,
/// so the scanner's [review-class-unresolved] diagnostic can fire. This makes
/// the fail-closed migration actually bite pre-existing state files.
#[test]
fn r2b_persisted_single_refreshes_to_unresolved_on_legacy_watch_2745() {
    let dir = std::env::temp_dir().join(format!("agend-2745-r2b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Pre-existing state: legacy Single.
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/y",
        "sha-A",
        CiConclusion::Green,
        vec!["dev".to_string()],
        ReviewClass::Single,
    );
    assert_eq!(
        load(&dir, "owner/repo", "feat/y").unwrap().review_class,
        ReviewClass::Single
    );

    // The current watch has NO explicit review_class → poller resolves Unresolved.
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/y",
        "sha-A",
        CiConclusion::Green,
        vec![],
        ReviewClass::Unresolved,
    );
    let mut s = load(&dir, "owner/repo", "feat/y").expect("reloaded");
    assert_eq!(
        s.review_class,
        ReviewClass::Unresolved,
        "a legacy Single state must be refreshed to Unresolved by an unresolved \
             current watch (fail-closed inventory bites pre-existing files)"
    );
    // Even a VERIFIED cannot open the gate now.
    apply(
        &mut s,
        Event::VerdictObserved {
            reviewer: "rev-1",
            reviewed_head: "sha-A",
            kind: VerdictKind::Verified,
        },
    );
    assert!(
        !is_merge_ready(&s),
        "refreshed Unresolved is never merge-ready"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// #2745 R2/R3: `reconcile_review_class` transition matrix — Dual is a
/// MONOTONIC FLOOR (any ordinary watch keeps Dual; closes the R2
/// Dual→Unresolved→Single two-observation bypass), Unresolved recovers on
/// re-arm, and a Single gate fail-closes to Unresolved on a legacy/typo watch.
#[test]
fn r2_reconcile_review_class_transition_matrix_2745() {
    use ReviewClass::{Dual, Single, Unresolved};
    // Dual is a MONOTONIC FLOOR — no ordinary watch can drop below it.
    assert_eq!(
        reconcile_review_class(Dual, Single),
        Dual,
        "Dual floor vs Single"
    );
    assert_eq!(
        reconcile_review_class(Dual, Unresolved),
        Dual,
        "Dual floor vs Unresolved — closes the two-observation downgrade bypass"
    );
    assert_eq!(reconcile_review_class(Dual, Dual), Dual);
    // Recovery from Unresolved: adopt whatever the re-arm declared.
    assert_eq!(reconcile_review_class(Unresolved, Single), Single);
    assert_eq!(reconcile_review_class(Unresolved, Dual), Dual);
    assert_eq!(reconcile_review_class(Unresolved, Unresolved), Unresolved);
    // Single gate: current watch Unresolved → fail-closed inventory; else adopt.
    assert_eq!(reconcile_review_class(Single, Unresolved), Unresolved);
    assert_eq!(
        reconcile_review_class(Single, Dual),
        Dual,
        "single→dual strengthens"
    );
    assert_eq!(reconcile_review_class(Single, Single), Single);
}

/// #2745 R2: head advance (force-push) preserves the review_class — apply()
/// clears verdicts + debounce keys on head advance but NOT the class, and
/// reconcile then keeps it, so a stale post-advance Single feed can't weaken.
#[test]
fn r2_head_advance_preserves_review_class_2745() {
    let mut s = new_state("sha-A", ReviewClass::Dual);
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-B",
            conclusion: CiConclusion::Green,
            observed_at: now(),
        },
    );
    assert_eq!(
        s.review_class,
        ReviewClass::Dual,
        "head advance must not reset the review_class"
    );
    assert_eq!(
        reconcile_review_class(s.review_class, ReviewClass::Single),
        ReviewClass::Dual,
        "post-advance stale Single feed still cannot weaken Dual"
    );
}

/// #2745 R3 (root R2 finding 1) — production SEQUENCE RED: a persisted Dual gate
/// must survive a Dual→Unresolved→Single observation sequence through the real
/// `record_ci_result` entry. Before the monotonic-floor fix, the intermediate
/// Unresolved laundered the floor so the trailing Single downgraded the gate to
/// one reviewer — and with one already-recorded VERIFIED at head, the next scan
/// would have flipped merge-ready. Now: stays Dual, still needs 2 distinct VERIFIED.
#[test]
fn r3_dual_floor_survives_unresolved_then_single_sequence_2745() {
    let dir = std::env::temp_dir().join(format!("agend-2745-r3seq-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Persist a Dual gate with CI green + one VERIFIED at head (the dangerous
    // pre-condition: one downgrade away from merge-ready).
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/z",
        "sha-A",
        CiConclusion::Green,
        vec!["dev".to_string()],
        ReviewClass::Dual,
    );
    {
        let mut s = load(&dir, "owner/repo", "feat/z").unwrap();
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-A",
                kind: VerdictKind::Verified,
            },
        );
        save(&dir, &s).unwrap();
        assert!(!is_merge_ready(&s), "one VERIFIED is not enough for Dual");
    }

    // Obs 2: a missing/typo/stale watch → Unresolved. Must NOT launder the floor.
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/z",
        "sha-A",
        CiConclusion::Green,
        vec![],
        ReviewClass::Unresolved,
    );
    assert_eq!(
        load(&dir, "owner/repo", "feat/z").unwrap().review_class,
        ReviewClass::Dual,
        "Dual floor survives an intermediate Unresolved observation"
    );

    // Obs 3: a Single observation/re-arm. Must NOT downgrade to one reviewer.
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/z",
        "sha-A",
        CiConclusion::Green,
        vec![],
        ReviewClass::Single,
    );
    let s = load(&dir, "owner/repo", "feat/z").unwrap();
    assert_eq!(
        s.review_class,
        ReviewClass::Dual,
        "Dual floor survives a trailing Single observation — no two-obs bypass"
    );
    assert!(
        !is_merge_ready(&s),
        "the one existing VERIFIED must NOT flip a floored Dual gate merge-ready"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// T20 (reviewer-rejection fix end-to-end): full pipeline for a
/// dual-review PR. CI green + ONE VERIFIED → NotReady. Second
/// VERIFIED from a distinct reviewer at the same SHA → MergeReady.
/// scan_and_emit fires `[pr-ready-for-merge]` only on the second
/// verdict, not the first.
#[test]
fn t20_dual_review_does_not_merge_until_two_verdicts_e2e() {
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    let dir = std::env::temp_dir().join(format!("agend-972-dual-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(dir.join("inbox")).ok();
    // #2059-#3: ready events route to the team orchestrator (merge
    // authority). "dev" is a team member; "lead-v" merges.
    write_team_fleet(&dir, "lead-v", &["dev"]);

    // First CI observation arms the file with Dual.
    record_ci_result(
        &dir,
        "owner/repo",
        "feat/dual-e2e",
        "sha-A",
        CiConclusion::Green,
        vec!["dev".to_string()],
        ReviewClass::Dual,
    );

    // ONE verdict arrives. State must NOT transition to MergeReady.
    let mut s = load(&dir, "owner/repo", "feat/dual-e2e").unwrap();
    observe_typed_verified(&mut s, "rev-1", "sha-A");
    assert_eq!(
        s.merge_state,
        MergeState::NotReady,
        "dual-review with one verdict must stay NotReady"
    );
    save(&dir, &s).unwrap();

    // Scanner pass: NO event emitted because state is NotReady.
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    scan_and_emit_with(
        &dir,
        &registry,
        &crate::daemon::pr_state::gh_poll::CliGhPoller,
    );
    assert!(
        crate::inbox::drain(&dir, "lead-v").is_empty(),
        "no [pr-ready-for-merge] until second verdict"
    );

    // SECOND verdict from distinct reviewer. Now MergeReady.
    let mut s = load(&dir, "owner/repo", "feat/dual-e2e").unwrap();
    observe_typed_verified(&mut s, "rev-2", "sha-A");
    assert_eq!(s.merge_state, MergeState::MergeReady);
    // #2749: dual-review gate test — stamp a fresh ancestry tuple so the
    // read-only freshness gate admits the emission once both verdicts land.
    stamp_fresh_ancestry(&mut s);
    save(&dir, &s).unwrap();

    // Scanner now fires [pr-ready-for-merge].
    scan_and_emit_with(
        &dir,
        &registry,
        &crate::daemon::pr_state::gh_poll::CliGhPoller,
    );
    let msgs = crate::inbox::drain(&dir, "lead-v");
    assert_eq!(msgs.len(), 1, "second verdict unlocks the merge gate");
    assert_eq!(msgs[0].kind.as_deref(), Some("pr-ready-for-merge"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// #2745 RED-3 (fail-closed core, THE repro): a PrState whose `review_class`
/// is `Unresolved` (intent ABSENT / UNKNOWN / MISMATCHED at arm time) must
/// NEVER reach MergeReady — not even with CI green ∧ a VERIFIED at head. An
/// omitted intent can never silently take the single-VERIFIED path. RED
/// today: `Unresolved::required_verified_count()` is a `1` placeholder so
/// `is_merge_ready` fires on one verdict (the #2745 premature pr-ready).
/// GREEN makes `is_merge_ready` reject `Unresolved` outright + the emitter
/// raises an actionable diagnostic instead.
#[test]
fn red3_unresolved_class_never_merge_ready_2745() {
    let mut s = new_state("sha-A", ReviewClass::Unresolved);
    apply(
        &mut s,
        Event::CiObserved {
            head_sha: "sha-A",
            conclusion: CiConclusion::Green,
            observed_at: now(),
        },
    );
    apply(
        &mut s,
        Event::VerdictObserved {
            reviewer: "rev-1",
            reviewed_head: "sha-A",
            kind: VerdictKind::Verified,
        },
    );
    assert!(
        !is_merge_ready(&s),
        "Unresolved review_class must never be merge-ready (fail-closed #2745)"
    );
    assert_eq!(s.merge_state, MergeState::NotReady);
}

/// #2745 RED-6 (cold-load Dual guard): a `Dual` PrState carrying ONE VERIFIED,
/// saved then reloaded from disk, must come back as `Dual` with its single
/// verdict tally intact — NOT silently collapsed to `Single` (which would let
/// the one existing verdict flip it merge-ready on cold load). The 2-distinct
/// threshold must still hold post-restart. Guards GREEN's serde/default
/// changes against introducing a Single-default-on-load loss path.
#[test]
fn red6_cold_load_dual_class_and_tally_guard_2745() {
    let dir = std::env::temp_dir().join(format!("agend-2745-coldload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut s = new_state("sha-A", ReviewClass::Dual);
    s.ci_state = CiState::Green {
        sha: "sha-A".to_string(),
        observed_at: now(),
    };
    s.verdict_state = VerdictState::Verified {
        reviewers: vec![("rev-1".to_string(), "sha-A".to_string())],
    };
    save(&dir, &s).unwrap();

    let loaded = load(&dir, &s.repo, &s.branch).expect("reload");
    assert_eq!(
        loaded.review_class,
        ReviewClass::Dual,
        "cold load must preserve Dual — never default to Single"
    );
    if let VerdictState::Verified { reviewers } = &loaded.verdict_state {
        assert_eq!(
            reviewers.len(),
            1,
            "single-verdict tally preserved across reload"
        );
    } else {
        panic!(
            "expected Verified after reload, got {:?}",
            loaded.verdict_state
        );
    }
    assert!(
        !is_merge_ready(&loaded),
        "Dual still needs 2 distinct VERIFIED after cold load — one must not flip ready"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// #2745 RED (poller parser, fail-closed): `parse_review_class` — the
/// production source of the `ReviewClass` passed into `record_ci_result` — is
/// FAIL-CLOSED. Only the exact tokens `single`/`dual` (case-insensitive)
/// resolve; everything else (absent / null / unknown string / typo / wrong
/// type) is `Unresolved`, NEVER a silent `Single`. Inverts the pre-#2745
/// fail-OPEN contract (unknown→Single) that let an omitted/typo'd class merge
/// on one VERIFIED. RED today: the parser still collapses the residual to
/// `Single`. GREEN routes it through `ReviewClass::parse_fail_closed`.
#[test]
fn t21_parse_review_class_fail_closed_2745() {
    use crate::daemon::ci_watch::parse_review_class;
    use serde_json::json;

    assert_eq!(
        parse_review_class(&json!({"review_class": "dual"})),
        ReviewClass::Dual
    );
    assert_eq!(
        parse_review_class(&json!({"review_class": "DUAL"})),
        ReviewClass::Dual
    );
    assert_eq!(
        parse_review_class(&json!({"review_class": "Dual"})),
        ReviewClass::Dual
    );
    assert_eq!(
        parse_review_class(&json!({"review_class": "single"})),
        ReviewClass::Single
    );
    assert_eq!(
        parse_review_class(&json!({"review_class": "unknown"})),
        ReviewClass::Unresolved,
        "unknown strings FAIL CLOSED to Unresolved (never a silent Single)"
    );
    assert_eq!(
        parse_review_class(&json!({"review_class": "duel"})),
        ReviewClass::Unresolved,
        "a typo'd 'duel' must not silently degrade to Single"
    );
    assert_eq!(
        parse_review_class(&json!({"review_class": null})),
        ReviewClass::Unresolved
    );
    assert_eq!(
        parse_review_class(&json!({})),
        ReviewClass::Unresolved,
        "absent field FAILS CLOSED (never a silent Single)"
    );
    assert_eq!(
        parse_review_class(&json!({"review_class": 42})),
        ReviewClass::Unresolved,
        "wrong type FAILS CLOSED"
    );
}

// ─── #986 caller-path integration tests (T2/T4/T5/T6/T9/T10) ─────

use crate::daemon::pr_state::gh_poll::tests::MockGhPoller;
use crate::daemon::pr_state::gh_poll::{GhPrMetadata, GhPrState};

fn home_with_state(tag: &str, state: PrState) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("agend-986-int-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();
    std::fs::create_dir_all(home.join("inbox")).ok();
    save(&home, &state).unwrap();
    home
}

fn empty_registry() -> crate::agent::AgentRegistry {
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;
    Arc::new(Mutex::new(HashMap::new()))
}

fn gh_meta_open(number: u64, branch: &str, author: &str) -> GhPrMetadata {
    GhPrMetadata {
        number,
        author_login: author.to_string(),
        head_ref: branch.to_string(),
        is_cross_repository: false,
        is_draft: false,
        state: GhPrState::Open,
        merged_at: None,
        head_ref_oid: None,
        base_ref_oid: None,
    }
}

/// #986 T2 — first observation populates pr_number + pr_author.
/// Before scan: state.pr_number=0, pr_author="". After scan with
/// gh-poll returning the matching PR: both fields populated.
#[test]
fn t9_first_gh_observation_populates_pr_identity() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    s.pr_author = String::new(); // simulate freshly-created state
    s.pr_number = 0;
    let home = home_with_state("first-obs", s);
    let poller = MockGhPoller::new(vec![Ok(vec![gh_meta_open(970, "feat/test", "dev")])]);

    scan_and_emit_with(&home, &empty_registry(), &poller);

    let loaded = load(&home, "owner/repo", "feat/test").unwrap();
    assert_eq!(loaded.pr_number, 970);
    assert_eq!(loaded.pr_author, "dev"); // tier 2 direct name match (no fleet entries)
    assert_eq!(loaded.gh_poll_failures, 0);
    assert!(loaded.last_gh_poll_at.is_some());
    let _ = std::fs::remove_dir_all(&home);
}

/// #986 §3.9 real-entry: a COLD `SnapshotGhPoller` (the background worker has
/// not filled the cache yet) driven through the REAL scanner must NOT mark the
/// pr_state as a clean "polled, 0 PR" — it records a gh_poll_failure (→
/// ambiguous), so a worktree whose open PR is simply not-yet-in-cache is never
/// false-released. Asserts the scanner reads the snapshot (cold→Err), never a
/// live `gh` poll.
#[test]
fn cold_snapshot_poll_marks_failure_not_false_no_pr_986() {
    let mut s = new_state("sha-cold", ReviewClass::Single);
    s.pr_number = 0;
    s.last_gh_poll_at = None; // never polled → due
    let home = home_with_state("cold-986", s);
    // COLD cache — the worker has never run, so the repo is absent.
    let cache = crate::daemon::pr_state::gh_poll::GhPollCache::new();
    let poller = crate::daemon::pr_state::gh_poll::SnapshotGhPoller::new(cache);
    scan_and_emit_with(&home, &empty_registry(), &poller);
    let loaded = load(&home, "owner/repo", "feat/test").unwrap();
    assert!(
        loaded.gh_poll_failures > 0,
        "#986: cold-cache poll must mark a failure (ambiguous), not a clean 'no PR'"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #986 Bug A §3.9 real-entry: a WARM repo whose cached snapshot predates the
/// branch's `created_at` (the branch's PR exists but isn't in the stale
/// snapshot yet) must NOT confirm "no PR" for that branch — it must NOT stamp
/// `last_gh_poll_at` (which would make `evaluate_pr_for_release` → QueriedNone →
/// false-release). Drives the REAL scanner with a stale snapshot.
#[test]
fn stale_snapshot_does_not_confirm_no_pr_986() {
    let mut s = new_state("sha-stale", ReviewClass::Single);
    s.pr_number = 0;
    s.last_gh_poll_at = None; // created_at is "now" (new_state)
    let home = home_with_state("stale-986", s);
    // Warm cache, but the snapshot was polled FAR in the past (before created_at)
    // and is empty (the branch's PR isn't captured) — the stale-reuse case.
    let cache = crate::daemon::pr_state::gh_poll::GhPollCache::new();
    cache.seed_for_test("owner/repo", vec![], "2000-01-01T00:00:00+00:00");
    let poller = crate::daemon::pr_state::gh_poll::SnapshotGhPoller::new(cache);
    scan_and_emit_with(&home, &empty_registry(), &poller);
    let loaded = load(&home, "owner/repo", "feat/test").unwrap();
    assert!(
        loaded.last_gh_poll_at.is_none(),
        "#986: a stale snapshot (polled before the branch was tracked) must NOT \
             confirm no-PR — last_gh_poll_at must stay unset (ambiguous)"
    );
    assert_eq!(loaded.pr_number, 0, "no PR observed (stale snapshot empty)");
    let _ = std::fs::remove_dir_all(&home);
}

/// #986 round-3 (codex) §3.9 real-entry: a stale snapshot that FINDS the
/// branch's PR as `Closed` (the PR was since reopened; the worker hasn't
/// refreshed) must NOT apply the sticky `ClosedUnmergedObserved` transition.
/// Removing the `found ||` bypass means freshness gates found-PR transitions
/// too — a stale found-PR is applied to nothing until a fresh poll.
#[test]
fn stale_found_pr_does_not_drive_terminal_transition_986() {
    let mut s = new_state("sha-reopen", ReviewClass::Single);
    s.pr_number = 42; // previously observed open
    s.last_gh_poll_at = Some("2026-06-06T00:00:00+00:00".into());
    // merge_state defaults to NotReady (non-terminal) — the PR is open.
    let home = home_with_state("reopen-986", s);
    // Stale snapshot (polled long before created_at) showing the PR as Closed.
    let closed = GhPrMetadata {
        number: 42,
        author_login: "dev".into(),
        head_ref: "feat/test".into(),
        is_cross_repository: false,
        is_draft: false,
        state: GhPrState::Closed,
        merged_at: None,
        head_ref_oid: None,
        base_ref_oid: None,
    };
    let cache = crate::daemon::pr_state::gh_poll::GhPollCache::new();
    cache.seed_for_test("owner/repo", vec![closed], "2000-01-01T00:00:00+00:00");
    let poller = crate::daemon::pr_state::gh_poll::SnapshotGhPoller::new(cache);
    scan_and_emit_with(&home, &empty_registry(), &poller);
    let loaded = load(&home, "owner/repo", "feat/test").unwrap();
    assert!(
        !matches!(
            loaded.merge_state,
            MergeState::ClosedUnmerged { .. } | MergeState::Merged { .. }
        ),
        "#986: a STALE found-Closed must NOT drive a terminal transition: {:?}",
        loaded.merge_state
    );
    assert!(
        loaded.last_gh_poll_at == Some("2026-06-06T00:00:00+00:00".into()),
        "stale poll must not re-stamp last_gh_poll_at"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// PR-3 (t-ci-ready-pr3-arm-not-armed) — INTEGRATION (codex re-verify): a
/// BOUND branch with NO ci-watch AND NO pr-state file must STILL be
/// discovered and auto-armed. This exercises the binding-seeded discovery
/// path through the real scanner — the exact structural hole codex's first
/// pass found, which the `auto_arm` unit tests (injecting `prs` directly)
/// could not reach. Without the bound-branch seed, the repo never enters the
/// poll list (no pr-state) → the open PR is never discovered → #1782 unfixed.
#[test]
#[cfg(unix)]
fn pr3_bound_branch_with_no_seed_is_discovered_and_armed() {
    let parent = std::env::temp_dir().join(format!("agend-pr3-integ-{}", std::process::id()));
    let home = parent.join("home");
    std::fs::create_dir_all(&home).unwrap();

    // Source repo whose origin remote resolves to the slug "owner/repo".
    let repo_path = parent.join("source-repo");
    std::fs::create_dir_all(&repo_path).unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec![
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ],
    ] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(&repo_path)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
    }

    // Bind "dev-x" → branch "feat/x" in that repo. NO pr-state, NO ci-watch.
    let bdir = crate::paths::runtime_dir(&home).join("dev-x");
    std::fs::create_dir_all(&bdir).unwrap();
    std::fs::write(
        bdir.join("binding.json"),
        serde_json::to_string(&serde_json::json!({
            "version": 1, "agent": "dev-x", "task_id": "t",
            "branch": "feat/x", "worktree": "/tmp/wt-dev-x",
            "source_repo": repo_path.display().to_string(),
            "issued_at": "2026-06-05T00:00:00Z",
        }))
        .unwrap(),
    )
    .unwrap();

    // #986 round-4: auto_arm MOVED to the worker, so discovery now spans
    // scanner → worker. (1) The scanner (snapshot poller) must seed DEMAND for
    // the bound branch's repo (the #1782 discovery seed). (2) The worker then
    // polls that repo (one OPEN PR) and runs auto_arm on the FRESH data.
    let cache = crate::daemon::pr_state::gh_poll::GhPollCache::new();
    scan_and_emit_with(
        &home,
        &empty_registry(),
        &crate::daemon::pr_state::gh_poll::SnapshotGhPoller::new(cache.clone()),
    );
    assert!(
        cache.demand_contains_for_test("owner/repo"),
        "scanner must seed demand for the bound branch's repo (#1782 discovery)"
    );
    let poller = MockGhPoller::new(vec![Ok(vec![gh_meta_open(700, "feat/x", "suzuke")])]);
    crate::daemon::pr_state::gh_poll::worker_poll_and_act(&home, &cache, "owner/repo", &poller);

    // The worker's auto_arm must have armed a watch for the discovered branch.
    let watch = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("owner/repo", "feat/x"),
    );
    assert!(
        watch.exists(),
        "a bound branch with no pr-state/watch seed must be discovered + auto-armed"
    );
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch).unwrap()).unwrap();
    let subs: Vec<&str> = v["subscribers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["instance"].as_str())
        .collect();
    assert!(
        subs.contains(&"dev-x"),
        "subscriber must be the BOUND agent (not the gh author login): {subs:?}"
    );
    std::fs::remove_dir_all(&parent).ok();
}

/// #986 T4 — `gh state=MERGED + mergedAt!=None` fires
/// MergedObserved → reducer transitions to Merged terminal state
/// → scanner emits `[pr-merged]` to author inbox + sweeps file.
#[test]
fn t10_merged_observation_fires_pr_merged_event() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    s.pr_author = String::new();
    let home = home_with_state("merged-obs", s);
    let merged_meta = GhPrMetadata {
        number: 970,
        author_login: "dev".into(),
        head_ref: "feat/test".into(),
        is_cross_repository: false,
        is_draft: false,
        state: GhPrState::Merged,
        merged_at: Some("2026-05-20T04:17:09Z".to_string()),
        head_ref_oid: None,
        base_ref_oid: None,
    };
    let poller = MockGhPoller::new(vec![Ok(vec![merged_meta.clone()])]);

    scan_and_emit_with(&home, &empty_registry(), &poller);

    // #1287: first scan emits but persists with dedup flag — file
    // survives until next scan confirms already_emitted.
    let persisted = load(&home, "owner/repo", "feat/test")
        .expect("#1287: file must survive first scan with dedup flag");
    assert_eq!(
        persisted.ready_emitted_for_sha.as_deref(),
        Some("sha-A"),
        "#1287: ready_emitted_for_sha must be set after emit"
    );
    // [pr-merged] in dev's inbox.
    let msgs = crate::inbox::drain(&home, "dev");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].kind.as_deref(), Some("pr-merged"));
    assert!(msgs[0].text.contains("pr-merged"));

    // Second scan: already_emitted → remove without re-emitting.
    let poller2 = MockGhPoller::new(vec![Ok(vec![merged_meta])]);
    scan_and_emit_with(&home, &empty_registry(), &poller2);
    assert!(
        load(&home, "owner/repo", "feat/test").is_none(),
        "second scan must sweep terminal file"
    );
    let msgs2 = crate::inbox::drain(&home, "dev");
    assert!(msgs2.is_empty(), "#1287: no duplicate emit on second scan");
    let _ = std::fs::remove_dir_all(&home);
}

/// [C1 / #1842] §3.9: a merged PR is announced ONCE even across the
/// scan-`remove` → lingering-CI-`_or_create` re-create loop. The recreated
/// state file has `ready_emitted_for_sha = None` (the reset that drove the 8×
/// re-emit), but the persistent terminal-emit ledger suppresses the replay.
#[test]
fn c1_merged_not_reemitted_after_delete_recreate_1842() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    s.pr_author = String::new();
    let home = home_with_state("c1-reemit-1842", s);
    let merged_meta = GhPrMetadata {
        number: 970,
        author_login: "dev".into(),
        head_ref: "feat/test".into(),
        is_cross_repository: false,
        is_draft: false,
        state: GhPrState::Merged,
        merged_at: Some("2026-05-20T04:17:09Z".to_string()),
        head_ref_oid: None,
        base_ref_oid: None,
    };

    // Scan 1: observe merged → emit [pr-merged] exactly once.
    scan_and_emit_with(
        &home,
        &empty_registry(),
        &MockGhPoller::new(vec![Ok(vec![merged_meta.clone()])]),
    );
    assert_eq!(
        crate::inbox::drain(&home, "dev").len(),
        1,
        "scan 1 emits [pr-merged] once"
    );

    // Scan 2: already_emitted (per-file flag) → sweep the terminal file.
    scan_and_emit_with(
        &home,
        &empty_registry(),
        &MockGhPoller::new(vec![Ok(vec![merged_meta.clone()])]),
    );
    assert!(
        load(&home, "owner/repo", "feat/test").is_none(),
        "scan 2 sweeps the terminal file"
    );
    assert!(
        crate::inbox::drain(&home, "dev").is_empty(),
        "scan 2 does not re-emit"
    );

    // RECREATE: a lingering CI observation re-creates a FRESH Merged state
    // file with ready_emitted_for_sha=None for the SAME merge identity (the
    // #1842 loop). Without the ledger this re-emits; with it, suppressed.
    let mut recreated = new_state("sha-A", ReviewClass::Single);
    recreated.merge_state = MergeState::Merged {
        merge_commit: "sha-A".to_string(),
        merged_at: "2026-05-20T04:17:09Z".to_string(),
    };
    recreated.ready_emitted_for_sha = None;
    save(&home, &recreated).unwrap();

    // Scan 3: per-file flag is reset (None) but the persistent ledger says
    // emitted → NO replay.
    scan_and_emit_with(
        &home,
        &empty_registry(),
        &MockGhPoller::new(vec![Ok(vec![merged_meta])]),
    );
    assert!(
        crate::inbox::drain(&home, "dev").is_empty(),
        "[C1/#1842] recreated merged file must NOT re-emit (ledger survives delete+recreate)"
    );
    assert!(
        load(&home, "owner/repo", "feat/test").is_none(),
        "scan 3 sweeps the recreated file"
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// #986 T5 + #2131 — `gh state=CLOSED + mergedAt=None` is AMBIGUOUS under
/// squash-merge eventual consistency, so the FIRST observation DEFERS (no emit,
/// state survives with the pending flag); a SECOND consecutive closed-unmerged
/// observation confirms it → reducer transitions → scanner emits
/// `[pr-closed-unmerged]`; a THIRD scan removes (already-emitted dedup).
#[test]
fn t11_closed_unmerged_observation_fires_event() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    s.pr_author = String::new();
    let home = home_with_state("closed-obs", s);
    let closed_meta = GhPrMetadata {
        number: 970,
        author_login: "dev".into(),
        head_ref: "feat/test".into(),
        is_cross_repository: false,
        is_draft: false,
        state: GhPrState::Closed,
        merged_at: None,
        head_ref_oid: None,
        base_ref_oid: None,
    };

    // #2131: first observation DEFERS — no emit, state survives with the
    // pending flag (it may be a squash-merge mergedAt lag).
    let poller1 = MockGhPoller::new(vec![Ok(vec![closed_meta.clone()])]);
    scan_and_emit_with(&home, &empty_registry(), &poller1);
    let deferred = load(&home, "owner/repo", "feat/test")
        .expect("#2131: deferred state must survive (not terminal yet)");
    assert!(
        deferred.closed_unmerged_pending,
        "#2131: first closed-unmerged observation must DEFER (set pending)"
    );
    assert!(
        crate::inbox::drain(&home, "dev").is_empty(),
        "#2131: no [pr-closed-unmerged] on the first (deferred) observation"
    );

    // gh-poll is cadence-gated on `last_gh_poll_at` (scan1 just stamped it);
    // production polls are periodic so the two observations span two cadence
    // windows. Clear it so scan2 re-polls (pending flag is preserved).
    let repoll = |home: &std::path::Path| {
        let mut st = load(home, "owner/repo", "feat/test").unwrap();
        st.last_gh_poll_at = None;
        save(home, &st).unwrap();
    };
    repoll(&home);

    // Second consecutive closed observation → confirm → emit + dedup flag.
    let poller2 = MockGhPoller::new(vec![Ok(vec![closed_meta.clone()])]);
    scan_and_emit_with(&home, &empty_registry(), &poller2);
    let persisted = load(&home, "owner/repo", "feat/test")
        .expect("#1287: file must survive the confirm scan with dedup flag");
    assert_eq!(persisted.ready_emitted_for_sha.as_deref(), Some("sha-A"));
    let msgs = crate::inbox::drain(&home, "dev");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].kind.as_deref(), Some("pr-closed-unmerged"));

    // Third scan: already_emitted → remove without re-emitting.
    let poller3 = MockGhPoller::new(vec![Ok(vec![closed_meta])]);
    scan_and_emit_with(&home, &empty_registry(), &poller3);
    assert!(load(&home, "owner/repo", "feat/test").is_none());
    assert!(
        crate::inbox::drain(&home, "dev").is_empty(),
        "#1287: no duplicate emit"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #2131 regression — the squash-merge eventual-consistency sequence: gh first
/// reports `CLOSED + mergedAt=None` (the transient lag), then `MERGED + mergedAt`.
/// The scanner must NEVER emit a false `[pr-closed-unmerged]`: the first
/// observation DEFERS, and the merged observation resolves the lag (clears
/// pending, terminal = Merged). This is the PR #2129 false-signal reproduction.
/// (Neuter the #2131 deferral → the first observation emits → this test REDs.)
#[test]
fn t_2131_closed_then_merged_emits_no_false_closed_unmerged() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    s.pr_author = String::new();
    let home = home_with_state("closed-then-merged", s);
    let mk = |state: GhPrState, merged_at: Option<&str>| GhPrMetadata {
        number: 2129,
        author_login: "dev".into(),
        head_ref: "feat/test".into(),
        is_cross_repository: false,
        is_draft: false,
        state,
        merged_at: merged_at.map(String::from),
        head_ref_oid: None,
        base_ref_oid: None,
    };

    // Scan 1: transient CLOSED+mergedAt=None → DEFER, no emit.
    scan_and_emit_with(
        &home,
        &empty_registry(),
        &MockGhPoller::new(vec![Ok(vec![mk(GhPrState::Closed, None)])]),
    );
    assert!(
        crate::inbox::drain(&home, "dev").is_empty(),
        "#2131: must NOT emit on the transient closed-unmerged"
    );
    let mut st = load(&home, "owner/repo", "feat/test").unwrap();
    assert!(
        st.closed_unmerged_pending,
        "#2131: first observation must DEFER (pending)"
    );
    // Re-poll (clear the cadence stamp; production polls are periodic).
    st.last_gh_poll_at = None;
    save(&home, &st).unwrap();

    // Scan 2: mergedAt landed → MERGED. Must clear pending + go terminal=Merged,
    // and NEVER emit [pr-closed-unmerged].
    scan_and_emit_with(
        &home,
        &empty_registry(),
        &MockGhPoller::new(vec![Ok(vec![mk(
            GhPrState::Merged,
            Some("2026-06-14T08:44:29Z"),
        )])]),
    );
    let kinds: Vec<_> = crate::inbox::drain(&home, "dev")
        .iter()
        .filter_map(|m| m.kind.clone())
        .collect();
    assert!(
        !kinds.iter().any(|k| k == "pr-closed-unmerged"),
        "#2131: must NEVER emit a false [pr-closed-unmerged]; got {kinds:?}"
    );
    if let Some(final_st) = load(&home, "owner/repo", "feat/test") {
        assert!(
            matches!(final_st.merge_state, MergeState::Merged { .. }),
            "#2131: terminal state must be Merged, got {:?}",
            final_st.merge_state
        );
        assert!(
            !final_st.closed_unmerged_pending,
            "#2131: pending must clear once merged"
        );
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// #986 T7 — gh-poll failure increments `gh_poll_failures` and
/// updates `last_gh_poll_at` so backoff math kicks in next tick.
#[test]
fn t12_gh_poll_failure_increments_backoff_counter() {
    let s = new_state("sha-A", ReviewClass::Single);
    let home = home_with_state("backoff", s);
    let poller = MockGhPoller::new(vec![Err(anyhow::anyhow!("simulated rate limit"))]);

    scan_and_emit_with(&home, &empty_registry(), &poller);

    let loaded = load(&home, "owner/repo", "feat/test").unwrap();
    assert_eq!(loaded.gh_poll_failures, 1);
    assert!(loaded.last_gh_poll_at.is_some());
    let _ = std::fs::remove_dir_all(&home);
}

/// #986 T-load-bearing (reviewer #990 BLOCKING #1) — the actual
/// regression path that motivated #986: PrState in MergeReady
/// state but `pr_author=""` (placeholder value from `new_for_branch`
/// when ci_watch arms before any gh-poll). A typed receipt requires an exact,
/// non-zero PR subject, so this fixture starts with the already-known PR number.
/// After scan_and_emit_with applies gh-poll:
/// - pr_author populated via 4-tier resolution chain
/// - `[pr-ready-for-merge]` event enqueued to RESOLVED author with
///   the exact PR number in the body
///
/// Pre-#986: pre-poll state sat MergeReady forever, ready event
/// fired to subscribers[0] (fallback) with `repo@branch` body
/// instead of `repo#N`. Operator-visible: lead manual kick still
/// needed even with #972 aggregator merged.
#[test]
fn t14_gh_poll_promotes_unknown_author_to_ready_event() {
    // Build a state ALREADY MergeReady (CI green + 1×VERIFIED at
    // same sha) but with placeholder pr_author.
    let mut s = new_state("sha-A", ReviewClass::Single);
    s.pr_author = String::new();
    s.pr_number = 990;
    s.ci_state = CiState::Green {
        sha: "sha-A".to_string(),
        observed_at: now(),
    };
    observe_typed_verified(&mut s, "rev-1", "sha-A");
    assert_eq!(s.merge_state, MergeState::MergeReady);
    // #2749: this test pins author promotion + merge-authority routing, not
    // ancestry — stamp a fresh tuple so the freshness gate admits emission.
    stamp_fresh_ancestry(&mut s);
    // subscribers[0] is "dev" from fixture — but we want gh-poll
    // to win via tier 2 name match. Set up a fleet.yaml with a
    // "suzuke" instance that matches the gh author.login.
    let home = home_with_state("ready-promote", s);
    // gh author "suzuke" matches the fleet instance via tier-2 name match
    // (drives pr_author resolution); the team's orchestrator "lead-z" is
    // the #2059-#3 merge authority that the ready event routes to.
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  suzuke:\n    backend: claude\n  lead-z:\n    backend: claude\n\
             teams:\n  squad:\n    orchestrator: lead-z\n    members:\n      - suzuke\n",
    )
    .unwrap();
    std::fs::create_dir_all(home.join("inbox")).ok();

    // MockGhPoller returns metadata: PR #990, author "suzuke"
    // (matches the fleet instance via tier 2 name match), state=OPEN.
    let poller = MockGhPoller::new(vec![Ok(vec![gh_meta_open(990, "feat/test", "suzuke")])]);

    scan_and_emit_with(&home, &empty_registry(), &poller);

    // Post-scan state: author populated, ready event swept
    // (file persists because state is OPEN not Merged), ready event
    // enqueued.
    let loaded = load(&home, "owner/repo", "feat/test").expect("state persists post-scan");
    assert_eq!(loaded.pr_number, 990, "exact receipt subject stays stable");
    assert_eq!(
        loaded.pr_author, "suzuke",
        "pr_author resolved via tier-2 name match against fleet.yaml"
    );

    // #2059-#3: [pr-ready-for-merge] routes to the MERGE AUTHORITY
    // (the team orchestrator lead-z), NOT the resolved author. Body must
    // include the gh-poll PR number (repo#990) NOT the @branch placeholder.
    let msgs = crate::inbox::drain(&home, "lead-z");
    assert_eq!(
        msgs.len(),
        1,
        "exactly one [pr-ready-for-merge] to the team orchestrator (merge authority)"
    );
    assert_eq!(msgs[0].kind.as_deref(), Some("pr-ready-for-merge"));
    assert!(
        msgs[0].text.contains("owner/repo#990"),
        "event body must use gh-discovered PR number, not @branch placeholder: {}",
        msgs[0].text
    );

    // The resolved author (suzuke) and subscribers[0] ("dev") must NOT
    // receive the ready event — merge routing is decoupled from authorship.
    assert!(
        crate::inbox::drain(&home, "suzuke").is_empty(),
        "resolved author must NOT receive [pr-ready-for-merge] (merge authority routing)"
    );
    assert!(
        crate::inbox::drain(&home, "dev").is_empty(),
        "subscribers[0] fallback must NOT fire — routing goes to merge authority"
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// #986 T10 (reviewer MANDATORY idempotency) — same gh-poll output
/// applied twice does NOT double-emit / double-transition. Reducer
/// recomputes derived state; Merged terminal already swept on
/// first pass, so second pass has nothing to do.
#[test]
fn t13_idempotent_same_observation_no_double_emit() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    s.pr_author = String::new();
    let home = home_with_state("idempotent", s);
    let merged_meta = GhPrMetadata {
        number: 970,
        author_login: "dev".into(),
        head_ref: "feat/test".into(),
        is_cross_repository: false,
        is_draft: false,
        state: GhPrState::Merged,
        merged_at: Some("2026-05-20T04:17:09Z".to_string()),
        head_ref_oid: None,
        base_ref_oid: None,
    };
    // Two consecutive polls return the same metadata.
    let poller = MockGhPoller::new(vec![Ok(vec![merged_meta.clone()]), Ok(vec![merged_meta])]);

    scan_and_emit_with(&home, &empty_registry(), &poller);
    let msgs1 = crate::inbox::drain(&home, "dev");
    assert_eq!(msgs1.len(), 1, "first scan emits [pr-merged]");

    // Second scan — file already swept; no PrState files to poll.
    // Even if a stale file existed, the terminal state would be
    // sticky and the scanner wouldn't re-emit.
    scan_and_emit_with(&home, &empty_registry(), &poller);
    let msgs2 = crate::inbox::drain(&home, "dev");
    assert_eq!(msgs2.len(), 0, "second scan: file swept, no re-emit");
    let _ = std::fs::remove_dir_all(&home);
}

fn tmp_home_for_1002(tag: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("agend-1002-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();
    std::fs::create_dir_all(home.join("inbox")).ok();
    home
}

/// #1002 Phase 1 observability pin: a malformed pr-state JSON
/// file MUST emit a tracing::debug! message identifying the path
/// and parse error, rather than silently continuing.
///
/// Pre-fix code at `scan_and_emit_with` had:
///   let Ok(state): Result<PrState, _> = serde_json::from_str(&content)
///       else { continue; };
/// — the silent `continue` meant a corrupt file or a schema-skew
/// PrState was indistinguishable from "no files at all". This pin
/// catches the next regression where a silent skip masks a real
/// issue.
#[test]
#[tracing_test::traced_test]
fn t15_malformed_pr_state_file_emits_observability_trace() {
    let home = tmp_home_for_1002("t15-malformed");
    let dir = pr_state_dir(&home);
    std::fs::create_dir_all(&dir).unwrap();
    // Write a deliberately malformed pr-state JSON file.
    let bad_path = dir.join("malformed.json");
    std::fs::write(&bad_path, "{this is not json").unwrap();

    // MockGhPoller with no responses — apply_gh_poll's
    // read_dir/parse layer is exercised first and emits its own
    // trace; the scanner-loop layer also reads the same dir.
    let poller = MockGhPoller::new(vec![]);
    scan_and_emit_with(&home, &empty_registry(), &poller);

    // The malformed file path appears in tracing output via the
    // #1002 debug line. We don't pin the exact format (impl-detail)
    // but require:
    //   1. The new "#1002" tracing marker is present
    //   2. The malformed file's name is referenced
    assert!(
        logs_contain("#1002"),
        "scanner must emit a #1002-tagged observability trace on malformed file"
    );
    assert!(
        logs_contain("malformed.json"),
        "trace must identify the malformed file by name"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// t-…-17 B4 (codex m-…-425) — a MALFORMED/unreadable PrState file must NOT be able
/// to OPEN the merge gate through the PRODUCTION emit path. The reconciler workset now
/// enumerates live PrState identities from file CONTENT (`list_state_identities`); a
/// malformed file is SURFACED + skipped there. But the authoritative fail-closed
/// guarantee lives on the merge-ready EMIT path: `scan_and_emit_with` can never
/// deserialize a garbage file into a MergeReady `PrState`, so it emits NO
/// `[pr-ready-for-merge]` and stamps NO `ready_emitted_for_sha` (the file stays garbage;
/// `load` returns None ⇒ nothing is merge-ready). This is the REAL-PATH proof — not just
/// a comment — that an unreadable PrState keeps the gate CLOSED.
#[test]
#[tracing_test::traced_test]
fn b4_malformed_pr_state_cannot_open_merge_gate_fail_closed() {
    let home = tmp_home_for_1002("b4-malformed-gate");
    let dir = pr_state_dir(&home);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(home.join("inbox")).ok();
    // A team so that IF anything were (wrongly) emitted it WOULD route to the merge
    // authority and be drainable — making the "no emit" assertion meaningful.
    write_team_fleet(&home, "lead-w", &["dev"]);

    // A MALFORMED PrState at the canonical (repo,branch) path — the shape a torn write
    // or schema-skew leaves on disk. It must NEVER become merge-ready.
    let bad_path = dir.join(pr_state_filename("owner/repo", "feat/test"));
    std::fs::write(&bad_path, b"{ this is not valid pr_state json").unwrap();

    let poller = MockGhPoller::new(vec![]);
    scan_and_emit_with(&home, &empty_registry(), &poller);

    // Production load path: a malformed file is NOT loadable ⇒ there is no gatable state.
    assert!(
        load(&home, "owner/repo", "feat/test").is_none(),
        "a malformed PrState must not load into a gatable state (fail closed)"
    );
    // Production emit path: NO [pr-ready-for-merge] reached the merge-authority target.
    assert!(
        crate::inbox::drain(&home, "lead-w").is_empty(),
        "a malformed PrState must NOT emit [pr-ready-for-merge] — the merge gate stays CLOSED"
    );
    // The scanner skipped the malformed file WITHOUT mutating it (no ready_emitted_for_sha
    // could be stamped onto an unparseable file).
    assert_eq!(
        std::fs::read_to_string(&bad_path).unwrap(),
        "{ this is not valid pr_state json",
        "the scanner left the malformed file untouched"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #1002 Phase 1 observability pin: record_verdict's gate A
/// (reviewed_head is None) MUST emit a tracing::debug! identifying
/// the silent skip with the gate marker. Pre-fix, the early-return
/// at `let Some(reviewed_head) = reviewed_head else { return };`
/// silently swallowed the call; #982's verdict_state stuck at
/// None could not be bisected without code spelunking.
#[test]
#[tracing_test::traced_test]
fn t16_record_verdict_gate_a_emits_observability_trace() {
    let home = tmp_home_for_1002("t16-gate-a");
    // record_verdict with reviewed_head=None hits gate A
    // immediately — no fleet or pr-state setup required.
    record_verdict(
        &home,
        "t-fake-task-id",
        "fixup-reviewer",
        None,
        VerdictKind::Verified,
    );
    assert!(
        logs_contain("#1002"),
        "record_verdict must emit a #1002 observability trace on gate A"
    );
    assert!(logs_contain("gate A"), "trace must identify gate A by name");
    let _ = std::fs::remove_dir_all(&home);
}

// ─── #2 t-verdict-to-author-routing: verdict → author notification ───

fn verdict_home(tag: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("agend-verdict-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();
    home
}

fn seed_task_with_branch(home: &Path, id: &str, branch: &str) {
    use crate::task_events::{append, InstanceName, TaskEvent, TaskId};
    append(
        home,
        &InstanceName::from("test:lead"),
        TaskEvent::Created {
            task_id: TaskId(id.into()),
            title: "t".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: Some(branch.into()),
            bind: None,
            eta_secs: None,
            tags: vec![],
            parent_id: None,
        },
    )
    .expect("seed task");
}

fn bind_author(home: &Path, agent: &str, branch: &str) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("binding.json"),
        serde_json::to_string(&serde_json::json!({
            "version": 1, "agent": agent, "task_id": "t",
            "branch": branch, "worktree": "/tmp/wt",
            "source_repo": "owner/repo", "issued_at": "2026-06-05T00:00:00Z",
        }))
        .unwrap(),
    )
    .unwrap();
}

/// Write a pr-state for ("owner/repo", branch) at `head`. `green` makes CI
/// green+aligned. `pr_author` is set to a gh login ("suzuke") to prove the
/// binding resolution beats it under a shared account.
fn write_verdict_state(home: &Path, branch: &str, head: &str, green: bool) {
    let mut s = new_for_branch("owner/repo", branch, head, ReviewClass::Single);
    s.pr_author = "suzuke".to_string();
    if green {
        s.ci_state = CiState::Green {
            sha: head.to_string(),
            observed_at: "2026-06-05T00:00:00Z".to_string(),
        };
    }
    save(home, &s).unwrap();
}

fn has_verdict_msg(home: &Path, agent: &str) -> Option<String> {
    crate::inbox::drain(home, agent)
        .into_iter()
        .find(|m| m.text.contains("[review-verdict]"))
        .map(|m| m.text)
}

#[test]
fn verdict_verified_not_merge_ready_notifies_bound_author() {
    let home = verdict_home("verified-notready");
    seed_task_with_branch(&home, "t-v", "feat/x");
    bind_author(&home, "dev-x", "feat/x");
    write_verdict_state(&home, "feat/x", "headsha", false); // CI pending → not merge-ready
    record_verdict(
        &home,
        "t-v",
        "fixup-reviewer",
        Some("headsha"),
        VerdictKind::Verified,
    );
    let txt = has_verdict_msg(&home, "dev-x");
    assert!(
        txt.as_deref()
            .is_some_and(|t| t.contains("VERIFIED by fixup-reviewer")),
        "VERIFIED-not-merge-ready must notify the bound author: {txt:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn legacy_verified_green_stays_not_ready_and_notifies_2760() {
    let home = verdict_home("verified-ready");
    seed_task_with_branch(&home, "t-v", "feat/x");
    bind_author(&home, "dev-x", "feat/x");
    write_verdict_state(&home, "feat/x", "headsha", true); // green + aligned
    record_verdict(
        &home,
        "t-v",
        "fixup-reviewer",
        Some("headsha"),
        VerdictKind::Verified,
    );
    assert!(
        has_verdict_msg(&home, "dev-x").is_some(),
        "legacy VERIFIED never becomes merge-ready, so its display notification remains ordinary"
    );
    let state = load(&home, "owner/repo", "feat/x").unwrap();
    assert!(!is_merge_ready(&state));
    assert!(state.validated_review_receipts.is_empty());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn verdict_rejected_notifies_author_with_fix_hint() {
    let home = verdict_home("rejected");
    seed_task_with_branch(&home, "t-r", "feat/x");
    bind_author(&home, "dev-x", "feat/x");
    // Even green: REJECTED never reaches merge-ready, so it always notifies.
    write_verdict_state(&home, "feat/x", "headsha", true);
    record_verdict(
        &home,
        "t-r",
        "fixup-reviewer",
        Some("headsha"),
        VerdictKind::Rejected { reason: None },
    );
    let txt = has_verdict_msg(&home, "dev-x");
    assert!(
        txt.as_deref().is_some_and(
            |t| t.contains("REJECTED by fixup-reviewer") && t.contains("fix and re-push")
        ),
        "REJECTED must notify the author with the fix hint: {txt:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn verdict_unverified_notifies_author() {
    let home = verdict_home("unverified");
    seed_task_with_branch(&home, "t-u", "feat/x");
    bind_author(&home, "dev-x", "feat/x");
    write_verdict_state(&home, "feat/x", "headsha", false);
    record_verdict(
        &home,
        "t-u",
        "fixup-reviewer",
        Some("headsha"),
        VerdictKind::Unverified,
    );
    let txt = has_verdict_msg(&home, "dev-x");
    assert!(
        txt.as_deref().is_some_and(|t| t.contains("UNVERIFIED")),
        "UNVERIFIED must notify the author: {txt:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn verdict_recipient_is_bound_agent_not_fixup_lead_or_gh_login() {
    let home = verdict_home("recipient");
    seed_task_with_branch(&home, "t-rec", "feat/x");
    bind_author(&home, "dev-x", "feat/x");
    write_verdict_state(&home, "feat/x", "headsha", false); // pr_author="suzuke"
    record_verdict(
        &home,
        "t-rec",
        "fixup-reviewer",
        Some("headsha"),
        VerdictKind::Verified,
    );
    assert!(
        has_verdict_msg(&home, "dev-x").is_some(),
        "the BOUND agent must receive the verdict"
    );
    assert!(
        has_verdict_msg(&home, "fixup-lead").is_none(),
        "fixup-lead must NOT receive it (binding beats the resolve_author fallback)"
    );
    assert!(
        has_verdict_msg(&home, "suzuke").is_none(),
        "the gh-login pr_author must NOT receive it under a shared account"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn pr_ready_for_merge_routes_to_merge_authority_2059() {
    let home = verdict_home("prready-recipient");
    seed_task_with_branch(&home, "t-p", "feat/x");
    bind_author(&home, "dev-x", "feat/x");
    // #2059-#3: a team whose orchestrator is the merge authority. The bound
    // agent (dev-x) + the reviewer are members; the orchestrator (lead-x)
    // is who merges.
    write_team_fleet(&home, "lead-x", &["dev-x", "fixup-reviewer"]);
    write_verdict_state(&home, "feat/x", "headsha", true); // green, pr_author="suzuke"
                                                           // #2749: this test pins merge-authority ROUTING, not ancestry — stamp a
                                                           // fresh tuple on the now-MergeReady state so the freshness gate admits it.
    {
        let mut s = load(&home, "owner/repo", "feat/x").expect("merge-ready state present");
        s.pr_number = 42;
        observe_typed_verified(&mut s, "fixup-reviewer", "headsha");
        assert!(is_merge_ready(&s));
        stamp_fresh_ancestry(&mut s);
        save(&home, &s).unwrap();
    }
    let poller = MockGhPoller::new(vec![Ok(vec![])]);
    scan_and_emit_with(&home, &empty_registry(), &poller);
    // #2059-#3: merge-ready routes to the MERGE AUTHORITY (orchestrator),
    // NOT the bound agent — the old bound-agent routing (gap C) was the
    // PR #2058 mis-route (the implementer's binding is released post-push,
    // and even live it's the author, not the merge actor).
    assert!(
        crate::inbox::drain(&home, "lead-x")
            .iter()
            .any(|m| m.text.contains("[pr-ready-for-merge]")),
        "[pr-ready-for-merge] must route to the team orchestrator (merge authority)"
    );
    assert!(
        !crate::inbox::drain(&home, "dev-x")
            .iter()
            .any(|m| m.text.contains("[pr-ready-for-merge]")),
        "[pr-ready-for-merge] must NOT route to the bound implementer (the #2058 mis-route)"
    );
    assert!(
        !crate::inbox::drain(&home, "suzuke")
            .iter()
            .any(|m| m.text.contains("[pr-ready-for-merge]")),
        "[pr-ready-for-merge] must NOT route to the gh-login pr_author"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ─── #1017 startup-replay suppression ─────────────────────────────

/// #1017 T17: stale Merged terminal-state files (mtime older than
/// the fixed 1h replay-age threshold) MUST be marked
/// as already-emitted by `suppress_stale_terminal_replay` at boot.
/// The next `scan_and_emit_with` tick then sweeps (removes) the
/// file without firing the [pr-merged] event — closing the
/// daemon-restart noise flood the operator hit on 2026-05-20.
#[test]
fn t17_1017_stale_merged_suppressed_then_swept_without_emit() {
    let mut s = new_state("sha-A", ReviewClass::Single);
    s.merge_state = MergeState::Merged {
        merge_commit: "merge-sha-A".to_string(),
        merged_at: "2026-05-20T04:00:00Z".to_string(),
    };
    let home = home_with_state("1017-stale-merged", s);

    // Threshold ZERO simulates "any age counts as stale". Tests
    // would otherwise need an mtime-mutator dev-dep (filetime crate)
    // to age the file; using the test seam avoids that.
    suppress_stale_terminal_replay_with(&home, std::time::Duration::ZERO);

    // Verify the file body now has ready_emitted_for_sha == head.
    let after_suppress = load(&home, "owner/repo", "feat/test")
        .expect("file persists after suppress (only flag mutated)");
    assert_eq!(
        after_suppress.ready_emitted_for_sha.as_deref(),
        Some("sha-A"),
        "stale Merged must have ready_emitted_for_sha set by suppress hook"
    );

    // First scan after boot: file should be swept (removed) but
    // NO [pr-merged] event emitted to the inbox.
    let poller = MockGhPoller::new(vec![Ok(vec![])]);
    scan_and_emit_with(&home, &empty_registry(), &poller);
    assert!(
        load(&home, "owner/repo", "feat/test").is_none(),
        "stale Merged file MUST be swept post-scan"
    );
    let msgs = crate::inbox::drain(&home, "dev");
    assert!(
        msgs.iter().all(|m| m.kind.as_deref() != Some("pr-merged")),
        "stale Merged MUST NOT emit [pr-merged] — got: {:?}",
        msgs.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #1017 T18: a FRESH Merged terminal-state file (mtime within
/// the replay-age threshold) is NOT touched by the suppress hook,
/// and the next `scan_and_emit_with` tick fires the [pr-merged]
/// event normally. Anti-regression: makes sure the suppression
/// doesn't over-rotate into masking legitimate post-restart
/// merges.
#[test]
fn t18_1017_fresh_merged_still_emits_normally() {
    let mut s = new_state("sha-B", ReviewClass::Single);
    s.merge_state = MergeState::Merged {
        merge_commit: "merge-sha-B".to_string(),
        merged_at: "2026-05-20T22:00:00Z".to_string(),
    };
    let home = home_with_state("1017-fresh-merged", s);

    // Threshold u32::MAX simulates "nothing is stale" — pin that
    // the suppress hook leaves fresh terminal files untouched.
    suppress_stale_terminal_replay_with(&home, std::time::Duration::from_secs(u32::MAX as u64));
    let after_suppress =
        load(&home, "owner/repo", "feat/test").expect("file persists after suppress");
    assert_eq!(
        after_suppress.ready_emitted_for_sha, None,
        "fresh Merged MUST NOT have ready_emitted_for_sha set by suppress hook"
    );

    // #1287: first scan emits + persists dedup flag (no removal).
    let poller = MockGhPoller::new(vec![Ok(vec![])]);
    scan_and_emit_with(&home, &empty_registry(), &poller);
    let persisted = load(&home, "owner/repo", "feat/test")
        .expect("#1287: file survives first scan with dedup flag");
    assert_eq!(persisted.ready_emitted_for_sha.as_deref(), Some("sha-B"));
    let msgs = crate::inbox::drain(&home, "dev");
    assert_eq!(msgs.len(), 1, "fresh Merged MUST emit [pr-merged]");
    assert_eq!(msgs[0].kind.as_deref(), Some("pr-merged"));

    // Second scan: already_emitted → swept, no re-emit.
    let poller2 = MockGhPoller::new(vec![Ok(vec![])]);
    scan_and_emit_with(&home, &empty_registry(), &poller2);
    assert!(
        load(&home, "owner/repo", "feat/test").is_none(),
        "second scan must sweep terminal file"
    );
    let msgs2 = crate::inbox::drain(&home, "dev");
    assert!(msgs2.is_empty(), "#1287: no duplicate emit on second scan");
    let _ = std::fs::remove_dir_all(&home);
}

/// #1017 T19: ClosedUnmerged stale terminal state follows the
/// same suppression contract as Merged. Symmetry pin.
#[test]
fn t19_1017_stale_closed_unmerged_suppressed_without_emit() {
    let mut s = new_state("sha-C", ReviewClass::Single);
    s.merge_state = MergeState::ClosedUnmerged {
        closed_at: "2026-05-20T05:00:00Z".to_string(),
    };
    let home = home_with_state("1017-stale-closed", s);

    // Threshold ZERO = anything counts as stale (see T17 rationale).
    suppress_stale_terminal_replay_with(&home, std::time::Duration::ZERO);
    let poller = MockGhPoller::new(vec![Ok(vec![])]);
    scan_and_emit_with(&home, &empty_registry(), &poller);
    let msgs = crate::inbox::drain(&home, "dev");
    assert!(
        msgs.iter()
            .all(|m| m.kind.as_deref() != Some("pr-closed-unmerged")),
        "stale ClosedUnmerged MUST NOT emit [pr-closed-unmerged]"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #1017 T20 → #env-cleanup: the replay-age threshold is now a fixed 1h
/// const (the `AGEND_PR_STATE_REPLAY_AGE_HOURS` override was demoted).
#[test]
fn t20_1017_replay_age_threshold_is_fixed_1h() {
    assert_eq!(replay_age_threshold(), std::time::Duration::from_secs(3600));
}

/// #2059-#3: write a fleet.yaml team so `find_team_for` resolves.
fn write_team_fleet(home: &std::path::Path, orch: &str, members: &[&str]) {
    let mut y = String::from("instances:\n");
    for m in members {
        y.push_str(&format!("  {m}:\n    backend: claude\n"));
    }
    y.push_str(&format!(
        "teams:\n  squad:\n    orchestrator: {orch}\n    members:\n"
    ));
    for m in members {
        y.push_str(&format!("      - {m}\n"));
    }
    std::fs::write(crate::fleet::fleet_yaml_path(home), y).expect("write fleet.yaml");
}

/// #2059-#3: ready-for-merge resolves to the team ORCHESTRATOR via the
/// durable fleet.yaml teams config — from a reviewer / subscriber member,
/// NOT the branch binding.
#[test]
fn resolve_merge_authority_routes_to_orchestrator_2059() {
    let home = tmp_home_for_1002("merge-auth-orch");
    write_team_fleet(&home, "lead-x", &["dev-x", "rev-x"]);
    let mut s = new_state("sha-A", ReviewClass::Single);
    s.pr_author = "dev-x".to_string();
    s.subscribers = vec!["dev-x".to_string()];
    s.verdict_state = VerdictState::Verified {
        reviewers: vec![("rev-x".to_string(), "sha-A".to_string())],
    };
    assert_eq!(
        resolve_merge_authority(&home, &s),
        "lead-x",
        "merge-ready must route to the team orchestrator, not the author/reviewer"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2059-#3 CORE: with NO branch binding present (the implementer released
/// the worktree post-push — the PR #2058 case), merge-authority STILL
/// resolves to the orchestrator via teams; contrast `resolve_notify_
/// recipient`, which would fall through to the author here.
#[test]
fn resolve_merge_authority_no_binding_still_routes_2059() {
    let home = tmp_home_for_1002("merge-auth-nobind");
    write_team_fleet(&home, "lead-y", &["dev-y"]);
    let mut s = new_state("sha-B", ReviewClass::Single);
    s.branch = "feat/released".to_string();
    s.pr_author = "dev-y".to_string();
    s.subscribers = vec!["dev-y".to_string()];
    // No binding for feat/released → resolve_notify_recipient = author (dev-y);
    // resolve_merge_authority = orchestrator (lead-y).
    assert_eq!(
        resolve_notify_recipient(&home, &s),
        "dev-y",
        "control: author-facing falls to author"
    );
    assert_eq!(
        resolve_merge_authority(&home, &s),
        "lead-y",
        "merge-authority must NOT mis-route to the author when the binding is gone"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2063 de-hardcode follow-up: no team for any member (a single-agent /
/// no-team deployment) → the AUTHOR self-notifies (they merge their own PR),
/// NOT a literal `fixup-lead` that would route into the void on a deployment
/// with no fixup-lead instance.
#[test]
fn resolve_merge_authority_no_team_self_notifies_author_2059() {
    let home = tmp_home_for_1002("merge-auth-fallback");
    // No fleet.yaml teams written → find_team_for returns None for all.
    let mut s = new_state("sha-C", ReviewClass::Single);
    s.pr_author = "stranger".to_string();
    s.subscribers = vec!["stranger".to_string()];
    assert_eq!(
        resolve_merge_authority(&home, &s),
        "stranger",
        "no resolvable team → self-notify the author, not the void"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2063 de-hardcode follow-up: no team AND no known author → the last-ditch
/// `fixup-lead` default still applies (nothing better to route to).
#[test]
fn resolve_merge_authority_no_team_no_author_last_ditch_fixup_lead_2059() {
    let home = tmp_home_for_1002("merge-auth-lastditch");
    let mut s = new_state("sha-D", ReviewClass::Single);
    // Empty author + no subscribers + no verdict + no teams → nothing to
    // route to but the last-ditch default.
    s.pr_author = String::new();
    s.subscribers = vec![];
    assert_eq!(
        resolve_merge_authority(&home, &s),
        "fixup-lead",
        "no team and no author → last-ditch fixup-lead default"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ─────────────────── t-…-17 C13: reserved_assignments gate ───────────────────

/// T31: a RESERVED-but-unverified required reviewer holds `is_merge_ready` CLOSED,
/// yet reserved entries are NEVER counted toward `required_verified_count`. With
/// zero reserved + the threshold met ⇒ ready; add one reserved ⇒ NOT ready; a
/// reserved entry never substitutes for a real VERIFIED.
#[test]
fn t31_reserved_assignment_holds_merge_ready_closed() {
    let mut s = new_state("sha-A", ReviewClass::Dual);
    s.ci_state = CiState::Green {
        sha: "sha-A".into(),
        observed_at: now(),
    };
    observe_typed_verified(&mut s, "r1", "sha-A");
    observe_typed_verified(&mut s, "r2", "sha-A");
    // Baseline: Dual threshold met at head, zero reserved ⇒ merge-ready.
    assert!(
        is_merge_ready(&s),
        "2 VERIFIED@head + 0 reserved ⇒ merge-ready"
    );

    // Add one reserved-but-unverified required reviewer ⇒ gate holds it CLOSED.
    s.reserved_assignments = vec![ReservedAssignment {
        target: "r3".into(),
        review_author: crate::mcp::handlers::comms_gates::ReviewAuthor::External("octocat".into()),
        assignment_id: uuid::Uuid::new_v4(),
    }];
    assert!(
        !is_merge_ready(&s),
        "a reserved-but-unverified required reviewer holds merge-ready closed"
    );

    // A reserved entry NEVER counts toward the verified threshold: 1 real VERIFIED
    // + 1 reserved must NOT become "2" and go ready.
    s.validated_review_receipts
        .retain(|receipt| receipt.reviewer_name == "r1");
    apply(&mut s, Event::DraftTransition { is_draft: false });
    assert!(
        !is_merge_ready(&s),
        "reserved never increments required_verified_count"
    );
}

/// T36: additive serde — an empty `reserved_assignments` is SKIPPED on serialize
/// (legacy state files are byte-identical) and legacy JSON WITHOUT the field
/// deserializes back to an empty vec.
#[test]
fn t36_reserved_assignments_serde_legacy_byte_identical() {
    let s = new_state("sha-A", ReviewClass::Single);
    let json = serde_json::to_string(&s).unwrap();
    assert!(
        !json.contains("reserved_assignments"),
        "an empty reserved_assignments is skip_serializing_if ⇒ absent from the wire"
    );
    // Legacy JSON (no field) round-trips to an empty vec, equal to the original.
    let back: PrState = serde_json::from_str(&json).unwrap();
    assert!(
        back.reserved_assignments.is_empty(),
        "a legacy doc without the field deserializes to empty"
    );
    assert_eq!(s, back, "byte-identical round-trip");
}

// ── t-…-17 C10 / A6: the record_ci_result reserved-derivation DRAIN (T30) ──
// Real entry (record_ci_result). The pr_state's pr_number is pre-set (gh-poll fills
// it in production; here we set it directly) so the pr_number-matched drain fires.

fn t30_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "agend-t30-{}-{}-{}",
        tag,
        std::process::id(),
        C.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn t30_persist_asgn(home: &std::path::Path, repo: &str, branch: &str, target: &str, pr: u64) {
    let rec = crate::daemon::assignment_authority::ActiveAssignment::new_pending(
        repo,
        branch,
        target,
        pr,
        "lead",
        "t-rev-1",
        ReviewClass::Dual,
        crate::mcp::handlers::comms_gates::ReviewAuthor::External("octocat".into()),
        "review",
        None,
        None,
        "2026-07-13T00:00:00Z",
    );
    crate::daemon::assignment_authority::persist(home, &rec).unwrap();
}

#[allow(clippy::too_many_arguments)]
fn t30_persist_typed_asgn(
    home: &std::path::Path,
    repo: &str,
    branch: &str,
    target: &str,
    pr: u64,
    head: &str,
    class: ReviewClass,
    slot: crate::review_receipt::ReviewSlot,
) -> crate::daemon::assignment_authority::ActiveAssignment {
    let rec = crate::daemon::assignment_authority::ActiveAssignment::new_pending_typed(
        repo,
        branch,
        target,
        crate::types::InstanceId::new(),
        pr,
        head,
        slot,
        "lead",
        "t-rev-typed",
        class,
        crate::mcp::handlers::comms_gates::ReviewAuthor::External("octocat".into()),
        "review",
        None,
        None,
        "2026-07-13T00:00:00Z",
    );
    crate::daemon::assignment_authority::persist(home, &rec).unwrap();
    rec
}

/// T30 containment ordering: the drain sets `reserved_assignments` while a legacy
/// buffered verdict remains inert. The active legacy assignment stays reserved and
/// the PR remains closed; only the separate typed buffer may replay authority.
#[test]
fn t30_drain_reserved_before_verdict_replay_holds_merge_closed() {
    let home = t30_home("order");
    t30_persist_asgn(&home, "owner/repo", "feat/x", "reviewer", 42);
    let mut ps = new_for_branch("owner/repo", "feat/x", "sha-A", ReviewClass::Single);
    ps.pr_number = 42;
    save(&home, &ps).unwrap();
    // A legacy verdict is buffered BEFORE the CI observation.
    verdict_buffer::buffer(&home, "sha-A", "reviewer", "verified", None);

    record_ci_result(
        &home,
        "owner/repo",
        "feat/x",
        "sha-A",
        CiConclusion::Green,
        vec![],
        ReviewClass::Single,
    );

    let s = load(&home, "owner/repo", "feat/x").unwrap();
    assert_eq!(
        s.reserved_assignments
            .iter()
            .map(|r| r.target.clone())
            .collect::<Vec<_>>(),
        vec!["reviewer".to_string()],
        "the active legacy assignment remains reserved"
    );
    assert!(
        s.validated_review_receipts.is_empty(),
        "legacy buffered VERIFIED must not replay into typed receipt authority"
    );
    assert!(
        !is_merge_ready(&s),
        "a reserved required reviewer holds the (else-ready Single) PR CLOSED (I17)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// T30 (excludes Satisfied + foreign generation): the drain reserves ONLY active
/// records whose pr_number matches the state's generation AND whose evidence is not
/// SatisfiedExactHead.
#[test]
fn t30_drain_excludes_satisfied_and_foreign_generation() {
    let home = t30_home("excl");
    t30_persist_asgn(&home, "owner/repo", "feat/x", "reviewer", 42); // matching gen, unengaged
    let satisfied = t30_persist_typed_asgn(
        &home,
        "owner/repo",
        "feat/x",
        "satisfied",
        42,
        "sha-A",
        ReviewClass::Single,
        crate::review_receipt::ReviewSlot::Primary,
    );
    t30_persist_asgn(&home, "owner/repo", "feat/x", "other-gen", 99); // FOREIGN generation
    let mut ps = new_for_branch("owner/repo", "feat/x", "sha-A", ReviewClass::Single);
    ps.pr_number = 42;
    observe_assignment_verdict(
        &mut ps,
        &satisfied,
        crate::review_receipt::ReviewVerdict::Verified,
    );
    save(&home, &ps).unwrap();

    record_ci_result(
        &home,
        "owner/repo",
        "feat/x",
        "sha-A",
        CiConclusion::Green,
        vec![],
        ReviewClass::Single,
    );

    let mut got: Vec<String> = load(&home, "owner/repo", "feat/x")
        .unwrap()
        .reserved_assignments
        .iter()
        .map(|r| r.target.clone())
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec!["reviewer".to_string()],
        "reserved excludes the Satisfied target AND the foreign-generation record"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// B4(d) — when a branch HAS active assignments but the reviewer-assignment lock
/// CANNOT be acquired during the `record_ci_result` A6 drain, the reserved set must
/// NOT be re-derived LOCK-FREE. A lock-free derivation can race a concurrent
/// revoke/transfer and produce a torn reserved set that CLEARS the merge gate.
/// Fail closed: keep the existing (gate-closing) `reserved_assignments`; the per-tick
/// reconciler re-derives under the lock later. Pre-fix the drain overwrites
/// `reserved_assignments` unconditionally, dropping the pre-existing reservation.
#[test]
fn b4_drain_preserves_reserved_when_assignment_lock_unavailable() {
    let home = t30_home("b4-lock");
    // An active assignment for THIS generation ⇒ has_active == true.
    t30_persist_asgn(&home, "owner/repo", "feat/x", "reviewer", 42);
    // A pre-existing reserved entry (a DIFFERENT target the lock-free re-derive would
    // DROP — the fail-open the drain must not cause).
    let mut ps = new_for_branch("owner/repo", "feat/x", "sha-A", ReviewClass::Single);
    ps.pr_number = 42;
    ps.reserved_assignments = vec![ReservedAssignment {
        target: "ghost".into(),
        review_author: crate::mcp::handlers::comms_gates::ReviewAuthor::External("octocat".into()),
        assignment_id: uuid::Uuid::new_v4(),
    }];
    save(&home, &ps).unwrap();

    // POISON the branch assignment lock so acquisition returns None while has_active
    // is true: put a DIRECTORY where the lock file is opened, so the open-for-write
    // fails (EISDIR) deterministically — no wall clock, no thread race.
    let lock_path = crate::daemon::assignment_authority::branch_lock_path_for_test(
        &home,
        "owner/repo",
        "feat/x",
    );
    let _ = std::fs::remove_file(&lock_path);
    std::fs::create_dir_all(&lock_path).unwrap();

    record_ci_result(
        &home,
        "owner/repo",
        "feat/x",
        "sha-A",
        CiConclusion::Green,
        vec![],
        ReviewClass::Single,
    );

    let s = load(&home, "owner/repo", "feat/x").unwrap();
    assert_eq!(
        s.reserved_assignments
            .iter()
            .map(|r| r.target.clone())
            .collect::<Vec<_>>(),
        vec!["ghost".to_string()],
        "has_active && lock unavailable ⇒ the drain must PRESERVE the existing reserved set (fail closed), not re-derive it lock-free"
    );
    // Cleanup: the poisoned lock dir would block remove_dir_all's file unlink? it's a
    // dir, remove_dir_all handles it.
    let _ = std::fs::remove_dir_all(&home);
}

/// t-…-17 B4 (codex m-…-322) — the SOLE-corrupt-record merge-gate fail-open. A branch
/// whose ONLY assignment record is CORRUPT, with NO pre-existing reservation, must NOT
/// be able to OPEN the merge gate. The lossy `has_active`/`list_active` DROP a corrupt
/// record's `Err`, so the branch looks assignment-free: the drain then derives on a
/// fresh state with an EMPTY reserved set and `is_merge_ready` OPENs even though the
/// authority is UNREADABLE. Fail closed: the tri-state probe reports `Unreadable`, the
/// drain SETs `authority_unknown`, and `is_merge_ready` returns false. This complements
/// the reconcile-path RED (assignment_reconcile.rs
/// `b4_corrupt_record_keeps_reservation_fail_closed`, which covers a prior reservation +
/// a co-resident healthy record); THIS covers the uncovered first-projection /
/// single-corrupt case with an empty reserved set.
#[test]
fn b4_sole_corrupt_record_holds_merge_gate_closed_fail_closed() {
    let home = t30_home("b4-sole-corrupt");
    // The SOLE assignment record for (repo,branch) is CORRUPT — write garbage at the
    // real on-disk record path (create the branch dir first). No healthy record, so the
    // lossy `has_active`/`list_active` see an assignment-free branch.
    let rpath = crate::daemon::assignment_authority::record_path_for_test(
        &home,
        "owner/repo",
        "feat/x",
        "reviewer",
    );
    std::fs::create_dir_all(rpath.parent().unwrap()).unwrap();
    std::fs::write(&rpath, b"{ not valid assignment json").unwrap();

    // A FRESH PrState with EVERY OTHER merge condition satisfied: CI is set Green at
    // head by `record_ci_result` below; the reviewer is Verified at head; not Draft;
    // review_class resolved (Single). reserved_assignments is empty (no prior
    // reservation) — the exact state the fail-open OPENs the gate on.
    let mut ps = new_for_branch("owner/repo", "feat/x", "sha-A", ReviewClass::Single);
    ps.pr_number = 42;
    observe_typed_verified(&mut ps, "reviewer", "sha-A");
    assert!(ps.reserved_assignments.is_empty(), "no prior reservation");
    assert!(!ps.authority_unknown, "authority_unknown starts clear");
    save(&home, &ps).unwrap();

    // Real entry: the A6 drain runs against the sole corrupt record.
    record_ci_result(
        &home,
        "owner/repo",
        "feat/x",
        "sha-A",
        CiConclusion::Green,
        vec![],
        ReviewClass::Single,
    );

    let s = load(&home, "owner/repo", "feat/x").unwrap();
    // Sanity: every OTHER condition is met, so ONLY the authority-unknown gate keeps
    // the PR closed — proving this is the sole-corrupt fail-open, not some other gate.
    assert!(
        matches!(s.ci_state, CiState::Green { ref sha, .. } if sha == "sha-A"),
        "CI is green at head"
    );
    assert!(
        matches!(s.verdict_state, VerdictState::Verified { .. }),
        "reviewer is Verified at head"
    );
    assert!(s.reserved_assignments.is_empty(), "reserved stays empty");
    assert!(
        s.authority_unknown,
        "a SOLE corrupt record ⇒ authority UNREADABLE ⇒ authority_unknown SET (fail closed)"
    );
    assert!(
        !is_merge_ready(&s),
        "a branch whose sole assignment record is corrupt must NOT be merge-ready (gate stays CLOSED)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// t-…-17 B4 (codex m-…-479) RED (c) — the REDUCER-side cached-`merge_state`
/// recompute. `record_ci_result` applies `Event::CiObserved` (which derives
/// `merge_state`) BEFORE the A6 authority drain sets `reserved_assignments`, so
/// pre-fix the CACHED `merge_state` stays `MergeReady` even though the just-added
/// reservation makes `is_merge_ready` false — a stale `MergeReady` the scanner would
/// emit `[pr-ready-for-merge]` on, bypassing the fail-closed gate. The shared
/// `apply_authority_transition` now recomputes nonterminal `merge_state` as its FINAL
/// step, so a same-head CI observation whose drain adds a reservation persists
/// `merge_state == NotReady`. Repair-convergence: once the reservation is revoked, a
/// subsequent `record_ci_result` re-derives back to `MergeReady` (bidirectional — the
/// recompute is not a one-way latch).
#[test]
fn b4_record_ci_result_recomputes_cached_merge_state_after_reservation_drain() {
    let home = t30_home("b4-479-reducer");
    // An active assignment for a SEPARATE, still-unverified reviewer on THIS generation
    // ⇒ the A6 drain reserves it (it is NOT the verified reviewer below, so `classify`
    // does not mark it SatisfiedExactHead and exclude it).
    t30_persist_asgn(&home, "owner/repo", "feat/x", "reviewer", 42);
    // A state otherwise fully ready at sha-A: a DIFFERENT reviewer VERIFIED at head
    // satisfies the Single threshold, not draft; CI is set Green at head by
    // record_ci_result below. reserved starts empty, so the pre-drain CiObserved
    // derivation computes MergeReady (the stale value the drain must recompute away).
    let mut ps = new_for_branch("owner/repo", "feat/x", "sha-A", ReviewClass::Single);
    ps.pr_number = 42;
    observe_typed_verified(&mut ps, "verifier", "sha-A");
    save(&home, &ps).unwrap();

    record_ci_result(
        &home,
        "owner/repo",
        "feat/x",
        "sha-A",
        CiConclusion::Green,
        vec![],
        ReviewClass::Single,
    );

    let s = load(&home, "owner/repo", "feat/x").unwrap();
    assert_eq!(
        s.reserved_assignments
            .iter()
            .map(|r| r.target.clone())
            .collect::<Vec<_>>(),
        vec!["reviewer".to_string()],
        "the A6 drain reserved the active record"
    );
    assert!(
        !is_merge_ready(&s),
        "a reserved required reviewer ⇒ is_merge_ready false"
    );
    // THE RED: pre-fix, merge_state stayed the stale MergeReady derived BEFORE the
    // drain; the recompute must persist NotReady.
    assert_eq!(
        s.merge_state,
        MergeState::NotReady,
        "codex m-…-479: apply_authority_transition must recompute the cached merge_state to \
         NotReady after the drain adds a reservation (pre-fix it stayed a stale MergeReady)"
    );

    // Repair-convergence: revoke the reservation (remove the sole active record), then a
    // subsequent same-head CI observation must re-derive merge_state back to MergeReady —
    // proving the recompute is bidirectional (not a one-way latch that only closes).
    let rpath = crate::daemon::assignment_authority::record_path_for_test(
        &home,
        "owner/repo",
        "feat/x",
        "reviewer",
    );
    std::fs::remove_file(&rpath).unwrap();
    record_ci_result(
        &home,
        "owner/repo",
        "feat/x",
        "sha-A",
        CiConclusion::Green,
        vec![],
        ReviewClass::Single,
    );
    let s2 = load(&home, "owner/repo", "feat/x").unwrap();
    assert!(
        s2.reserved_assignments.is_empty(),
        "reservation revoked ⇒ reserved empty"
    );
    assert!(
        !s2.authority_unknown,
        "record removed ⇒ authority Absent ⇒ authority_unknown CLEARED"
    );
    assert!(
        is_merge_ready(&s2),
        "the otherwise-ready state is merge-ready again once the reservation is gone"
    );
    assert_eq!(
        s2.merge_state,
        MergeState::MergeReady,
        "repair-convergence: the recompute restores MergeReady when readiness returns (bidirectional)"
    );
    let _ = std::fs::remove_dir_all(&home);
}
