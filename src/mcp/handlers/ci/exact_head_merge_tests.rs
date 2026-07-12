//! P0 exact-head merge precondition — production `handle_merge_repo` race tests
//! (test-first). Deterministic race reproduction via an injected mock
//! `ScmProvider` returning sequenced snapshots (no timing/sleep): the mock is
//! installed at the shared `make_scm_provider` seam, so every provider call the
//! handler makes hits the SAME instance and its per-call counter advances in
//! order. RED against d68341a2 (handler does not yet acquire/recheck/pin the
//! head+base); GREEN once `expected_head_sha` + one-shot head+base identity
//! recheck + fail-closed land.
//!
//! Base freshness is proven by EXACT base identity (`baseRefOid`), NOT
//! `mergeStateStatus` (derived + laggy): the base-move test advances the base OID
//! while `mergeStateStatus` stays CLEAN, so a status-only check would miss it.

use crate::scm::{
    CheckState, CompareResult, IssueSummary, ListFilter, MergeOpts, MergeOutcome, PrSummary,
    ScmProvider,
};
use serde_json::json;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Sequenced, field-dispatched mock. Every `pr_view` that requests `headRefOid`
/// walks `(heads, bases)` in lockstep (one shared counter; last value repeated).
/// `compare` is constant-Fresh. `pr_merge` records its `MergeOpts`.
struct MergeMock {
    heads: Vec<&'static str>,
    bases: Vec<&'static str>,
    hb_calls: AtomicUsize,
    head_err: bool,
    merge_state: &'static str,
    checks_pass: bool,
    recorded: Arc<Mutex<Option<MergeOpts>>>,
}

impl MergeMock {
    fn new(recorded: Arc<Mutex<Option<MergeOpts>>>) -> Self {
        Self {
            heads: vec![H0],
            bases: vec![B0],
            hb_calls: AtomicUsize::new(0),
            head_err: false,
            merge_state: "CLEAN",
            checks_pass: true,
            recorded,
        }
    }
}

impl ScmProvider for MergeMock {
    fn pr_view(&self, _r: &str, _p: u64, fields: &[&str]) -> anyhow::Result<PrSummary> {
        // verify_merge_landed reads state+mergeCommit → report a landed PR.
        if fields.contains(&"state") {
            return Ok(PrSummary {
                state: Some("MERGED".into()),
                merge_commit_oid: Some("mergecommit0".into()),
                ..Default::default()
            });
        }
        if fields.contains(&"headRefOid") {
            if self.head_err {
                anyhow::bail!("gh pr view headRefOid failed (simulated)");
            }
            let i = self
                .hb_calls
                .fetch_add(1, Ordering::SeqCst)
                .min(self.heads.len().max(self.bases.len()) - 1);
            return Ok(PrSummary {
                head_ref_oid: Some(self.heads[i.min(self.heads.len() - 1)].to_string()),
                base_ref_oid: Some(self.bases[i.min(self.bases.len() - 1)].to_string()),
                ..Default::default()
            });
        }
        if fields.contains(&"mergeStateStatus") {
            return Ok(PrSummary {
                merge_state_status: Some(self.merge_state.into()),
                ..Default::default()
            });
        }
        Ok(PrSummary::default())
    }

    fn pr_checks(&self, _r: &str, _p: u64) -> anyhow::Result<Vec<CheckState>> {
        let state = if self.checks_pass { "SUCCESS" } else { "FAILURE" };
        Ok(vec![CheckState {
            name: "CI".into(),
            state: state.into(),
        }])
    }

    fn pr_list(
        &self,
        _r: &str,
        _f: &ListFilter,
        _fl: &[&str],
        _c: Option<&Path>,
    ) -> anyhow::Result<Vec<PrSummary>> {
        unimplemented!("MergeMock::pr_list not used by the merge path")
    }

    fn pr_merge(&self, _r: &str, _p: u64, opts: &MergeOpts) -> anyhow::Result<MergeOutcome> {
        *self.recorded.lock().unwrap() = Some(opts.clone());
        Ok(MergeOutcome::Submitted)
    }

    fn issue_view(&self, _r: &str, _n: u64, _f: &[&str]) -> anyhow::Result<IssueSummary> {
        unimplemented!("MergeMock::issue_view not used by the merge path")
    }

    fn compare(&self, _r: &str, _b: &str, _h: &str) -> anyhow::Result<CompareResult> {
        Ok(CompareResult::default()) // behind_by 0 → merge_freshness Fresh
    }
}

fn tmp_home(tag: &str) -> std::path::PathBuf {
    let h = std::env::temp_dir().join(format!(
        "agend-exact-head-merge-{}-{}",
        std::process::id(),
        tag
    ));
    std::fs::create_dir_all(&h).unwrap();
    h
}

fn base_args() -> serde_json::Value {
    json!({"pr": 4242, "repository": "suzuke/agend-terminal"})
}

fn force_args() -> serde_json::Value {
    json!({"pr": 4242, "repository": "suzuke/agend-terminal", "force": true, "force_reason": "emergency"})
}

const H0: &str = "h0h0h0h0h0h0h0h0h0h0h0h0h0h0h0h0h0h0h0h0";
const H1: &str = "d1ffd1ffd1ffd1ffd1ffd1ffd1ffd1ffd1ffd1ff";
const B0: &str = "ba5eba5eba5eba5eba5eba5eba5eba5eba5eba5e0";
const B1: &str = "ba5eADVANCEba5eADVANCEba5eADVANCEba5eADV1";

fn refused(r: &serde_json::Value) -> bool {
    r.get("error").is_some() && r["merged"].as_bool() != Some(true)
}

/// P0-4: a normal merge PINS the exact acquired head — `pr_merge` receives
/// `expected_head_sha == acquired head`. RED: handler passes no pin.
#[test]
fn normal_merge_pins_expected_head_sha() {
    let home = tmp_home("pin");
    let recorded = Arc::new(Mutex::new(None));
    let _g = crate::scm::set_test_scm_provider(Arc::new(MergeMock::new(recorded.clone())));
    let r = super::handle_merge_repo(&home, &base_args(), "dev");
    assert_eq!(r["merged"].as_bool(), Some(true), "should merge: {r}");
    let opts = recorded.lock().unwrap().clone().expect("pr_merge was called");
    assert_eq!(
        opts.expected_head_sha.as_deref(),
        Some(H0),
        "P0-4: merge must be pinned to the acquired head; got {:?}",
        opts.expected_head_sha
    );
    std::fs::remove_dir_all(&home).ok();
}

/// P0-1: head moves between gate read and write (H0 → H1) → REFUSE, no merge.
#[test]
fn head_move_between_gate_and_write_refuses() {
    let home = tmp_home("headmove");
    let recorded = Arc::new(Mutex::new(None));
    let mut mock = MergeMock::new(recorded.clone());
    mock.heads = vec![H0, H0, H1]; // acquire, merge_freshness, recheck → moved
    mock.bases = vec![B0];
    let _g = crate::scm::set_test_scm_provider(Arc::new(mock));
    let r = super::handle_merge_repo(&home, &base_args(), "dev");
    assert!(refused(&r), "P0-1: a moved head must REFUSE, got: {r}");
    assert!(recorded.lock().unwrap().is_none(), "P0-1: pr_merge must NOT run");
    std::fs::remove_dir_all(&home).ok();
}

/// P0-2: base OID advances (B0 → B1) while mergeStateStatus stays CLEAN → REFUSE.
/// Proves the recheck uses EXACT base identity, not the derived/laggy status.
#[test]
fn base_oid_move_with_clean_status_refuses() {
    let home = tmp_home("basemove");
    let recorded = Arc::new(Mutex::new(None));
    let mut mock = MergeMock::new(recorded.clone());
    mock.heads = vec![H0]; // head stable
    mock.bases = vec![B0, B0, B1]; // acquire, merge_freshness, recheck → base moved
    mock.merge_state = "CLEAN"; // status UNCHANGED — a status-only check would miss it
    let _g = crate::scm::set_test_scm_provider(Arc::new(mock));
    let r = super::handle_merge_repo(&home, &base_args(), "dev");
    assert!(
        refused(&r),
        "P0-2: a base-OID advance (status still CLEAN) must REFUSE, got: {r}"
    );
    assert!(recorded.lock().unwrap().is_none(), "P0-2: pr_merge must NOT run");
    std::fs::remove_dir_all(&home).ok();
}

/// P0-2b: force must NOT bypass the base-identity recheck. Base OID advances under
/// force (mergeStateStatus CLEAN) → REFUSE. Force relaxes freshness POLICY, never
/// the head/base identity atomicity.
#[test]
fn base_oid_move_under_force_still_refuses() {
    let home = tmp_home("basemove-force");
    let recorded = Arc::new(Mutex::new(None));
    let mut mock = MergeMock::new(recorded.clone());
    mock.heads = vec![H0]; // head stable
    mock.bases = vec![B0, B1]; // force skips merge_freshness → acquire, recheck
    let _g = crate::scm::set_test_scm_provider(Arc::new(mock));
    let r = super::handle_merge_repo(&home, &force_args(), "dev");
    assert!(
        refused(&r),
        "P0-2b: force must NOT bypass the base-identity recheck, got: {r}"
    );
    assert!(
        recorded.lock().unwrap().is_none(),
        "P0-2b: pr_merge must NOT run under force when base moved"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// P0-3: head lookup fails AND force=true → fail-closed. Force relaxes policy,
/// never the non-bypassable head acquisition/pin.
#[test]
fn head_lookup_failure_with_force_fails_closed() {
    let home = tmp_home("headfail-force");
    let recorded = Arc::new(Mutex::new(None));
    let mut mock = MergeMock::new(recorded.clone());
    mock.head_err = true;
    let _g = crate::scm::set_test_scm_provider(Arc::new(mock));
    let r = super::handle_merge_repo(&home, &force_args(), "dev");
    assert!(refused(&r), "P0-3: head unknown + force must fail closed, got: {r}");
    assert!(recorded.lock().unwrap().is_none(), "P0-3: pr_merge must NOT run");
    std::fs::remove_dir_all(&home).ok();
}

/// Unsupported/erroring provider (cannot yield the head) → fail-closed, never a
/// silently-unpinned merge.
#[test]
fn head_unavailable_provider_fails_closed() {
    let home = tmp_home("headfail-nonforce");
    let recorded = Arc::new(Mutex::new(None));
    let mut mock = MergeMock::new(recorded.clone());
    mock.head_err = true;
    let _g = crate::scm::set_test_scm_provider(Arc::new(mock));
    let r = super::handle_merge_repo(&home, &base_args(), "dev");
    assert!(refused(&r), "erroring provider must fail closed, got: {r}");
    assert!(recorded.lock().unwrap().is_none(), "pr_merge must NOT run");
    std::fs::remove_dir_all(&home).ok();
}
