//! Test module for `daemon::auto_release`, re-homed verbatim from the inline
//! `mod tests` in `auto_release.rs` so that production file stays under the
//! `src_file_size_invariant` anti-monolith ceiling (it sat at exactly 2500 LOC,
//! leaving no room for the #3005 change).
//!
//! Wired via `#[cfg(test)] #[path = "auto_release_tests.rs"] mod tests;`, which
//! keeps the module named `tests` — every test retains its original
//! `daemon::auto_release::tests::*` path and `cargo test` filter behavior.

use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

fn tmp_home(tag: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "agend-test-auto-release-{tag}-{}-{id}",
        std::process::id()
    ))
}

fn sample_intent(task_id: &str) -> AutoReleaseIntent {
    AutoReleaseIntent {
        task_id: task_id.to_string(),
        reviewer: "reviewer-1".to_string(),
        verdict_msg_id: Some("m-test".to_string()),
        reviewed_head: Some("deadbeef".to_string()),
        enqueued_at: chrono::Utc::now().to_rfc3339(),
        event_kind: None,
        repo: None,
        branch: None,
        lease: None,
        unknown_retry_after: None,
    }
}

fn sample_task(id: &str, assignee: Option<&str>) -> crate::tasks::Task {
    crate::tasks::Task {
        id: id.to_string(),
        title: "t".into(),
        description: "d".into(),
        status: crate::task_events::TaskStatus::Claimed,
        priority: crate::task_events::TaskPriority::Normal,
        assignee: assignee.map(String::from),
        routed_to: None,
        created_by: "lead".into(),
        depends_on: vec![],
        result: None,
        created_at: "2026-05-17T00:00:00Z".into(),
        updated_at: "2026-05-17T00:00:00Z".into(),
        due_at: None,
        branch: None,
        started_at: None,
        eta_secs: None,
        auto_release_on_verdict: None,
        tags: vec![],
        parent_id: None,
        metadata: std::collections::BTreeMap::new(),
    }
}

/// A report carrying the server-validated receipt matches; visible text is
/// irrelevant to release authority after validation.
#[test]
fn verdict_send_pattern_matches_canonical_verdict() {
    let mut msg = canonical_verdict_message();
    assert!(is_verdict_message(&msg), "canonical verdict must match");
    // A later display-text rewrite cannot revoke the typed authority.
    msg.text = "   VERIFIED — all green".into();
    assert!(
        is_verdict_message(&msg),
        "visible text is display-only here"
    );
}

/// All visible verdict spellings remain release-eligible only because the
/// same validated receipt is present, not because their words are parsed.
#[test]
fn all_terminal_verdicts_match_2010() {
    let base = canonical_verdict_message();
    for verdict in ["VERIFIED", "REJECTED", "UNVERIFIED"] {
        let mut m = base.clone();
        m.text = format!("{verdict} — evidence block follows");
        assert!(
            is_verdict_message(&m),
            "{verdict} report must enqueue a release intent (#2010 2a)"
        );
        // Leading whitespace tolerated via trim_start.
        m.text = format!("   {verdict} — indented");
        assert!(is_verdict_message(&m), "{verdict} with indent must match");
    }
}

/// #2059: the strip is idempotent — a BARE verdict (a raw `send` report with
/// no `[report_result] ` wrapper) still matches, and a non-verdict report is
/// NOT a false positive. (The PRODUCER-FED fixture — feeding the real
/// `build_report_text` output through this matcher — lives in
/// `comms_inbox.rs` next to the producer, #1493 discipline.)
#[test]
fn strip_report_wrapper_idempotent_and_no_false_positive_2059() {
    // Bare verdict (unwrapped) still resolves.
    assert!(is_terminal_verdict_text("VERIFIED — all green"));
    assert_eq!(strip_report_wrapper("VERIFIED — x"), "VERIFIED — x");
    // Wrapper stripped to the bare word (incl. the trailing space).
    assert_eq!(
        strip_report_wrapper("[report_result] VERIFIED — x"),
        "VERIFIED — x"
    );
    // A non-verdict report must NOT be a false positive (wrapped or bare).
    assert!(!is_terminal_verdict_text(
        "[report_result] Done — pushed PR #2058"
    ));
    assert!(!is_terminal_verdict_text("just a plain message"));
}

/// Only kind=report plus a validated receipt selects release. Prose,
/// reviewed_head, and correlation_id do not participate.
#[test]
fn non_verdict_kinds_skip_detection() {
    let base = canonical_verdict_message();
    let mut m = base.clone();
    m.text = "looks good to me, merging".into();
    assert!(
        is_verdict_message(&m),
        "receipt, not prose, is authoritative"
    );
    m = base.clone();
    m.kind = Some("task".into());
    assert!(!is_verdict_message(&m), "kind=task must not match");
    m = base.clone();
    m.kind = Some("update".into());
    assert!(!is_verdict_message(&m), "kind=update must not match");
    m = base.clone();
    m.text = "REJECTED — r1 needed".into();
    m.reviewed_head = None;
    assert!(
        is_verdict_message(&m),
        "top-level reviewed_head is display-only"
    );
    m = base.clone();
    m.text = "UNVERIFIED — re-run CI".into();
    m.correlation_id = None;
    assert!(
        is_verdict_message(&m),
        "receipt carries the exact task; correlation is routing/display only"
    );
    m.validated_code_review = None;
    assert!(
        !is_verdict_message(&m),
        "missing validated receipt must not match"
    );
}

/// #2010 2a §3.9 — the reviewer-binding release bypass: all FOUR conditions
/// (verdict intent + bound agent IS the verdict sender + reviewer fleet role
/// + review task terminal) must hold; dropping any one keeps the binding.
#[test]
fn reviewer_binding_bypass_requires_all_four_conditions_2010() {
    use crate::task_events::TaskStatus;
    let mut intent = sample_intent("t-rev");
    intent.event_kind = Some("verdict".to_string());
    intent.reviewer = "reviewer-1".to_string();
    let rev = Some("reviewer"); // fleet role of the verdict sender

    let mut done_task = sample_task("t-rev", Some("reviewer-1"));
    done_task.status = TaskStatus::Done;
    // All four hold → bypass (REJECTED/UNVERIFIED/VERIFIED all enqueue a
    // "verdict" intent, so the kind is irrelevant here).
    assert!(
        reviewer_binding_release_bypass(&intent, Some(&done_task), "reviewer-1", rev),
        "all four conditions → bypass the open-PR invariant"
    );
    // Cancelled is also terminal; descriptive template role still counts.
    let mut cancelled = done_task.clone();
    cancelled.status = TaskStatus::Cancelled;
    assert!(
        reviewer_binding_release_bypass(
            &intent,
            Some(&cancelled),
            "reviewer-1",
            Some("Code reviewer — independent review")
        ),
        "cancelled review task + descriptive reviewer role → bypass"
    );

    // (1) not a verdict intent (merge/task_done event) → no bypass.
    let mut merge_intent = intent.clone();
    merge_intent.event_kind = Some("merge".to_string());
    assert!(
        !reviewer_binding_release_bypass(&merge_intent, Some(&done_task), "reviewer-1", rev),
        "non-verdict event must not bypass (only the verdict-sender path does)"
    );

    // (2) bound agent is NOT the verdict sender → no bypass.
    assert!(
        !reviewer_binding_release_bypass(&intent, Some(&done_task), "other-agent", rev),
        "binding whose agent != verdict sender must NOT bypass"
    );

    // (4) verdict sender's role is NOT a reviewer → no bypass. This is the
    // #2010 codex-r1 gate: an implementer's self-verdict never bypasses.
    assert!(
        !reviewer_binding_release_bypass(&intent, Some(&done_task), "reviewer-1", None),
        "no role → not a reviewer → no bypass"
    );
    assert!(
        !reviewer_binding_release_bypass(
            &intent,
            Some(&done_task),
            "reviewer-1",
            Some("Implementer — build features")
        ),
        "implementer role must NOT bypass"
    );

    // (3) review task not terminal (still claimed/in_review) → no bypass yet.
    let claimed = sample_task("t-rev", Some("reviewer-1")); // default Claimed
    assert!(
        !reviewer_binding_release_bypass(&intent, Some(&claimed), "reviewer-1", rev),
        "non-terminal review task must not bypass (retry until done)"
    );
    // Missing task → no bypass.
    assert!(
        !reviewer_binding_release_bypass(&intent, None, "reviewer-1", rev),
        "missing task must not bypass"
    );
}

/// PR-D · D2 (#2711) equivalence pin: `should_release_now` — the classifier
/// delegation — must reproduce the pre-D2 inline gate
/// `matches!(Release) || (matches!(SkipDirtyWorktree) && releasable)`
/// byte-for-byte on EVERY reachable `(decision, releasable, reviewer_bypassed)`.
///
/// Reachability at the gate (`process_intent`): the code returns `Retry` before
/// the gate unless `releasable || reviewer_bypassed`, and `reviewer_bypassed` is
/// only ever set inside the `!releasable` block — so at the gate EXACTLY ONE of
/// the two is true. The reviewer-bypass arm (releasable=false) is where the
/// classifier ALONE would drift: it keeps a clean reviewer worktree the pre-D2
/// gate released, so the explicit bypass arm restores it — and must NOT release
/// a dirty one (review-WIP protection). This pin locks BOTH directions. (The
/// two unreachable combos — (false,false) already returned Retry; (true,true)
/// can't arise since bypass ⟹ !releasable — are deliberately not asserted.)
#[test]
fn should_release_now_equals_pre_d2_gate() {
    // The pre-D2 inline expression, verbatim (main 8ddf26f1) — the baseline.
    fn pre_d2(decision: &ReleaseDecision, releasable: bool) -> bool {
        matches!(decision, ReleaseDecision::Release)
            || (matches!(decision, ReleaseDecision::SkipDirtyWorktree) && releasable)
    }
    let decisions = [
        ReleaseDecision::Release,
        ReleaseDecision::SkipDirtyWorktree,
        ReleaseDecision::SkipOptOut,
        ReleaseDecision::SkipNotBound,
        ReleaseDecision::SkipNoAssignee,
        ReleaseDecision::SkipTaskMissing,
    ];
    for d in &decisions {
        // Reachable gate states: exactly one of {releasable, reviewer_bypassed}.
        for &(releasable, reviewer_bypassed) in &[(true, false), (false, true)] {
            assert_eq!(
                should_release_now(d, releasable, reviewer_bypassed),
                pre_d2(d, releasable),
                "release_now DRIFT: decision={d:?} releasable={releasable} \
                     reviewer_bypassed={reviewer_bypassed}",
            );
        }
    }
}

/// #2010 codex-r1 §3.9 — the implementer self-verdict EXPLOIT must not
/// bypass. An implementer opens a report with "VERIFIED" on its OWN task
/// (correlation = own task); the #1228 reporter==assignee auto-close marks
/// that task Done in the same message — so conditions 1 (verdict intent),
/// 2 (`intent.reviewer == assignee`, since the implementer verdicts its own
/// task) and 3 (task Done) ALL hold. Only condition 4 (the fleet role gate)
/// stops the implementer's binding from releasing on an open PR.
#[test]
fn implementer_self_verdict_does_not_bypass_2010_r1() {
    use crate::task_events::TaskStatus;
    let mut intent = sample_intent("t-dev");
    intent.event_kind = Some("verdict".to_string());
    intent.reviewer = "dev-1".to_string(); // the verdict SENDER is the implementer

    let mut self_done = sample_task("t-dev", Some("dev-1")); // own task, assignee == sender
    self_done.status = TaskStatus::Done; // #1228 auto-closed it

    // Conditions 1–3 all hold (the exploit shape). Role gate must veto it.
    assert!(
        !reviewer_binding_release_bypass(&intent, Some(&self_done), "dev-1", None),
        "implementer self-verdict (no reviewer role) must NOT release its own \
             binding on an open PR"
    );
    assert!(
        !reviewer_binding_release_bypass(
            &intent,
            Some(&self_done),
            "dev-1",
            Some("Implementer — build features, run tests")
        ),
        "implementer role string must NOT satisfy the reviewer gate"
    );
    // #2010 codex-r2: an implementer whose description (serde alias for role)
    // MENTIONS a review activity must still be rejected — the old
    // contains("review") gate let exactly this revive the bypass.
    assert!(
        !reviewer_binding_release_bypass(
            &intent,
            Some(&self_done),
            "dev-1",
            Some("Implementer — build features and submit changes for review")
        ),
        "an implementer description mentioning 'review' must NOT bypass (codex-r2)"
    );
}

/// `is_reviewer_role` matches both production reviewer role shapes by exact
/// form and rejects implementer / orchestrator / absent roles — INCLUDING an
/// implementer description that merely MENTIONS a review activity (the #2010
/// codex-r2 counter-probe: a bare `contains("review")` let it through).
#[test]
fn is_reviewer_role_exact_forms_only_2010_r2() {
    // Accept: the two real reviewer shapes (the 3 live fixup reviewers are
    // exactly `reviewer`; the deploy template is `Code reviewer — …`).
    assert!(is_reviewer_role(Some("reviewer")), "fixup-team short tag");
    assert!(is_reviewer_role(Some("REVIEWER")), "case-insensitive");
    assert!(is_reviewer_role(Some("  reviewer  ")), "trimmed");
    assert!(
        is_reviewer_role(Some(
            "Code reviewer — independent review from a non-Claude vantage, \
                 verdicts VERIFIED/REJECTED/UNVERIFIED"
        )),
        "template descriptive role (exact production string)"
    );

    // Reject: implementer / orchestrator, incl. ones that mention review.
    assert!(
        !is_reviewer_role(Some(
            "Implementer — build features and submit changes for review"
        )),
        "#2010 codex-r2: an implementer description mentioning a review \
             ACTIVITY must NOT pass (this revived the self-verdict bypass under \
             the old contains(\"review\") gate)"
    );
    assert!(!is_reviewer_role(Some(
        "Implementer — pick up tasks from the board, build features, run tests"
    )));
    assert!(!is_reviewer_role(Some(
        "Team orchestrator — break work into tasks, dispatch, gate merges after reviewer approval"
    )));
    assert!(!is_reviewer_role(None), "no role → not a reviewer");
    assert!(!is_reviewer_role(Some("")), "empty role → not a reviewer");
}

/// `enqueue_intent` is atomic: temp file is renamed into place;
/// after success no `.tmp` file remains in the queue dir.
#[test]
fn enqueue_intent_writes_atomic_file() {
    let home = tmp_home("enqueue");
    std::fs::create_dir_all(&home).unwrap();
    let intent = sample_intent("t-enqueue-1");
    enqueue_intent(&home, &intent).expect("enqueue");
    let final_path = queue_dir(&home).join("t-enqueue-1.json");
    assert!(
        final_path.exists(),
        "final intent file must exist after enqueue"
    );
    // No `.tmp` file left behind.
    let stragglers: Vec<_> = std::fs::read_dir(queue_dir(&home))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
        .collect();
    assert!(
        stragglers.is_empty(),
        "no .tmp stragglers should remain after atomic rename"
    );
    // Round-trip the body.
    let body = std::fs::read_to_string(&final_path).unwrap();
    let parsed: AutoReleaseIntent = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed, intent);
    let _ = std::fs::remove_dir_all(&home);
}

/// Tracker throttle: TICKS_PER_SCAN=3 means tick 1+2 return false,
/// tick 3 fires (returns true), tick 4 resets to false.
#[test]
fn tracker_throttles_to_tick_per_scan() {
    let home = tmp_home("throttle");
    std::fs::create_dir_all(&home).unwrap();
    let mut tracker = AutoReleaseTracker::default();
    for i in 0..(TICKS_PER_SCAN - 1) {
        assert!(
            !tracker.maybe_scan(&home),
            "tick {i} (pre-throttle) must return false"
        );
    }
    assert!(
        tracker.maybe_scan(&home),
        "{TICKS_PER_SCAN}th tick must fire and return true"
    );
    assert!(
        !tracker.maybe_scan(&home),
        "post-fire tick must reset counter and return false"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// `decide_release`: dirty worktree → `SkipDirtyWorktree`
/// regardless of opt-out / assignee presence (operator WIP
/// protection takes precedence over the release decision).
#[test]
fn decide_release_skips_dirty_worktree() {
    let task = sample_task("t-1", Some("dev-1"));
    let binding = serde_json::json!({ "worktree": "/tmp/x" });
    let d = decide_release(Some(&task), Some(&binding), Some(true));
    assert_eq!(d, ReleaseDecision::SkipDirtyWorktree);
}

/// `decide_release`: explicit `Some(false)` opt-out flag short-
/// circuits even when assignee + binding + clean tree present.
#[test]
fn decide_release_skips_opt_out_flag() {
    let mut task = sample_task("t-2", Some("dev-2"));
    task.auto_release_on_verdict = Some(false);
    let binding = serde_json::json!({ "worktree": "/tmp/y" });
    let d = decide_release(Some(&task), Some(&binding), Some(false));
    assert_eq!(d, ReleaseDecision::SkipOptOut);
}

/// `decide_release`: happy path — task + assignee + binding +
/// clean tree all green, decision is `Release`.
#[test]
fn decide_release_happy_path() {
    let task = sample_task("t-3", Some("dev-3"));
    let binding = serde_json::json!({ "worktree": "/tmp/z" });
    let d = decide_release(Some(&task), Some(&binding), Some(false));
    assert_eq!(d, ReleaseDecision::Release);
}

/// `decide_release`: missing task → drop intent.
#[test]
fn decide_release_skips_missing_task() {
    let d = decide_release(None, None, None);
    assert_eq!(d, ReleaseDecision::SkipTaskMissing);
}

/// `decide_release`: task has no assignee → nothing to release.
#[test]
fn decide_release_skips_no_assignee() {
    let task = sample_task("t-4", None);
    let d = decide_release(Some(&task), None, None);
    assert_eq!(d, ReleaseDecision::SkipNoAssignee);
}

/// `decide_release`: assignee present but binding gone (already
/// released, never bound) → idempotent skip.
#[test]
fn decide_release_skips_not_bound() {
    let task = sample_task("t-5", Some("dev-5"));
    let d = decide_release(Some(&task), None, None);
    assert_eq!(d, ReleaseDecision::SkipNotBound);
}

/// `drain_queue` drops a file whose JSON is malformed and emits
/// a warn — but the file is removed so the tracker doesn't keep
/// retrying the same broken record (poison-message handling).
#[test]
fn drain_queue_drops_malformed_intents() {
    let home = tmp_home("malformed");
    std::fs::create_dir_all(queue_dir(&home)).unwrap();
    let bad_path = queue_dir(&home).join("garbage.json");
    std::fs::write(&bad_path, b"{not json").unwrap();
    drain_queue(&home);
    assert!(
        !bad_path.exists(),
        "malformed intent must be dropped on drain"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #1244: no binding for branch → silent no-op.
#[test]
fn auto_release_on_merge_no_binding_is_noop() {
    let home = tmp_home("1244-no-bind");
    std::fs::create_dir_all(crate::paths::runtime_dir(&home)).unwrap();
    auto_release_for_merged_branch(&home, "owner/repo", "feat/gone");
    // No panic, no crash — silent skip.
    let _ = std::fs::remove_dir_all(&home);
}

/// #1244: dirty worktree → skip auto-release, binding preserved.
#[test]
fn auto_release_on_merge_skips_dirty_worktree() {
    let home = tmp_home("1244-dirty");
    let agent = "dev-dirty";
    let branch = "feat/dirty-branch";
    let rt = crate::paths::runtime_dir(&home).join(agent);
    std::fs::create_dir_all(&rt).unwrap();
    // Create a real git worktree dir with uncommitted changes
    let wt = std::env::temp_dir().join(format!("agend-test-1244-dirty-wt-{}", std::process::id()));
    std::fs::create_dir_all(&wt).unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&wt)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&wt)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "test"])
        .current_dir(&wt)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    // Create initial commit so git status works
    std::fs::write(wt.join("initial.txt"), "init").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&wt)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&wt)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    // Now create an uncommitted file → dirty
    std::fs::write(wt.join("dirty.txt"), "uncommitted").unwrap();
    let binding = serde_json::json!({
        "version": 1,
        "branch": branch,
        "task_id": "t-test",
        "worktree": wt.to_str().unwrap(),
    });
    std::fs::write(
        rt.join("binding.json"),
        serde_json::to_string_pretty(&binding).unwrap(),
    )
    .unwrap();
    auto_release_for_merged_branch(&home, "owner/repo", branch);
    assert!(
        rt.join("binding.json").exists(),
        "binding.json must be preserved when worktree is dirty"
    );
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&wt);
}

/// t-worktree-leak (PR-1): merge no longer releases directly — it ENQUEUES a
/// release-invariant recompute intent (event_kind="merge") carrying the CAS
/// lease snapshot, which the sweeper processes. (The full enqueue→sweep→
/// release path is covered by the invariant tests below.)
#[test]
fn auto_release_on_merge_enqueues_recompute_intent() {
    let home = tmp_home("1244-release");
    // Real source repo whose origin resolves to "owner/repo" so the codex ①a
    // cross-repo guard (binding repo == event repo) passes.
    let repo = itest_source_repo(&home, "owner/repo");
    let agent = "dev-merge";
    let branch = "feat/merged-branch";
    let rt = crate::paths::runtime_dir(&home).join(agent);
    std::fs::create_dir_all(&rt).unwrap();
    let binding = serde_json::json!({
        "version": 1,
        "branch": branch,
        "task_id": "t-test",
        "worktree": "",
        "source_repo": repo.to_str().unwrap(),
        "issued_at": "2026-06-05T00:00:00Z",
    });
    std::fs::write(
        rt.join("binding.json"),
        serde_json::to_string_pretty(&binding).unwrap(),
    )
    .unwrap();
    auto_release_for_merged_branch(&home, "owner/repo", branch);
    let queued = std::fs::read_dir(queue_dir(&home))
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    assert_eq!(queued, 1, "merge must enqueue exactly one recompute intent");
    let content = std::fs::read_to_string(queue_dir(&home).join("t-test.json")).unwrap();
    let intent: AutoReleaseIntent = serde_json::from_str(&content).unwrap();
    assert_eq!(intent.event_kind.as_deref(), Some("merge"));
    assert_eq!(intent.branch.as_deref(), Some(branch));
    assert!(
        intent.lease.is_some(),
        "merge intent must carry the CAS lease snapshot"
    );
    let _ = std::fs::remove_dir_all(&home);
}

fn canonical_verdict_message() -> crate::inbox::InboxMessage {
    let mut msg = crate::inbox::InboxMessage {
        id: Some("m-verdict-1".into()),
        task_id: Some("t-x".into()),
        correlation_id: Some("t-x".into()),
        reviewed_head: Some("deadbeef".into()),
        from: "from:reviewer-1".into(),
        text: "VERIFIED — clean baseline + 5 platforms green".into(),
        kind: Some("report".into()),
        timestamp: "2026-05-17T00:00:00Z".into(),
        ..Default::default()
    };
    msg.report_purpose = crate::review_receipt::ReportPurpose::CodeReview;
    msg.validated_code_review = Some(crate::review_receipt::ValidatedCodeReviewReceipt::for_test(
        crate::review_receipt::ReviewReceiptSummary {
            receipt_id: "review-receipt:m-verdict-1".into(),
            source_id: "m-verdict-1".into(),
            evidence_digest: "a".repeat(64),
            assignment_id: uuid::Uuid::new_v4(),
            reviewer_instance_id: crate::types::InstanceId::new(),
            reviewer_name: "reviewer-1".into(),
            repo: "owner/repo".into(),
            pr_number: 1,
            branch: "feat/x".into(),
            task_id: "t-x".into(),
            reviewed_head: "deadbeef".into(),
            review_class: crate::daemon::pr_state::ReviewClass::Single,
            slot: crate::review_receipt::ReviewSlot::Primary,
            verdict: crate::review_receipt::ReviewVerdict::Verified,
        },
    ));
    msg
}

// ── t-worktree-leak (PR-1): release-invariant tests ──

fn write_pr(
    home: &Path,
    branch: &str,
    ms: crate::daemon::pr_state::MergeState,
    pr_number: u64,
    polled: bool,
) {
    use crate::daemon::pr_state;
    let mut s = pr_state::new_for_branch("o/r", branch, "headsha", pr_state::ReviewClass::Single);
    s.merge_state = ms;
    s.pr_number = pr_number;
    if polled {
        s.last_gh_poll_at = Some("2026-06-05T00:00:00Z".to_string());
    }
    pr_state::save(home, &s).unwrap();
}

use crate::daemon::pr_state::MergeState;

#[test]
fn invariant_merged_is_releasable() {
    let home = tmp_home("inv-merged");
    write_pr(
        &home,
        "feat/x",
        MergeState::Merged {
            merge_commit: "c0ffee".into(),
            merged_at: "2026-06-05T00:00:00Z".into(),
        },
        5,
        true,
    );
    let (r, c) = releasable_by_invariant(&home, "o/r", "feat/x");
    assert!(r, "merged PR is releasable");
    assert_eq!(c, PrConfidence::ObservedTerminal);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn invariant_open_pr_is_not_releasable() {
    // The Q1(b) behavior: a VERIFIED on an OPEN PR must NOT release.
    let home = tmp_home("inv-open");
    write_pr(&home, "feat/x", MergeState::MergeReady, 7, true);
    let (r, c) = releasable_by_invariant(&home, "o/r", "feat/x");
    assert!(
        !r,
        "open PR must not be releasable (release waits for terminal)"
    );
    assert_eq!(c, PrConfidence::ObservedOpen);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn invariant_no_pr_polled_is_releasable_when_tasks_done() {
    // gh-poll ran, no PR found (pr_number 0) + task exists and is done →
    // releasable via the no-PR branch (covers tasks that never produce a PR).
    // P0 cross-lease: zero-task vacuous truth removed; a done task is required.
    let home = tmp_home("inv-nopr");
    write_pr(&home, "feat/x", MergeState::NotReady, 0, true);
    seed_task(&home, "t-nopr", "dev", "feat/x", true);
    let (r, c) = releasable_by_invariant(&home, "o/r", "feat/x");
    assert!(r, "no-PR + all-tasks-done is releasable");
    assert_eq!(c, PrConfidence::QueriedNone);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn invariant_never_polled_is_unknown_not_releasable() {
    // pr_state exists (ci-watch armed) but never gh-polled → cannot positively
    // confirm no-PR (absence ≠ no-PR, must-fix #3).
    let home = tmp_home("inv-unknown");
    write_pr(&home, "feat/x", MergeState::NotReady, 0, false);
    let (r, c) = releasable_by_invariant(&home, "o/r", "feat/x");
    assert!(!r);
    assert_eq!(c, PrConfidence::Unknown);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn queried_none_requires_successful_poll_986() {
    // #986: QueriedNone (positively-no-PR → release) requires a SUCCESSFUL poll
    // (gh_poll_failures == 0). The Err path (scanner.rs:387) ALSO sets
    // last_gh_poll_at, so a failed / cold-cache poll (failures>0) must be
    // ambiguous (Unknown), never a false "no PR" that releases the worktree.
    // P0 cross-lease: zero-task vacuous truth removed; a done task is required.
    let home = tmp_home("qn-986");
    seed_task(&home, "t-986", "dev", "feat/x", true);
    // Successful poll, pr_number 0 → positively no PR → releasable.
    write_pr(&home, "feat/x", MergeState::NotReady, 0, true);
    let (r, c) = releasable_by_invariant(&home, "o/r", "feat/x");
    assert!(r, "success + no PR → releasable");
    assert_eq!(c, PrConfidence::QueriedNone);
    // Failed / cold-cache poll: failures>0 → ambiguous → NOT releasable.
    let mut s = crate::daemon::pr_state::load(&home, "o/r", "feat/x").unwrap();
    s.gh_poll_failures = 1;
    crate::daemon::pr_state::save(&home, &s).unwrap();
    let (r2, c2) = releasable_by_invariant(&home, "o/r", "feat/x");
    assert!(!r2, "failed/cold poll (failures>0) must NOT release");
    assert_eq!(c2, PrConfidence::Unknown);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn invariant_absent_pr_state_is_unknown() {
    let home = tmp_home("inv-absent");
    let (r, c) = releasable_by_invariant(&home, "o/r", "feat/missing");
    assert!(!r);
    assert_eq!(c, PrConfidence::Unknown);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn invariant_closed_unmerged_respects_grace() {
    let home = tmp_home("inv-closed");
    // Within grace (just closed) → not releasable.
    let fresh = chrono::Utc::now().to_rfc3339();
    write_pr(
        &home,
        "feat/x",
        MergeState::ClosedUnmerged { closed_at: fresh },
        9,
        true,
    );
    let (r, _) = releasable_by_invariant(&home, "o/r", "feat/x");
    assert!(
        !r,
        "closed-unmerged within grace must NOT release (may rework)"
    );
    // Past grace → releasable.
    let old = (chrono::Utc::now() - chrono::Duration::hours(CLOSE_GRACE_HOURS + 1)).to_rfc3339();
    write_pr(
        &home,
        "feat/x",
        MergeState::ClosedUnmerged { closed_at: old },
        9,
        true,
    );
    let (r2, c2) = releasable_by_invariant(&home, "o/r", "feat/x");
    assert!(r2, "closed-unmerged past grace is releasable");
    assert_eq!(c2, PrConfidence::ObservedTerminal);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn eligibility_requires_dispatch_task_id() {
    assert!(is_dispatch_lease(&serde_json::json!({ "task_id": "t-1" })));
    // Fail-safe: empty / missing task_id → NOT eligible.
    assert!(!is_dispatch_lease(&serde_json::json!({ "task_id": "" })));
    assert!(!is_dispatch_lease(
        &serde_json::json!({ "branch": "feat/x" })
    ));
}

#[test]
fn cas_lease_identity_detects_release() {
    let snap = LeaseIdentity::from_binding(
        "dev",
        &serde_json::json!({ "task_id": "t-1", "branch": "feat/x", "worktree": "/w", "issued_at": "T1" }),
    );
    // Same binding → matches.
    let same = LeaseIdentity::from_binding(
        "dev",
        &serde_json::json!({ "task_id": "t-1", "branch": "feat/x", "worktree": "/w", "issued_at": "T1" }),
    );
    assert_eq!(snap, same);
    // Re-leased (new task / issued_at) → mismatch → CAS skips.
    let relesed = LeaseIdentity::from_binding(
        "dev",
        &serde_json::json!({ "task_id": "t-2", "branch": "feat/x", "worktree": "/w", "issued_at": "T2" }),
    );
    assert_ne!(snap, relesed);
    // codex gap ①b: re-leased to the SAME branch name in a DIFFERENT repo →
    // source_repo differs → CAS catches it.
    let snap_repo = LeaseIdentity::from_binding(
        "dev",
        &serde_json::json!({ "task_id": "t-1", "branch": "feat/x", "worktree": "/w", "issued_at": "T1", "source_repo": "/repos/a" }),
    );
    let other_repo = LeaseIdentity::from_binding(
        "dev",
        &serde_json::json!({ "task_id": "t-1", "branch": "feat/x", "worktree": "/w", "issued_at": "T1", "source_repo": "/repos/b" }),
    );
    assert_ne!(
        snap_repo, other_repo,
        "CAS must catch a re-lease to a different repo"
    );
}

#[test]
fn cross_repo_same_branch_enqueue_skips_codex_1a() {
    // codex gap ①a: a bound branch in repo B must NOT be released by an event
    // for the same branch name in repo A.
    let home = tmp_home("itest-xrepo");
    let repo_b = itest_source_repo(&home, "owner/repo-b");
    itest_lease(&home, &repo_b, "dev", "feat/shared", "t-b", false);
    // Event for repo-A (a DIFFERENT repo) on the same branch name.
    enqueue_release_recompute(&home, "owner/repo-a", "feat/shared", "merge");
    assert_eq!(
        queue_len(&home),
        0,
        "cross-repo same-branch event must not enqueue against repo-b's lease"
    );
    assert!(bound(&home, "dev"), "repo-b's binding untouched");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn intent_expiry_after_7_days() {
    let mut intent = sample_intent("t-exp");
    intent.enqueued_at =
        (chrono::Utc::now() - chrono::Duration::days(INTENT_EXPIRY_DAYS + 1)).to_rfc3339();
    assert!(intent_expired(&intent), "intent older than 7d expires");
    intent.enqueued_at = chrono::Utc::now().to_rfc3339();
    assert!(!intent_expired(&intent), "fresh intent does not expire");
}

// ── t-worktree-leak (PR-1): enqueue→sweep→release INTEGRATION tests ──
// §3.9 / #1799: drive the REAL entry (the scanner's merge call /
// enqueue_release_recompute), provision real state (managed git worktree +
// dispatch binding + board task + pr_state), run the sweeper, and assert the
// worktree is actually released or retained — not an injected-input unit test.

fn itest_source_repo(home: &Path, slug: &str) -> std::path::PathBuf {
    let dir = home.join("source-repo");
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("https://github.com/{slug}.git");
    for args in [
        vec!["init", "-b", "main"],
        vec!["remote", "add", "origin", url.as_str()],
    ] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(&dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
    }
    // Mirror the real repo's .gitignore (line 29) so the `.agend-managed`
    // marker the lease writes into the worktree does NOT show as untracked —
    // otherwise `git status --porcelain` is non-empty and the dirty-guard
    // refuses to release (exactly how production stays clean).
    std::fs::write(dir.join(".gitignore"), ".agend-managed\n").unwrap();
    std::process::Command::new("git")
        .args(["add", ".gitignore"])
        .current_dir(&dir)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "init",
        ])
        .current_dir(&dir)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok();
    dir
}

fn seed_task(home: &Path, id: &str, owner: &str, branch: &str, done: bool) {
    use crate::task_events::{append, DoneSource, InstanceName, TaskEvent, TaskId};
    append(
        home,
        &InstanceName::from("test:lead"),
        TaskEvent::Created {
            task_id: TaskId(id.into()),
            title: "t".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: Some(InstanceName::from(owner)),
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
    .unwrap();
    if done {
        append(
            home,
            &InstanceName::from(owner),
            TaskEvent::Done {
                task_id: TaskId(id.into()),
                by: InstanceName::from(owner),
                source: DoneSource::OperatorManual {
                    authored_at: chrono::Utc::now().to_rfc3339(),
                    result: Some("ok".into()),
                },
            },
        )
        .unwrap();
    }
}

/// Provision a real managed worktree + dispatch binding (non-empty task_id) +
/// a board task owned by the agent. Returns the worktree path.
fn itest_lease(
    home: &Path,
    repo: &Path,
    agent: &str,
    branch: &str,
    task_id: &str,
    done: bool,
) -> std::path::PathBuf {
    let lease = crate::worktree_pool::lease(home, repo, agent, branch).expect("lease");
    crate::binding::bind_full(home, agent, task_id, branch, &lease.path, repo, false)
        .expect("bind_full");
    seed_task(home, task_id, agent, branch, done);
    lease.path
}

fn write_pr_slug(
    home: &Path,
    repo: &str,
    branch: &str,
    ms: MergeState,
    pr_number: u64,
    polled: bool,
) {
    use crate::daemon::pr_state;
    let mut s = pr_state::new_for_branch(repo, branch, "headsha", pr_state::ReviewClass::Single);
    s.merge_state = ms;
    s.pr_number = pr_number;
    if polled {
        s.last_gh_poll_at = Some("2026-06-05T00:00:00Z".to_string());
    }
    pr_state::save(home, &s).unwrap();
}

fn bound(home: &Path, agent: &str) -> bool {
    crate::binding::read(home, agent).is_some()
}
fn queue_len(home: &Path) -> usize {
    std::fs::read_dir(queue_dir(home))
        .map(|d| d.flatten().count())
        .unwrap_or(0)
}

fn write_fleet(home: &Path, agent: &str) {
    let p = crate::fleet::fleet_yaml_path(home);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&p, format!("instances:\n  {agent}:\n    backend: claude\n")).unwrap();
}

/// REAL task-done entry: the MCP handler (`task action=done`) — exercises the
/// handler→enqueue wiring (codex gap ③ / §3.9). These policy fixtures model
/// root's operator-forced terminalization, because their synthetic worktrees
/// are often deliberately dirty, unpushed, or cross-leased. Asserts no error.
fn task_done_via_handler(home: &Path, task_id: &str) {
    let r = crate::tasks::handle(
        home,
        "operator",
        &serde_json::json!({
            "action": "done",
            "id": task_id,
            "force": true,
            "force_reason": "auto-release policy fixture"
        }),
    );
    assert!(r.get("error").is_none(), "task action=done failed: {r}");
}

#[test]
fn integration_merge_releases_via_real_scanner() {
    // codex gap ③: drive the REAL scanner entry `scan_and_emit_with` (not the
    // helper) so the scanner→enqueue wiring is under test (breaks → fails).
    let home = tmp_home("itest-merge");
    let repo = itest_source_repo(&home, "owner/repo");
    itest_lease(&home, &repo, "dev", "feat/m", "t-m", false);
    write_pr_slug(
        &home,
        "owner/repo",
        "feat/m",
        MergeState::Merged {
            merge_commit: "c0ffee".into(),
            merged_at: "2026-06-05T00:00:00Z".into(),
        },
        5,
        true,
    );
    assert!(bound(&home, "dev"), "pre: agent is bound");
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    // Mock poller returns no PRs; the stored Merged state is sticky and drives
    // the scanner's terminal-merge arm → auto_release_for_merged_branch → enqueue.
    let poller = crate::daemon::pr_state::gh_poll::tests::MockGhPoller::new(vec![Ok(vec![])]);
    crate::daemon::pr_state::scan_and_emit_with(&home, &registry, &poller);
    assert_eq!(
        queue_len(&home),
        1,
        "real scanner→enqueue wiring produced an intent"
    );
    drain_queue(&home);
    assert!(
        !bound(&home, "dev"),
        "real scanner → enqueue → sweep → released (binding gone)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn merged_task_assignee_can_finish_after_auto_release_with_receipt() {
    let home = tmp_home("merge-receipt-task-done");
    let repo = itest_source_repo(&home, "owner/repo");
    let branch = "feat/receipt-done";
    let task_id = "t-receipt-done";
    let merge_sha = "a".repeat(40);
    itest_lease(&home, &repo, "dev", branch, task_id, false);
    let claimed = crate::tasks::handle(
        &home,
        "dev",
        &serde_json::json!({"action": "claim", "id": task_id}),
    );
    assert!(claimed.get("error").is_none(), "claim failed: {claimed}");

    let post_merge = crate::mcp::handlers::ci::post_merge_receipt_and_watch(
        &home,
        "owner/repo",
        &merge_sha,
        42,
        branch,
        "lead",
    );
    assert_eq!(
        post_merge["receipt"], "persisted",
        "real post-merge hook must persist completion provenance: {post_merge}"
    );
    assert!(
        crate::merge_receipt::find(&home, "owner/repo", &merge_sha, task_id).is_some(),
        "merge receipt must exist before auto-release"
    );

    write_pr_slug(
        &home,
        "owner/repo",
        branch,
        MergeState::Merged {
            merge_commit: merge_sha,
            merged_at: "2026-08-04T00:00:00Z".into(),
        },
        42,
        true,
    );
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let poller = crate::daemon::pr_state::gh_poll::tests::MockGhPoller::new(vec![Ok(vec![])]);
    crate::daemon::pr_state::scan_and_emit_with(&home, &registry, &poller);
    drain_queue(&home);
    assert!(!bound(&home, "dev"), "merge must auto-release the worktree");

    let done = crate::tasks::handle(
        &home,
        "dev",
        &serde_json::json!({"action": "done", "id": task_id}),
    );
    assert!(
        done.get("error").is_none(),
        "merge receipt should replace the intentionally released live binding: {done}"
    );
    assert_eq!(
        done["status"], "done",
        "task must reach terminal state: {done}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn integration_open_pr_retains_via_real_handler() {
    let home = tmp_home("itest-open");
    write_fleet(&home, "dev");
    let repo = itest_source_repo(&home, "owner/repo");
    itest_lease(&home, &repo, "dev", "feat/o", "t-o", false);
    write_pr_slug(
        &home,
        "owner/repo",
        "feat/o",
        MergeState::MergeReady,
        7,
        true,
    );
    // REAL entry: the task-done handler marks done + enqueues.
    task_done_via_handler(&home, "t-o");
    assert_eq!(queue_len(&home), 1, "task-done handler→enqueue wiring");
    drain_queue(&home);
    assert!(
        bound(&home, "dev"),
        "open PR → NOT released (binding stays)"
    );
    assert_eq!(queue_len(&home), 1, "open-PR intent retained for retry");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn integration_no_pr_task_done_releases_via_real_handler() {
    let home = tmp_home("itest-nopr");
    write_fleet(&home, "dev");
    let repo = itest_source_repo(&home, "owner/repo");
    itest_lease(&home, &repo, "dev", "feat/n", "t-n", false);
    write_pr_slug(&home, "owner/repo", "feat/n", MergeState::NotReady, 0, true); // polled, no PR
                                                                                 // REAL entry: task-done handler.
    task_done_via_handler(&home, "t-n");
    drain_queue(&home);
    assert!(
        !bound(&home, "dev"),
        "no-PR + task done (via real handler) → released"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn integration_cas_skips_re_leased_binding() {
    let home = tmp_home("itest-cas");
    let repo = itest_source_repo(&home, "owner/repo");
    let wt = itest_lease(&home, &repo, "dev", "feat/c", "t-c", false);
    write_pr_slug(
        &home,
        "owner/repo",
        "feat/c",
        MergeState::Merged {
            merge_commit: "c".into(),
            merged_at: "2026-06-05T00:00:00Z".into(),
        },
        5,
        true,
    );
    // Enqueue snapshots the CURRENT lease (task_id=t-c).
    crate::daemon::auto_release::enqueue_release_recompute(&home, "owner/repo", "feat/c", "merge");
    // Re-lease the SAME agent to a new task → snapshot is now stale.
    crate::binding::bind_full(&home, "dev", "t-c2", "feat/c", &wt, &repo, false).expect("rebind");
    drain_queue(&home);
    assert!(
        bound(&home, "dev"),
        "CAS: a stale (re-leased) intent must NOT release the new lease"
    );
    assert_eq!(
        queue_len(&home),
        0,
        "stale intent dropped (CAS skip is terminal)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn integration_expired_intent_dropped() {
    let home = tmp_home("itest-exp");
    std::fs::create_dir_all(queue_dir(&home)).unwrap();
    let mut intent = sample_intent("t-exp");
    intent.enqueued_at =
        (chrono::Utc::now() - chrono::Duration::days(INTENT_EXPIRY_DAYS + 1)).to_rfc3339();
    enqueue_intent(&home, &intent).unwrap();
    assert_eq!(queue_len(&home), 1);
    drain_queue(&home);
    assert_eq!(
        queue_len(&home),
        0,
        "expired intent dropped (force-reclaim backstop takes over)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #3005 ordering regression: `drain_queue` evaluates the 7-day `enqueued_at`
/// expiry BEFORE the Unknown-probe deferral, so a stamp can never immortalize an
/// intent. This drives the real `drain_queue` with an intent that is BOTH past
/// expiry AND carrying a far-future `unknown_retry_after` — it must still be
/// dropped. Reverse the two checks and the deferral's `continue` would skip the
/// expiry entirely, keeping the file alive on every sweep until the stamp
/// elapsed (and a re-stamp could extend it again).
#[test]
fn expired_intent_dropped_before_unknown_deferral_applies_3005() {
    let home = tmp_home("itest-exp-vs-stamp");
    std::fs::create_dir_all(queue_dir(&home)).unwrap();
    let mut intent = sample_intent("t-exp-stamp");
    intent.enqueued_at =
        (chrono::Utc::now() - chrono::Duration::days(INTENT_EXPIRY_DAYS + 1)).to_rfc3339();
    intent.unknown_retry_after =
        Some((chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339());
    enqueue_intent(&home, &intent).unwrap();
    assert_eq!(queue_len(&home), 1);
    drain_queue(&home);
    assert_eq!(
        queue_len(&home),
        0,
        "#3005: expiry must be checked before the deferral — a future \
         `unknown_retry_after` must not keep an expired intent alive"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #P1b test git helper: run git in `dir` with bypass + inline identity.
fn itest_git(dir: &Path, args: &[&str]) {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
}

/// #P1b: a MERGED PR (pr_state present = ObservedTerminal) + a DIRTY dispatch
/// worktree + done task. Pre-fix this refused forever (dirty→retain), leaving
/// an immortal worktree. Post-fix: dirty on a terminal branch is stale
/// build/handoff artifacts → WIP-preserve + release (release_full snapshots the
/// dirty tree to a recovery ref before removing, #2672).
#[test]
fn integration_merged_dirty_wip_preserves_and_releases() {
    let home = tmp_home("itest-merged-dirty");
    write_fleet(&home, "dev");
    let repo = itest_source_repo(&home, "owner/repo");
    let wt = itest_lease(&home, &repo, "dev", "feat/md", "t-md", false);
    // Untracked, non-ignored file → git status --porcelain non-empty → dirty.
    std::fs::write(wt.join("build-artifact.txt"), "dirty").unwrap();
    write_pr_slug(
        &home,
        "owner/repo",
        "feat/md",
        MergeState::Merged {
            merge_commit: "c".into(),
            merged_at: "2026-06-05T00:00:00Z".into(),
        },
        5,
        true,
    );
    task_done_via_handler(&home, "t-md");
    assert_eq!(queue_len(&home), 1, "task-done enqueued the intent");
    drain_queue(&home);
    assert!(
        !bound(&home, "dev"),
        "MERGED + dirty → WIP-preserved + released (not an immortal worktree)"
    );
    assert_eq!(
        queue_len(&home),
        0,
        "released intent is done (queue drained)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #P1b full-chain (lead merge-gate requirement): the fleet-standard
/// `--delete-branch` merge leaves NO pr_state (scanner deletes it) → evaluate
/// falls to Unknown. Pre-fix the dirty worktree retained FOREVER (immortal).
/// Post-fix the terminal-resolution fallback confirms the branch is
/// squash-merged (git cherry / patch-id, branch-delete-immune) → releasable →
/// WIP-preserve + release.
#[test]
fn integration_deleted_branch_merged_dirty_releases_via_squash_check() {
    let home = tmp_home("itest-deleted-merged");
    write_fleet(&home, "dev");
    let repo = itest_source_repo(&home, "owner/repo");
    let branch = "feat/sm";
    let wt = itest_lease(&home, &repo, "dev", branch, "t-sm", false);
    // A real branch commit (tracked change).
    std::fs::write(wt.join("feature.txt"), "feature work").unwrap();
    itest_git(&wt, &["add", "feature.txt"]);
    itest_git(&wt, &["commit", "-m", "feat work"]);
    // Squash-merge the branch into main in the source repo — what a GitHub
    // squash-merge does — so `git cherry main feat/sm` sees the patch present.
    itest_git(&repo, &["merge", "--squash", branch]);
    itest_git(&repo, &["commit", "-m", "squash merge feat/sm"]);
    // NO write_pr_slug → pr_state absent (== deleted after --delete-branch) →
    // evaluate = Unknown. Dirty (untracked, non-ignored) = build/handoff artifact.
    std::fs::write(wt.join("build-artifact.txt"), "dirty").unwrap();
    task_done_via_handler(&home, "t-sm");
    assert_eq!(queue_len(&home), 1, "task-done enqueued the intent");
    drain_queue(&home);
    assert!(
        !bound(&home, "dev"),
        "merged (squash-confirmed) + dirty → WIP-preserved + RELEASED, not immortal"
    );
    assert_eq!(
        queue_len(&home),
        0,
        "released intent is done (queue drained)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #P1b guard: NO pr_state + branch NOT merged + done task + dirty → the
/// terminal-resolution fallback must find is_squash_merged = false → stay
/// Unknown → RETAIN (never false-release a done-but-unmerged worktree).
#[test]
fn integration_unmerged_unknown_dirty_retains() {
    let home = tmp_home("itest-unmerged-unknown");
    write_fleet(&home, "dev");
    let repo = itest_source_repo(&home, "owner/repo");
    let branch = "feat/pg";
    let wt = itest_lease(&home, &repo, "dev", branch, "t-pg", false);
    // A real UNMERGED branch commit (never applied to main).
    std::fs::write(wt.join("feature.txt"), "unmerged work").unwrap();
    itest_git(&wt, &["add", "feature.txt"]);
    itest_git(&wt, &["commit", "-m", "unmerged feat"]);
    std::fs::write(wt.join("build-artifact.txt"), "dirty").unwrap();
    // No pr_state → Unknown; NOT squash-merged; gh reports an existing (open)
    // PR → branch_never_had_pr = Some(false) → the no-PR path also declines →
    // RETAIN. Deterministic mock avoids a real gh call in the test.
    let _scm = crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_pr_list(
        crate::scm::MockPrList::Prs(1),
    ));
    task_done_via_handler(&home, "t-pg");
    assert_eq!(queue_len(&home), 1, "task-done enqueued the intent");
    drain_queue(&home);
    assert!(
        bound(&home, "dev"),
        "Unknown + NOT merged + dirty → binding preserved (no false-release)"
    );
    assert_eq!(queue_len(&home), 1, "unmerged intent RETAINED for retry");
    let _ = std::fs::remove_dir_all(&home);
}

/// #t-…24962-7 (a): a verdict-auto-closed task must ENQUEUE a release-recompute
/// (mirroring the MCP task-done handler) so the reporter's review worktree gets
/// released. RED against pre-fix `auto_close_on_report`, which appended Done
/// WITHOUT enqueueing → the review-worktree binding leaked (intent never
/// existed = the specimens' empty queue).
#[test]
fn auto_close_on_report_enqueues_release_recompute() {
    let home = tmp_home("itest-autoclose-enq");
    write_fleet(&home, "reviewer");
    let repo = itest_source_repo(&home, "owner/repo");
    let worktree = itest_lease(&home, &repo, "reviewer", "review/x", "t-rev", false);
    let provisioned_head =
        crate::git_helpers::git_cmd(&worktree, &["rev-parse", "HEAD"]).expect("review lease HEAD");
    crate::binding::bind_full_with_provenance(
        &home,
        "reviewer",
        "t-rev",
        "review/x",
        &worktree,
        &repo,
        false,
        Some(crate::binding::BindingProvenance::DaemonProvisionedReview {
            provisioned_head: &provisioned_head,
        }),
    )
    .expect("daemon-provisioned review binding");
    let seeded_tracking_ref = crate::git_helpers::git_bypass(
        &repo,
        &[
            "update-ref",
            "refs/remotes/origin/review-subject",
            &provisioned_head,
        ],
    )
    .expect("seed synthetic review tracking ref");
    assert!(
        seeded_tracking_ref.status.success(),
        "synthetic review tracking ref seeding failed"
    );
    assert_eq!(queue_len(&home), 0, "no intent before the terminal report");
    let closed = crate::tasks::auto_close::auto_close_on_report(
        &home,
        "report",
        "t-rev",
        "reviewer",
        "VERIFIED — clean",
        true,
    )
    .expect("auto_close ok");
    assert!(closed, "terminal report auto-closes the review task");
    assert_eq!(
        queue_len(&home),
        1,
        "verdict auto-close must enqueue a release-recompute intent"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #t-…24962-7 (b): a done task on a branch that NEVER had a PR (review / spike
/// / design worktree), no pr_state, dirty → gh positively confirms no PR →
/// WIP-preserve + release. RED (pre-fix): Unknown → retain FOREVER (immortal
/// review worktree — the specimen class).
#[test]
fn integration_no_pr_ever_dirty_releases_via_gh_confirm() {
    let home = tmp_home("itest-nopr-ever");
    write_fleet(&home, "reviewer");
    let repo = itest_source_repo(&home, "owner/repo");
    let wt = itest_lease(&home, &repo, "reviewer", "review/y", "t-revy", false);
    std::fs::write(wt.join("build-artifact.txt"), "dirty").unwrap();
    // gh confirms the branch never had ANY PR (empty pr list).
    let _scm = crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_pr_list(
        crate::scm::MockPrList::Prs(0),
    ));
    task_done_via_handler(&home, "t-revy");
    assert_eq!(queue_len(&home), 1, "task-done enqueued the intent");
    drain_queue(&home);
    assert!(
        !bound(&home, "reviewer"),
        "no-PR-ever + dirty → WIP-preserved + released (not immortal)"
    );
    assert_eq!(
        queue_len(&home),
        0,
        "released intent is done (queue drained)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #t-…24962-7 guard: a transient gh failure during the no-PR confirmation
/// must NOT release (branch_never_had_pr = None) — the worktree is RETAINED for
/// a later sweep (#986 convention), never false-released on an unconfirmed query.
#[test]
fn integration_no_pr_transient_gh_fail_retains() {
    let home = tmp_home("itest-nopr-ghfail");
    write_fleet(&home, "reviewer");
    let repo = itest_source_repo(&home, "owner/repo");
    let wt = itest_lease(&home, &repo, "reviewer", "review/z", "t-revz", false);
    std::fs::write(wt.join("build-artifact.txt"), "dirty").unwrap();
    // gh fails → cannot confirm no-PR.
    let _scm = crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_pr_list(
        crate::scm::MockPrList::Fail("gh: rate limited".into()),
    ));
    task_done_via_handler(&home, "t-revz");
    assert_eq!(queue_len(&home), 1, "task-done enqueued the intent");
    drain_queue(&home);
    assert!(
        bound(&home, "reviewer"),
        "transient gh failure → binding preserved (no false-release)"
    );
    assert_eq!(queue_len(&home), 1, "unconfirmed intent RETAINED for retry");
    let _ = std::fs::remove_dir_all(&home);
}

/// #3005: a done-but-unprovable branch (no pr_state → Unknown, not squash-merged,
/// gh reports an existing PR → release cannot be proven) re-ran the ENTIRE
/// expensive terminal-resolution probe — the `default_branch` and
/// `is_squash_merged` git spawns plus the head-scoped `branch_never_had_pr` gh
/// query — on EVERY sweep (≈30s), forever, until the 7-day expiry.
/// (`evaluate_pr_for_release` itself issues no gh call: it is a local `pr_state`
/// file read, `pr_state::load` → `read_to_string`.)
/// `pr_list_calls` is the deterministic witness — it stops advancing once the
/// deferral holds, with no wall-clock sleep needed to observe it.
///
/// Post-fix a single `unknown_retry_after` stamp defers the next probe by a fixed
/// 5 minutes, while still RETAINING both the intent and the binding — and a fresh
/// enqueue (new evidence) clears the stamp so the very next sweep probes again
/// immediately, with no wait.
#[test]
fn unknown_probe_backs_off_and_new_evidence_wakes_it_immediately() {
    let home = tmp_home("itest-unknown-backoff");
    write_fleet(&home, "dev");
    let repo = itest_source_repo(&home, "owner/repo");
    let branch = "feat/backoff";
    let wt = itest_lease(&home, &repo, "dev", branch, "t-bo", false);
    // A real UNMERGED branch commit → is_squash_merged = false.
    std::fs::write(wt.join("feature.txt"), "unmerged work").unwrap();
    itest_git(&wt, &["add", "feature.txt"]);
    itest_git(&wt, &["commit", "-m", "unmerged feat"]);
    // No pr_state → Unknown; gh reports an existing (open) PR →
    // branch_never_had_pr = Some(false) → neither terminal-resolution path can
    // prove release → retain. Deterministic mock, no real gh call.
    let provider = crate::scm::MockScmProvider::with_pr_list(crate::scm::MockPrList::Prs(1));
    let _scm = crate::scm::set_test_scm_provider(provider.clone());

    task_done_via_handler(&home, "t-bo");
    assert_eq!(queue_len(&home), 1, "task-done enqueued the intent");

    drain_queue(&home);
    let per_sweep = provider.pr_list_calls();
    assert_eq!(
        per_sweep, 2,
        "baseline: the unprovable path reaches gh (the head-scoped no-PR probe) — \
         pinned as an exact count so the post-fix assertion below is meaningful"
    );
    assert_eq!(queue_len(&home), 1, "unprovable intent RETAINED");
    assert!(bound(&home, "dev"), "unprovable → never false-released");

    // Second sweep fired immediately (no wall-clock sleep): within the fixed
    // 5-minute window the expensive probe must be skipped entirely.
    drain_queue(&home);
    assert_eq!(
        provider.pr_list_calls(),
        per_sweep,
        "#3005: inside the 5-minute Unknown backoff the expensive probe must NOT re-run"
    );
    assert_eq!(
        queue_len(&home),
        1,
        "#3005: a backed-off intent is RETAINED, never dropped"
    );
    assert!(
        bound(&home, "dev"),
        "#3005: backoff must not release or drop the binding"
    );

    // Fresh evidence (a merge event) re-enqueues the intent with no stamp, so the
    // next sweep probes immediately instead of waiting out the 5 minutes.
    crate::daemon::auto_release::enqueue_release_recompute(&home, "owner/repo", branch, "merge");
    drain_queue(&home);
    assert_eq!(
        provider.pr_list_calls(),
        per_sweep * 2,
        "#3005: a fresh enqueue clears the backoff — new evidence wakes the probe immediately"
    );
    assert!(bound(&home, "dev"), "still unprovable → still not released");
    let _ = std::fs::remove_dir_all(&home);
}

// ── cross-lease auto-release P0 RED tests ─────────────────────────
// Five adversarial cases proving the two bugs:
//   Bug A: handler.rs enqueues release for the owner's CURRENT binding
//          without checking binding.task_id == completed task id.
//   Bug B: all_branch_tasks_done uses list_all(home) which only reads
//          the default board; project-board tasks are invisible, and
//          zero matching tasks vacuously returns true.

/// Seed a task on a non-default project board (not the home/default board).
fn seed_task_on_board(home: &Path, project: &str, id: &str, owner: &str, branch: &str, done: bool) {
    use crate::task_events::{append_at, board_root, DoneSource, InstanceName, TaskEvent, TaskId};
    let board = board_root(home, project);
    std::fs::create_dir_all(&board).unwrap();
    append_at(
        &board,
        &InstanceName::from("test:lead"),
        TaskEvent::Created {
            task_id: TaskId(id.into()),
            title: "t".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: Some(InstanceName::from(owner)),
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
    .unwrap();
    if done {
        append_at(
            &board,
            &InstanceName::from(owner),
            TaskEvent::Done {
                task_id: TaskId(id.into()),
                by: InstanceName::from(owner),
                source: DoneSource::OperatorManual {
                    authored_at: chrono::Utc::now().to_rfc3339(),
                    result: Some("ok".into()),
                },
            },
        )
        .unwrap();
    }
}

/// RED case 1 — Bug A: task done for t-old must NOT enqueue release for
/// the owner's CURRENT binding which points to a DIFFERENT task (t-new).
/// Live reproducer: root marks stale task done → handler reads owner's
/// current binding → enqueues release for the active WIP worktree.
#[test]
fn cross_lease_done_no_release_when_binding_task_mismatch() {
    let home = tmp_home("cross-lease-mismatch");
    let repo = itest_source_repo(&home, "owner/repo");
    write_fleet(&home, "dev-1");
    // dev-1 is actively working on t-new / feat/new
    itest_lease(&home, &repo, "dev-1", "feat/new", "t-new", false);
    // An OLD task (t-old / feat/old) also exists, owned by dev-1
    seed_task(&home, "t-old", "dev-1", "feat/old", false);
    // Root marks t-old done (the stale cleanup scenario) through the shared
    // operator-forced handler fixture.
    task_done_via_handler(&home, "t-old");
    assert_eq!(
            queue_len(&home),
            0,
            "Bug A: handler must NOT enqueue release when binding.task_id (t-new) != completed task (t-old)"
        );
    assert!(
        bound(&home, "dev-1"),
        "dev-1 must remain bound to t-new — the active WIP must not be touched"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// RED case 2 — Bug B: an in-progress task on a non-default project board
/// with the same branch must block release. Currently list_all(home) misses it.
#[test]
fn project_board_in_progress_task_blocks_release() {
    let home = tmp_home("proj-board-blocks");
    let _repo = itest_source_repo(&home, "owner/repo");
    // Task on project board (not default) — in-progress, branch feat/x
    seed_task_on_board(
        &home,
        "Hack_agend-terminal",
        "t-proj",
        "dev-2",
        "feat/x",
        false,
    );
    // Bind dev-2 so the repo-scoping in all_branch_tasks_done can resolve
    crate::binding::bind(&home, "dev-2", "t-proj", "feat/x");
    assert!(
            !all_branch_tasks_done(&home, "owner/repo", "feat/x"),
            "Bug B: in-progress task on project board must block release (currently invisible to list_all)"
        );
    let _ = std::fs::remove_dir_all(&home);
}

/// RED case 3 — Bug B: a corrupt boards/ directory must cause fail-closed
/// (return false), not silently proceed with default-only view.
#[test]
fn board_enumeration_error_fails_closed() {
    let home = tmp_home("board-enum-error");
    // Seed a done task on the default board so current code would say "all done"
    seed_task(&home, "t-default-done", "dev-3", "feat/e", true);
    // Create boards/ as a FILE (not directory) → read_dir fails
    let boards_path = home.join("boards");
    std::fs::write(&boards_path, "corrupt").unwrap();
    assert!(
        !all_branch_tasks_done(&home, "owner/repo", "feat/e"),
        "Bug B: unreadable boards/ must fail closed — cannot guarantee all boards were checked"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// RED case 4 — Bug B: zero matching branch tasks must fail closed (return
/// false). Currently .all() on empty iterator vacuously returns true.
#[test]
fn zero_matching_branch_tasks_fails_closed() {
    let home = tmp_home("zero-match");
    // Seed a task with a DIFFERENT branch so the board isn't empty
    seed_task(&home, "t-other", "dev-4", "feat/other", false);
    assert!(
        !all_branch_tasks_done(&home, "owner/repo", "feat/z"),
        "Bug B: zero tasks matching branch must fail closed — no evidence of completion"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// RED case 5 (control) — same task+lease terminal: a done task on the
/// default board with correct binding must still permit release (true).
/// This is the POSITIVE case that must survive the fix.
#[test]
fn same_lease_terminal_permits_release() {
    let home = tmp_home("same-lease-ok");
    let repo = itest_source_repo(&home, "owner/repo");
    // Task is done, binding matches
    itest_lease(&home, &repo, "dev-5", "feat/y", "t-done", true);
    assert!(
        all_branch_tasks_done(&home, "owner/repo", "feat/y"),
        "same task+lease terminal must still permit release"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Regression: malformed event record on a project board must cause
/// strict replay to fail, making all_branch_tasks_done return false.
/// Before the fix, lenient replay_at silently skipped corrupt records,
/// hiding pending tasks and permitting unsafe release.
#[test]
fn malformed_board_record_fails_closed() {
    let home = tmp_home("malformed-record");
    // Done task on default board — would allow release if project board is ignored
    seed_task(&home, "t-done", "dev-6", "feat/m", true);
    // Create a project board with a malformed event record
    let board = crate::task_events::board_root(&home, "Hack_agend-terminal");
    std::fs::create_dir_all(&board).unwrap();
    let log = board.join("task_events.jsonl");
    std::fs::write(&log, "not valid json\n").unwrap();
    assert!(
        !all_branch_tasks_done(&home, "owner/repo", "feat/m"),
        "malformed event record on project board must fail closed — \
             strict replay rejects corrupt records"
    );
    let _ = std::fs::remove_dir_all(&home);
}
