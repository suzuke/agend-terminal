//! Decision45: operator-only, exact-PR review-class correction.
//!
//! Ordinary CI/watch reconciliation is deliberately monotonic: a persisted
//! `Dual` floor cannot be weakened by a later `Single` observation. This
//! module is the narrow, audited exception authorized by the operator. It is
//! intentionally separate from `ci watch` so a caller cannot smuggle a class
//! downgrade through a normal re-arm.

use crate::daemon::pr_state::ReviewClass;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const JOURNAL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CorrectionJournal {
    schema_version: u32,
    operation_id: String,
    repository: String,
    branch: String,
    pr_number: u64,
    head_sha: String,
    expected_old_class: ReviewClass,
    new_class: ReviewClass,
    reason: String,
    phase: String,
    before_class: ReviewClass,
    invalidated_assignments: usize,
    invalidated_buffered_receipts: usize,
    invalidated_persisted_receipts: usize,
    #[serde(default)]
    updated_watch_records: usize,
    started_at: String,
    completed_at: Option<String>,
}

fn journal_dir(home: &Path) -> PathBuf {
    home.join("review-class-corrections")
}

fn journal_path(home: &Path, operation_id: &str) -> PathBuf {
    journal_dir(home).join(format!("{operation_id}.json"))
}

fn valid_operation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn parse_class(args: &Value, key: &str) -> Result<ReviewClass, Value> {
    let Some(raw) = args[key].as_str() else {
        return Err(json!({
            "error": format!("review_class_correction requires `{key}`"),
            "code": "review_class_correction_missing_field",
        }));
    };
    let class = ReviewClass::parse_fail_closed(Some(raw));
    if matches!(class, ReviewClass::Unresolved) {
        return Err(json!({
            "error": format!("{key} must be exactly `single` or `dual`"),
            "code": "review_class_correction_invalid_class",
        }));
    }
    Ok(class)
}

fn exact_head(args: &Value) -> Option<&str> {
    args["head_sha"]
        .as_str()
        .filter(|head| crate::review_receipt::is_full_head(head))
}

fn load_journal(home: &Path, operation_id: &str) -> Result<Option<CorrectionJournal>, Value> {
    let path = journal_path(home, operation_id);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(json!({
                "error": format!("review-class correction journal read failed: {error}"),
                "code": "review_class_correction_journal_unreadable",
            }));
        }
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        json!({
            "error": format!("review-class correction journal is corrupt: {error}"),
            "code": "review_class_correction_journal_corrupt",
        })
    })
}

#[allow(clippy::too_many_arguments)]
fn same_request(
    journal: &CorrectionJournal,
    repository: &str,
    branch: &str,
    pr_number: u64,
    head_sha: &str,
    expected_old_class: ReviewClass,
    new_class: ReviewClass,
    reason: &str,
) -> bool {
    journal.repository == repository
        && journal.branch == branch
        && journal.pr_number == pr_number
        && journal.head_sha.eq_ignore_ascii_case(head_sha)
        && journal.expected_old_class == expected_old_class
        && journal.new_class == new_class
        && journal.reason == reason
}

/// Return true while an exact subject has a durable correction that has not
/// reached `completed`. Dispatch, receipt replay, and merge call this before
/// admitting authority, so a crash between intent and effect cannot open a
/// half-corrected gate.
pub(crate) fn is_incomplete(
    home: &Path,
    repository: &str,
    branch: &str,
    pr_number: u64,
    head_sha: &str,
) -> bool {
    let Ok(entries) = std::fs::read_dir(journal_dir(home)) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        path.extension().and_then(|ext| ext.to_str()) == Some("json")
            && std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<CorrectionJournal>(&bytes).ok())
                .is_some_and(|journal| {
                    journal.phase != "completed"
                        && same_request(
                            &journal,
                            repository,
                            branch,
                            pr_number,
                            head_sha,
                            journal.expected_old_class,
                            journal.new_class,
                            &journal.reason,
                        )
                })
    })
}

fn validate_snapshot(
    home: &Path,
    repository: &str,
    branch: &str,
    pr_number: u64,
    head_sha: &str,
    expected_old_class: ReviewClass,
) -> Result<crate::daemon::pr_state::PrState, Value> {
    let provider = crate::scm::make_scm_provider(repository, None);
    let summary = provider
        .pr_view(
            repository,
            pr_number,
            &["number", "headRefOid", "headRefName"],
        )
        .map_err(|error| {
            json!({
                "error": format!("review-class correction PR snapshot failed: {error}"),
                "code": "review_class_correction_snapshot_failed",
            })
        })?;
    if summary.number != pr_number
        || summary.head_ref.as_deref() != Some(branch)
        || summary.head_ref_oid.as_deref() != Some(head_sha)
    {
        return Err(json!({
            "error": "review-class correction refused: exact PR subject snapshot drifted",
            "code": "review_class_correction_snapshot_drift",
            "expected": {"repository": repository, "pr_number": pr_number, "branch": branch, "head_sha": head_sha},
            "observed": {"number": summary.number, "branch": summary.head_ref, "head_sha": summary.head_ref_oid},
        }));
    }
    let state = crate::daemon::pr_state::load(home, repository, branch).ok_or_else(|| {
        json!({
            "error": "review-class correction refused: exact PR state is absent",
            "code": "review_class_correction_state_missing",
        })
    })?;
    if state.repo != repository
        || state.branch != branch
        || state.pr_number != pr_number
        || state.head_sha != head_sha
    {
        return Err(json!({
            "error": "review-class correction refused: local PR state subject drifted",
            "code": "review_class_correction_state_drift",
        }));
    }
    if state.review_class != expected_old_class {
        return Err(json!({
            "error": format!("review-class correction expected {}, found {}", expected_old_class.as_token(), state.review_class.as_token()),
            "code": "review_class_correction_old_class_mismatch",
        }));
    }
    Ok(state)
}

/// Update only the exact repository/branch watch records. The per-watch lock
/// preserves poller cursor/history while preventing a concurrent CI flush
/// from restoring the stale class after the correction has completed.
fn update_watch_class(
    home: &Path,
    repository: &str,
    branch: &str,
    new_class: ReviewClass,
) -> anyhow::Result<usize> {
    let dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut updated = 0;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let lock = crate::store::acquire_file_lock(&path.with_extension("lock"))?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                drop(lock);
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let mut watch: Value = serde_json::from_slice(&bytes)
            .map_err(|error| anyhow::anyhow!("invalid CI watch {}: {error}", path.display()))?;
        if watch.get("repo").and_then(Value::as_str) != Some(repository)
            || watch.get("branch").and_then(Value::as_str) != Some(branch)
        {
            drop(lock);
            continue;
        }
        watch["review_class"] = json!(new_class.as_token());
        crate::store::atomic_write(&path, serde_json::to_string_pretty(&watch)?.as_bytes())?;
        updated += 1;
        drop(lock);
    }
    Ok(updated)
}

pub(crate) fn handle_correct_review_class(
    home: &Path,
    args: &Value,
    sender: &Option<crate::identity::Sender>,
) -> Value {
    if sender.is_some() {
        return json!({
            "error": "review-class correction is operator-only",
            "code": "review_class_correction_operator_only",
        });
    }
    let action = args["action"].as_str().unwrap_or("");
    if !matches!(action, "preview" | "apply") {
        return json!({
            "error": "review-class correction action must be `preview` or `apply`",
            "code": "review_class_correction_invalid_action",
        });
    }
    let Some(raw_repository) = args["repository"].as_str() else {
        return json!({"error": "review-class correction requires `repository`", "code": "review_class_correction_missing_field"});
    };
    let Some(repository) =
        crate::mcp::handlers::dispatch_hook::canonicalize_repo_slug(raw_repository)
    else {
        return json!({"error": "review-class correction repository is not canonical owner/repo", "code": "review_class_correction_invalid_repository"});
    };
    let Some(branch) = args["branch"].as_str().filter(|value| !value.is_empty()) else {
        return json!({"error": "review-class correction requires `branch`", "code": "review_class_correction_missing_field"});
    };
    let Some(pr_number) = args["pr_number"].as_u64().filter(|value| *value > 0) else {
        return json!({"error": "review-class correction requires nonzero `pr_number`", "code": "review_class_correction_missing_field"});
    };
    let Some(head_sha) = exact_head(args) else {
        return json!({"error": "review-class correction requires a full exact head SHA", "code": "review_class_correction_invalid_head"});
    };
    let Some(operation_id) = args["operation_id"]
        .as_str()
        .filter(|value| valid_operation_id(value))
    else {
        return json!({"error": "review-class correction requires a stable safe `operation_id`", "code": "review_class_correction_invalid_operation_id"});
    };
    let Some(reason) = args["reason"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return json!({"error": "review-class correction requires a nonempty `reason`", "code": "review_class_correction_missing_reason"});
    };
    let expected_old_class = match parse_class(args, "expected_old_class") {
        Ok(class) => class,
        Err(error) => return error,
    };
    let new_class = match parse_class(args, "new_class") {
        Ok(class) => class,
        Err(error) => return error,
    };
    if expected_old_class == new_class {
        return json!({"error": "review-class correction must change the class", "code": "review_class_correction_no_change"});
    }
    // A completed operation is an exact-once result. Check the durable record
    // before the live PR/state snapshot so a retry remains idempotent even
    // though the expected old class is no longer present after success.
    if action == "apply" {
        match load_journal(home, operation_id) {
            Ok(Some(journal)) => {
                if !same_request(
                    &journal,
                    &repository,
                    branch,
                    pr_number,
                    head_sha,
                    expected_old_class,
                    new_class,
                    reason,
                ) {
                    return json!({"error": "operation_id is already bound to a different correction", "code": "review_class_correction_operation_conflict"});
                }
                if journal.phase == "completed" {
                    return json!({"ok": true, "already_applied": true, "operation_id": operation_id, "effect": journal});
                }
                // Recovery is local-only: do not hold the operation lock over
                // the provider snapshot below. A state already carrying the
                // requested class means the prior attempt reached its state
                // effect and only its completion record was interrupted.
                if let Some(state) = crate::daemon::pr_state::load(home, &repository, branch) {
                    if state.repo == repository
                        && state.branch == branch
                        && state.pr_number == pr_number
                        && state.head_sha == head_sha
                        && state.review_class == new_class
                    {
                        let recovery_lock_path =
                            journal_path(home, operation_id).with_extension("lock");
                        let _recovery_lock = match crate::store::acquire_file_lock(
                            &recovery_lock_path,
                        ) {
                            Ok(lock) => lock,
                            Err(error) => {
                                return json!({"error": format!("review-class correction recovery lock failed: {error}"), "code": "review_class_correction_lock_failed"});
                            }
                        };
                        let current = match load_journal(home, operation_id) {
                            Ok(current) => current,
                            Err(error) => return error,
                        };
                        if let Some(current) = current.filter(|current| {
                            same_request(
                                current,
                                &repository,
                                branch,
                                pr_number,
                                head_sha,
                                expected_old_class,
                                new_class,
                                reason,
                            )
                        }) {
                            if current.phase == "completed" {
                                return json!({"ok": true, "already_applied": true, "operation_id": operation_id, "effect": current});
                            }
                            let mut recovered = current;
                            recovered.phase = "completed".to_string();
                            recovered.completed_at = Some(chrono::Utc::now().to_rfc3339());
                            if let Err(error) = crate::store::save_atomic(
                                &journal_path(home, operation_id),
                                &recovered,
                            ) {
                                return json!({"error": format!("review-class correction recovery persistence failed: {error}"), "code": "review_class_correction_completion_unknown"});
                            }
                            return json!({"ok": true, "recovered": true, "operation_id": operation_id, "effect": recovered});
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(error) => return error,
        }
    }
    let state = match validate_snapshot(
        home,
        &repository,
        branch,
        pr_number,
        head_sha,
        expected_old_class,
    ) {
        Ok(state) => state,
        Err(error) => return error,
    };
    if action == "preview" {
        return json!({
            "ok": true,
            "preview": true,
            "operation_id": operation_id,
            "repository": repository,
            "branch": branch,
            "pr_number": pr_number,
            "head_sha": head_sha,
            "old_class": state.review_class.as_token(),
            "new_class": new_class.as_token(),
            "reason": reason,
            "active_assignments": crate::daemon::assignment_authority::list_active(home, &repository, branch).iter().filter(|record| record.pr_number == pr_number && record.reviewed_head.as_deref() == Some(head_sha)).count(),
        });
    }

    let lock_path = journal_path(home, operation_id).with_extension("lock");
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(lock) => lock,
        Err(error) => {
            return json!({"error": format!("review-class correction lock failed: {error}"), "code": "review_class_correction_lock_failed"});
        }
    };
    let existing = match load_journal(home, operation_id) {
        Ok(existing) => existing,
        Err(error) => return error,
    };
    if let Some(journal) = existing.as_ref() {
        if !same_request(
            journal,
            &repository,
            branch,
            pr_number,
            head_sha,
            expected_old_class,
            new_class,
            reason,
        ) {
            return json!({"error": "operation_id is already bound to a different correction", "code": "review_class_correction_operation_conflict"});
        }
        if journal.phase == "completed" {
            return json!({"ok": true, "already_applied": true, "operation_id": operation_id, "effect": journal});
        }
        // A crash after the exact state mutation but before the completion
        // record was published leaves the durable intent behind. The new
        // class is the recovery marker; finish the same operation locally
        // without repeating assignment/receipt effects.
        if let Some(state) = crate::daemon::pr_state::load(home, &repository, branch) {
            if state.repo == repository
                && state.branch == branch
                && state.pr_number == pr_number
                && state.head_sha == head_sha
                && state.review_class == new_class
            {
                let mut recovered = journal.clone();
                recovered.phase = "completed".to_string();
                recovered.completed_at = Some(chrono::Utc::now().to_rfc3339());
                if let Err(error) =
                    crate::store::save_atomic(&journal_path(home, operation_id), &recovered)
                {
                    return json!({"error": format!("review-class correction recovery persistence failed: {error}"), "code": "review_class_correction_completion_unknown"});
                }
                return json!({"ok": true, "recovered": true, "operation_id": operation_id, "effect": recovered});
            }
        }
    }
    let started_at = existing
        .as_ref()
        .map(|journal| journal.started_at.clone())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let mut intent = existing.unwrap_or(CorrectionJournal {
        schema_version: JOURNAL_VERSION,
        operation_id: operation_id.to_string(),
        repository: repository.clone(),
        branch: branch.to_string(),
        pr_number,
        head_sha: head_sha.to_string(),
        expected_old_class,
        new_class,
        reason: reason.to_string(),
        phase: "prepared".to_string(),
        before_class: expected_old_class,
        invalidated_assignments: 0,
        invalidated_buffered_receipts: 0,
        invalidated_persisted_receipts: 0,
        updated_watch_records: 0,
        started_at,
        completed_at: None,
    });
    if let Err(error) = crate::store::save_atomic(&journal_path(home, operation_id), &intent) {
        return json!({"error": format!("review-class correction intent persistence failed: {error}"), "code": "review_class_correction_intent_failed"});
    }

    let updated_watch_records = match update_watch_class(home, &repository, branch, new_class) {
        Ok(count) => count,
        Err(error) => {
            return json!({"error": format!("review-class correction watch update failed: {error}"), "code": "review_class_correction_watch_failed"});
        }
    };
    intent.updated_watch_records = updated_watch_records;
    if let Err(error) = crate::store::save_atomic(&journal_path(home, operation_id), &intent) {
        return json!({"error": format!("review-class correction watch effect persistence failed: {error}"), "code": "review_class_correction_effect_persistence_failed"});
    }
    let now = chrono::Utc::now().to_rfc3339();
    let (invalidated_assignments, invalidated_buffered_receipts) =
        match crate::daemon::assignment_authority::retire_for_review_class_correction(
            home,
            &repository,
            branch,
            pr_number,
            head_sha,
            &now,
        ) {
            Ok(counts) => counts,
            Err(error) => {
                return json!({"error": format!("review-class correction assignment invalidation failed: {error}"), "code": "review_class_correction_assignment_failed"});
            }
        };
    intent.invalidated_assignments = invalidated_assignments;
    intent.invalidated_buffered_receipts = invalidated_buffered_receipts;
    if let Err(error) = crate::store::save_atomic(&journal_path(home, operation_id), &intent) {
        return json!({"error": format!("review-class correction invalidation persistence failed: {error}"), "code": "review_class_correction_effect_persistence_failed"});
    }
    let mut invalidated_persisted_receipts = 0;
    let state_result = crate::daemon::pr_state::with_pr_state(home, &repository, branch, |state| {
        if state.repo != repository
            || state.branch != branch
            || state.pr_number != pr_number
            || state.head_sha != head_sha
            || state.review_class != expected_old_class
        {
            return Err("review-class correction state CAS drift".to_string());
        }
        invalidated_persisted_receipts = state.validated_review_receipts.len();
        state.validated_review_receipts.clear();
        state.reserved_assignments.clear();
        state.verdict_state = crate::daemon::pr_state::VerdictState::Pending;
        state.review_class = new_class;
        state.merge_state = crate::daemon::pr_state::MergeState::NotReady;
        state.review_dispatch_emitted_for_sha = None;
        state.review_dispatch_unavailable_emitted_for_sha = None;
        Ok(())
    });
    match state_result {
        Ok(Some(Ok(()))) => {}
        Ok(Some(Err(error))) => {
            return json!({"error": error, "code": "review_class_correction_state_drift"});
        }
        Ok(None) => {
            return json!({"error": "review-class correction state disappeared", "code": "review_class_correction_state_missing"});
        }
        Err(error) => {
            return json!({"error": format!("review-class correction state write failed: {error}"), "code": "review_class_correction_state_write_failed"});
        }
    }
    intent.invalidated_persisted_receipts = invalidated_persisted_receipts;
    intent.phase = "effects_applied".to_string();
    if let Err(error) = crate::store::save_atomic(&journal_path(home, operation_id), &intent) {
        return json!({"error": format!("review-class correction state effect persistence failed: {error}"), "code": "review_class_correction_effect_persistence_failed"});
    }
    let mut completed = intent;
    completed.phase = "completed".to_string();
    completed.invalidated_assignments = invalidated_assignments;
    completed.invalidated_buffered_receipts = invalidated_buffered_receipts;
    completed.invalidated_persisted_receipts = invalidated_persisted_receipts;
    completed.updated_watch_records = updated_watch_records;
    completed.completed_at = Some(chrono::Utc::now().to_rfc3339());
    if let Err(error) = crate::store::save_atomic(&journal_path(home, operation_id), &completed) {
        return json!({"error": format!("review-class correction completion persistence failed: {error}"), "code": "review_class_correction_completion_unknown"});
    }
    crate::event_log::log(
        home,
        "review_class_correction",
        operation_id,
        &format!(
            "corrected {repository}#{pr_number} {branch} {head_sha}: {} -> {}",
            expected_old_class.as_token(),
            new_class.as_token()
        ),
    );
    json!({"ok": true, "operation_id": operation_id, "effect": completed})
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::scm::{
        CheckState, CompareResult, IssueSummary, ListFilter, MergeOpts, MergeOutcome, PrSummary,
        ScmProvider,
    };
    use serde_json::json;
    use std::sync::Arc;

    const REPO: &str = "owner/repo";
    const BRANCH: &str = "fix/review-class";
    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

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
            _cwd: Option<&std::path::Path>,
        ) -> anyhow::Result<Vec<PrSummary>> {
            Ok(Vec::new())
        }
        fn pr_merge(
            &self,
            _repo: &str,
            _pr: u64,
            _opts: &MergeOpts,
        ) -> anyhow::Result<MergeOutcome> {
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
        let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home)
            .join(crate::daemon::ci_watch::watch_filename(REPO, BRANCH));
        let watch: Value = serde_json::from_slice(&std::fs::read(watch_path).unwrap()).unwrap();
        assert_eq!(watch["review_class"], "single", "{watch}");

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
        assert!(is_incomplete(&home, REPO, BRANCH, 42, HEAD));
        assert!(!is_incomplete(&home, REPO, BRANCH, 43, HEAD));
        assert!(!is_incomplete(&home, REPO, BRANCH, 42, &"f".repeat(40)));
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
}
