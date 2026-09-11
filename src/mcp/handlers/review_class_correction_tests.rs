#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::scm::{
    CheckState, CompareResult, IssueSummary, ListFilter, MergeOpts, MergeOutcome, PrSummary,
    ScmProvider,
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const REPO: &str = "owner/repo";
const BRANCH: &str = "fix/review-class";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER_HEAD: &str = "fedcba9876543210fedcba9876543210fedcba98";

struct ExactPrProvider {
    summary: PrSummary,
}

impl ScmProvider for ExactPrProvider {
    fn pr_view(&self, _repo: &str, _pr: u64, _fields: &[&str]) -> anyhow::Result<PrSummary> {
        Ok(self.summary.clone())
    }
    fn pr_checks(&self, _repo: &str, _pr: u64) -> anyhow::Result<Vec<CheckState>> {
        Ok(Vec::new())
    }
    fn pr_list(
        &self,
        _repo: &str,
        _filter: &ListFilter,
        _fields: &[&str],
        _cwd: Option<&Path>,
    ) -> anyhow::Result<Vec<PrSummary>> {
        Ok(Vec::new())
    }
    fn pr_merge(&self, _repo: &str, _pr: u64, _opts: &MergeOpts) -> anyhow::Result<MergeOutcome> {
        Ok(MergeOutcome::Submitted)
    }
    fn issue_view(
        &self,
        _repo: &str,
        _number: u64,
        _fields: &[&str],
    ) -> anyhow::Result<IssueSummary> {
        Ok(IssueSummary::default())
    }
    fn compare(&self, _repo: &str, _base: &str, _head: &str) -> anyhow::Result<CompareResult> {
        Ok(CompareResult::default())
    }
}

fn home(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "agend-review-class-correction-{label}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn args(action: &str, operation_id: &str) -> Value {
    json!({
        "action": action,
        "repository": REPO,
        "pr_number": 42,
        "branch": BRANCH,
        "head_sha": HEAD,
        "expected_old_class": "dual",
        "new_class": "single",
        "operation_id": operation_id,
        "reason": "operator-approved Decision45 correction",
    })
}

fn seed_with_class(home: &Path, review_class: ReviewClass) {
    let mut state = crate::daemon::pr_state::new_for_branch(REPO, BRANCH, HEAD, review_class);
    state.pr_number = 42;
    crate::daemon::pr_state::save(home, &state).unwrap();
    let watch_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    std::fs::create_dir_all(&watch_dir).unwrap();
    let watch_path = watch_dir.join(crate::daemon::ci_watch::watch_filename(REPO, BRANCH));
    crate::store::atomic_write(
        &watch_path,
        serde_json::to_string_pretty(&json!({
            "repo": REPO,
            "branch": BRANCH,
            "pr_number": 42,
            "subject_head_sha": HEAD,
            "review_class": review_class.as_token(),
        }))
        .unwrap()
        .as_bytes(),
    )
    .unwrap();
}

fn seed(home: &Path) {
    seed_with_class(home, ReviewClass::Dual);
}

fn provider() -> crate::scm::TestScmGuard {
    crate::scm::set_test_scm_provider(Arc::new(ExactPrProvider {
        summary: PrSummary {
            number: 42,
            head_ref: Some(BRANCH.to_string()),
            head_ref_oid: Some(HEAD.to_string()),
            ..Default::default()
        },
    }))
}

#[test]
fn real_entry_denies_agent_and_orchestrator_spoof() {
    let home = home("deny");
    let sender = crate::identity::Sender::new("lead").unwrap();
    let out = handle_correct_review_class(&home, &args("apply", "deny-1"), &Some(sender));
    assert_eq!(out["code"], "review_class_correction_operator_only");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn real_entry_preview_apply_and_same_operation_retry_are_audited() {
    let home = home("apply");
    seed(&home);
    let _provider = provider();
    let preview = handle_correct_review_class(&home, &args("preview", "op-1"), &None);
    assert_eq!(preview["preview"], true, "{preview}");
    assert_eq!(preview["old_class"], "dual", "{preview}");

    let applied = handle_correct_review_class(&home, &args("apply", "op-1"), &None);
    assert_eq!(applied["ok"], true, "{applied}");
    assert_eq!(applied["effect"]["phase"], "completed", "{applied}");
    assert_eq!(
        crate::daemon::pr_state::load(&home, REPO, BRANCH)
            .unwrap()
            .review_class,
        ReviewClass::Single
    );
    let watch_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
    let mismatched_watch = watch_dir.join(crate::daemon::ci_watch::watch_filename_exact_head(
        REPO, BRANCH, OTHER_HEAD,
    ));
    crate::store::atomic_write(
        &mismatched_watch,
        serde_json::to_string(&json!({
            "repo": REPO,
            "branch": BRANCH,
            "pr_number": 42,
            "target_head_sha": OTHER_HEAD,
            "review_class": "dual",
        }))
        .unwrap()
        .as_bytes(),
    )
    .unwrap();
    let watch_path = watch_dir.join(crate::daemon::ci_watch::watch_filename(REPO, BRANCH));
    let watch: Value = serde_json::from_slice(&std::fs::read(watch_path).unwrap()).unwrap();
    assert_eq!(watch["review_class"], "single", "{watch}");
    let mismatched: Value =
        serde_json::from_slice(&std::fs::read(mismatched_watch).unwrap()).unwrap();
    assert_eq!(mismatched["review_class"], "dual", "{mismatched}");

    let retry = handle_correct_review_class(&home, &args("apply", "op-1"), &None);
    assert_eq!(retry["already_applied"], true, "{retry}");
    let conflict = handle_correct_review_class(
        &home,
        &{
            let mut value = args("apply", "op-1");
            value["reason"] = json!("different correction");
            value
        },
        &None,
    );
    assert_eq!(
        conflict["code"],
        "review_class_correction_operation_conflict"
    );
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn real_entry_refuses_snapshot_and_old_class_drift() {
    let home = home("drift");
    seed(&home);
    let _provider = crate::scm::set_test_scm_provider(Arc::new(ExactPrProvider {
        summary: PrSummary {
            number: 42,
            head_ref: Some(BRANCH.to_string()),
            head_ref_oid: Some("fedcba9876543210fedcba9876543210fedcba98".to_string()),
            ..Default::default()
        },
    }));
    let out = handle_correct_review_class(&home, &args("apply", "drift-1"), &None);
    assert_eq!(
        out["code"], "review_class_correction_snapshot_drift",
        "{out}"
    );

    let _provider = provider();
    let mut wrong = args("apply", "drift-2");
    wrong["expected_old_class"] = json!("single");
    wrong["new_class"] = json!("dual");
    let out = handle_correct_review_class(&home, &wrong, &None);
    assert_eq!(
        out["code"], "review_class_correction_old_class_mismatch",
        "{out}"
    );
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn incomplete_journal_fences_exact_subject_only() {
    let home = home("fence");
    let journal = CorrectionJournal {
        schema_version: JOURNAL_VERSION,
        operation_id: "fence-1".into(),
        repository: REPO.into(),
        branch: BRANCH.into(),
        pr_number: 42,
        head_sha: HEAD.into(),
        expected_old_class: ReviewClass::Dual,
        new_class: ReviewClass::Single,
        reason: "operator reason".into(),
        phase: "prepared".into(),
        before_class: ReviewClass::Dual,
        invalidated_assignments: 0,
        invalidated_buffered_receipts: 0,
        invalidated_persisted_receipts: 0,
        updated_watch_records: 0,
        started_at: chrono::Utc::now().to_rfc3339(),
        completed_at: None,
    };
    crate::store::save_atomic(&journal_path(&home, "fence-1"), &journal).unwrap();
    assert!(is_incomplete(&home, REPO, BRANCH, 42, HEAD).unwrap());
    assert!(!is_incomplete(&home, REPO, BRANCH, 43, HEAD).unwrap());
    assert!(!is_incomplete(&home, REPO, BRANCH, 42, &"f".repeat(40)).unwrap());
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn corrupt_journal_fence_fails_closed() {
    let home = home("corrupt-journal");
    let dir = journal_dir(&home);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("corrupt.json"), b"not-json").unwrap();
    let error = is_incomplete(&home, REPO, BRANCH, 42, HEAD).unwrap_err();
    assert!(error.contains("corrupt"), "{error}");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn incomplete_journal_defers_ci_observation_for_exact_subject() {
    let home = home("ci-fence");
    seed(&home);
    let journal = CorrectionJournal {
        schema_version: JOURNAL_VERSION,
        operation_id: "ci-fence-1".into(),
        repository: REPO.into(),
        branch: BRANCH.into(),
        pr_number: 42,
        head_sha: HEAD.into(),
        expected_old_class: ReviewClass::Dual,
        new_class: ReviewClass::Single,
        reason: "operator reason".into(),
        phase: "prepared".into(),
        before_class: ReviewClass::Dual,
        invalidated_assignments: 0,
        invalidated_buffered_receipts: 0,
        invalidated_persisted_receipts: 0,
        updated_watch_records: 0,
        started_at: chrono::Utc::now().to_rfc3339(),
        completed_at: None,
    };
    crate::store::save_atomic(&journal_path(&home, "ci-fence-1"), &journal).unwrap();
    crate::daemon::pr_state::record_ci_result(
        &home,
        REPO,
        BRANCH,
        HEAD,
        crate::daemon::pr_state::CiConclusion::Green,
        Vec::new(),
        ReviewClass::Single,
    );
    assert_eq!(
        crate::daemon::pr_state::load(&home, REPO, BRANCH)
            .unwrap()
            .review_class,
        ReviewClass::Dual
    );
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn real_entry_allows_legitimate_new_dual_class() {
    let home = home("dual");
    seed_with_class(&home, ReviewClass::Single);
    let _provider = provider();
    let mut request = args("apply", "dual-1");
    request["expected_old_class"] = json!("single");
    request["new_class"] = json!("dual");
    let out = handle_correct_review_class(&home, &request, &None);
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(
        crate::daemon::pr_state::load(&home, REPO, BRANCH)
            .unwrap()
            .review_class,
        ReviewClass::Dual
    );
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn real_entry_recovers_interrupted_completion_after_restart() {
    let home = home("recovery");
    seed_with_class(&home, ReviewClass::Single);
    let _provider = provider();
    let journal = CorrectionJournal {
        schema_version: JOURNAL_VERSION,
        operation_id: "recover-1".into(),
        repository: REPO.into(),
        branch: BRANCH.into(),
        pr_number: 42,
        head_sha: HEAD.into(),
        expected_old_class: ReviewClass::Dual,
        new_class: ReviewClass::Single,
        reason: "operator-approved Decision45 correction".into(),
        phase: "effects_applied".into(),
        before_class: ReviewClass::Dual,
        invalidated_assignments: 1,
        invalidated_buffered_receipts: 2,
        invalidated_persisted_receipts: 1,
        updated_watch_records: 1,
        started_at: chrono::Utc::now().to_rfc3339(),
        completed_at: None,
    };
    crate::store::save_atomic(&journal_path(&home, "recover-1"), &journal).unwrap();
    let out = handle_correct_review_class(&home, &args("apply", "recover-1"), &None);
    assert_eq!(out["recovered"], true, "{out}");
    assert_eq!(out["effect"]["phase"], "completed", "{out}");
    let _ = std::fs::remove_dir_all(home);
}
