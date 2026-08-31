//! Canonical merge review-threshold enforcement — production `handle_merge_repo`
//! entry tests.  The provider is injected at the shared `ScmProvider` seam, but
//! every test drives the actual handler, including exact-head acquisition,
//! freshness, review authority, audit, and merge.

use crate::scm::{
    CheckState, CompareResult, IssueSummary, ListFilter, MergeOpts, MergeOutcome, PrSummary,
    ScmProvider,
};
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};

const REPO: &str = "suzuke/agend-terminal";
const BRANCH: &str = "feat/review-threshold";
const PR: u64 = 4242;
const HEAD: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
const OTHER_HEAD: &str = "feedfacefeedfacefeedfacefeedfacefeedface";
const BASE: &str = "0badf00d0badf00d0badf00d0badf00d0badf00d";

struct MergeMock {
    recorded: Arc<Mutex<Option<MergeOpts>>>,
}

impl MergeMock {
    fn new(recorded: Arc<Mutex<Option<MergeOpts>>>) -> Self {
        Self { recorded }
    }
}

impl ScmProvider for MergeMock {
    fn pr_view(&self, _repo: &str, _pr: u64, fields: &[&str]) -> anyhow::Result<PrSummary> {
        if fields.contains(&"state") {
            return Ok(PrSummary {
                state: Some("MERGED".into()),
                merge_commit_oid: Some("mergecommit0".into()),
                ..Default::default()
            });
        }
        if fields.contains(&"headRefOid") {
            return Ok(PrSummary {
                head_ref_oid: Some(HEAD.into()),
                base_ref_oid: Some(BASE.into()),
                head_ref: Some(BRANCH.into()),
                merge_state_status: Some("CLEAN".into()),
                ..Default::default()
            });
        }
        Ok(PrSummary::default())
    }

    fn pr_checks(&self, _repo: &str, _pr: u64) -> anyhow::Result<Vec<CheckState>> {
        Ok(vec![CheckState {
            name: "CI".into(),
            state: "SUCCESS".into(),
        }])
    }

    fn pr_list(
        &self,
        _repo: &str,
        _filter: &ListFilter,
        _fields: &[&str],
        _cwd: Option<&Path>,
    ) -> anyhow::Result<Vec<PrSummary>> {
        anyhow::bail!("pr_list is not used by the merge handler")
    }

    fn pr_merge(&self, _repo: &str, _pr: u64, opts: &MergeOpts) -> anyhow::Result<MergeOutcome> {
        *self.recorded.lock().unwrap() = Some(opts.clone());
        Ok(MergeOutcome::Submitted)
    }

    fn issue_view(
        &self,
        _repo: &str,
        _number: u64,
        _fields: &[&str],
    ) -> anyhow::Result<IssueSummary> {
        anyhow::bail!("issue_view is not used by the merge handler")
    }

    fn compare(&self, _repo: &str, _base: &str, _head: &str) -> anyhow::Result<CompareResult> {
        Ok(CompareResult::default())
    }
}

fn home(tag: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-review-threshold-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    home
}

fn args() -> serde_json::Value {
    json!({"pr": PR, "repository": REPO})
}

fn force_args() -> serde_json::Value {
    json!({
        "pr": PR,
        "repository": REPO,
        "force": true,
        "force_reason": "review-threshold emergency"
    })
}

fn receipt(
    reviewer: &str,
    reviewed_head: &str,
    review_class: crate::daemon::pr_state::ReviewClass,
) -> crate::review_receipt::ReviewReceiptSummary {
    let source_id = format!("source-{reviewer}");
    crate::review_receipt::ReviewReceiptSummary {
        receipt_id: format!("review-receipt:{source_id}"),
        source_id,
        evidence_digest: "a".repeat(64),
        assignment_id: uuid::Uuid::new_v4(),
        reviewer_instance_id: crate::types::InstanceId::new(),
        reviewer_name: reviewer.into(),
        repo: REPO.into(),
        pr_number: PR,
        branch: BRANCH.into(),
        task_id: "t-review-threshold".into(),
        reviewed_head: reviewed_head.into(),
        review_class,
        slot: if reviewer == "reviewer-1" {
            crate::review_receipt::ReviewSlot::Primary
        } else {
            crate::review_receipt::ReviewSlot::Secondary
        },
        verdict: crate::review_receipt::ReviewVerdict::Verified,
    }
}

fn state(
    review_class: crate::daemon::pr_state::ReviewClass,
    receipts: Vec<crate::review_receipt::ReviewReceiptSummary>,
) -> crate::daemon::pr_state::PrState {
    let mut state = crate::daemon::pr_state::new_for_branch(REPO, BRANCH, HEAD, review_class);
    state.pr_number = PR;
    state.ci_state = crate::daemon::pr_state::CiState::Green {
        sha: HEAD.into(),
        observed_at: "2026-08-31T00:00:00Z".into(),
    };
    state.validated_review_receipts = receipts;
    state
}

fn seed(home: &Path, state: &crate::daemon::pr_state::PrState) {
    crate::daemon::pr_state::save(home, state).unwrap();
}

fn install_provider(recorded: Arc<Mutex<Option<MergeOpts>>>) -> impl Drop {
    crate::scm::set_test_scm_provider(Arc::new(MergeMock::new(recorded)))
}

/// RED 1: no canonical PrState linkage must refuse before `pr_merge`.
#[test]
fn merge_without_pr_state_linkage_refuses() {
    let home = home("no-linkage");
    let recorded = Arc::new(Mutex::new(None));
    let _g = install_provider(recorded.clone());

    let result = super::handle_merge_repo(&home, &args(), "lead");

    assert_eq!(result["code"], "no_linkage", "{result}");
    assert!(recorded.lock().unwrap().is_none(), "{result}");
    std::fs::remove_dir_all(home).ok();
}

/// RED 2: an unresolved review class is never treated as Single.
#[test]
fn unresolved_review_class_refuses() {
    let home = home("unresolved");
    let recorded = Arc::new(Mutex::new(None));
    let _g = install_provider(recorded.clone());
    seed(
        &home,
        &state(crate::daemon::pr_state::ReviewClass::Unresolved, Vec::new()),
    );

    let result = super::handle_merge_repo(&home, &args(), "lead");

    assert_eq!(result["code"], "unresolved_class", "{result}");
    assert!(recorded.lock().unwrap().is_none(), "{result}");
    std::fs::remove_dir_all(home).ok();
}

/// RED 3: Dual with one exact-head VERIFIED receipt reports the N-of-M deficit.
#[test]
fn dual_with_one_verified_refuses_with_deficit() {
    let home = home("one-verified");
    let recorded = Arc::new(Mutex::new(None));
    let _g = install_provider(recorded.clone());
    seed(
        &home,
        &state(
            crate::daemon::pr_state::ReviewClass::Dual,
            vec![receipt(
                "reviewer-1",
                HEAD,
                crate::daemon::pr_state::ReviewClass::Dual,
            )],
        ),
    );

    let result = super::handle_merge_repo(&home, &args(), "lead");

    assert_eq!(result["code"], "insufficient_verified", "{result}");
    assert_eq!(result["verified_count"], 1, "{result}");
    assert_eq!(result["required_verified_count"], 2, "{result}");
    assert!(recorded.lock().unwrap().is_none(), "{result}");
    std::fs::remove_dir_all(home).ok();
}

/// RED 4: a receipt for an older head is an explicit stale-head refusal, not a
/// generic missing-verdict response.
#[test]
fn stale_review_receipt_refuses_with_stale_head() {
    let home = home("stale-head");
    let recorded = Arc::new(Mutex::new(None));
    let _g = install_provider(recorded.clone());
    seed(
        &home,
        &state(
            crate::daemon::pr_state::ReviewClass::Dual,
            vec![receipt(
                "reviewer-1",
                OTHER_HEAD,
                crate::daemon::pr_state::ReviewClass::Dual,
            )],
        ),
    );

    let result = super::handle_merge_repo(&home, &args(), "lead");

    assert_eq!(result["code"], "stale_head", "{result}");
    assert!(recorded.lock().unwrap().is_none(), "{result}");
    std::fs::remove_dir_all(home).ok();
}

/// GREEN 5: two distinct exact-head VERIFIED receipts satisfy Dual and reach
/// the existing merge write, which still receives the exact-head pin.
#[test]
fn dual_with_two_distinct_exact_head_verified_merges() {
    let home = home("two-verified");
    let recorded = Arc::new(Mutex::new(None));
    let _g = install_provider(recorded.clone());
    seed(
        &home,
        &state(
            crate::daemon::pr_state::ReviewClass::Dual,
            vec![
                receipt(
                    "reviewer-1",
                    HEAD,
                    crate::daemon::pr_state::ReviewClass::Dual,
                ),
                receipt(
                    "reviewer-2",
                    HEAD,
                    crate::daemon::pr_state::ReviewClass::Dual,
                ),
            ],
        ),
    );

    let result = super::handle_merge_repo(&home, &args(), "lead");

    assert_eq!(result["merged"], true, "{result}");
    assert_eq!(
        recorded
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|opts| opts.expected_head_sha.as_deref()),
        Some(HEAD)
    );
    std::fs::remove_dir_all(home).ok();
}

/// GREEN 6: force remains the sole review-threshold bypass and its audit record
/// carries the review class, counts, reviewer identities, and receipt heads.
#[test]
fn force_merge_audit_records_review_threshold_evidence() {
    let home = home("force-audit");
    let recorded = Arc::new(Mutex::new(None));
    let _g = install_provider(recorded.clone());
    seed(
        &home,
        &state(
            crate::daemon::pr_state::ReviewClass::Dual,
            vec![receipt(
                "reviewer-1",
                HEAD,
                crate::daemon::pr_state::ReviewClass::Dual,
            )],
        ),
    );

    let result = super::handle_merge_repo(&home, &force_args(), "lead");
    assert_eq!(result["merged"], true, "{result}");
    let audit = std::fs::read_to_string(home.join("fleet_events.jsonl")).unwrap();
    let event: serde_json::Value = serde_json::from_str(audit.trim()).unwrap();
    assert_eq!(event["kind"], "merge_force_bypass");
    assert_eq!(event["review_class"], "dual");
    assert_eq!(event["verified_count"], 1);
    assert_eq!(event["required_verified_count"], 2);
    assert_eq!(event["reviewer_identities"][0], "reviewer-1");
    assert_eq!(event["receipt_heads"][0], HEAD);
    std::fs::remove_dir_all(home).ok();
}
