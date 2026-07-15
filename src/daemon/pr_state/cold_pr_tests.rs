use super::*;
use crate::scm::{self, PrSummary, ScmProvider};
use std::path::Path;
use std::sync::Arc;

struct ColdPrMock {
    pr_number: u64,
    head_sha: String,
    head_ref: String,
    author: String,
}

impl ScmProvider for ColdPrMock {
    fn pr_view(&self, _r: &str, pr: u64, _f: &[&str]) -> anyhow::Result<PrSummary> {
        if pr != self.pr_number {
            anyhow::bail!("PR #{pr} not found");
        }
        Ok(PrSummary {
            number: self.pr_number,
            head_ref_oid: Some(self.head_sha.clone()),
            head_ref: Some(self.head_ref.clone()),
            author_login: Some(self.author.clone()),
            ..Default::default()
        })
    }
    fn pr_checks(&self, _r: &str, _p: u64) -> anyhow::Result<Vec<scm::CheckState>> {
        unimplemented!()
    }
    fn pr_list(
        &self,
        _r: &str,
        _f: &scm::ListFilter,
        _fl: &[&str],
        _c: Option<&Path>,
    ) -> anyhow::Result<Vec<PrSummary>> {
        unimplemented!()
    }
    fn pr_merge(
        &self,
        _r: &str,
        _p: u64,
        _o: &scm::MergeOpts,
    ) -> anyhow::Result<scm::MergeOutcome> {
        unimplemented!()
    }
    fn issue_view(&self, _r: &str, _n: u64, _f: &[&str]) -> anyhow::Result<scm::IssueSummary> {
        unimplemented!()
    }
    fn compare(&self, _r: &str, _b: &str, _h: &str) -> anyhow::Result<scm::CompareResult> {
        unimplemented!()
    }
}

struct NotFoundMock;

impl ScmProvider for NotFoundMock {
    fn pr_view(&self, _r: &str, pr: u64, _f: &[&str]) -> anyhow::Result<PrSummary> {
        anyhow::bail!("PR #{pr} not found (404)")
    }
    fn pr_checks(&self, _r: &str, _p: u64) -> anyhow::Result<Vec<scm::CheckState>> {
        unimplemented!()
    }
    fn pr_list(
        &self,
        _r: &str,
        _f: &scm::ListFilter,
        _fl: &[&str],
        _c: Option<&Path>,
    ) -> anyhow::Result<Vec<PrSummary>> {
        unimplemented!()
    }
    fn pr_merge(
        &self,
        _r: &str,
        _p: u64,
        _o: &scm::MergeOpts,
    ) -> anyhow::Result<scm::MergeOutcome> {
        unimplemented!()
    }
    fn issue_view(&self, _r: &str, _n: u64, _f: &[&str]) -> anyhow::Result<scm::IssueSummary> {
        unimplemented!()
    }
    fn compare(&self, _r: &str, _b: &str, _h: &str) -> anyhow::Result<scm::CompareResult> {
        unimplemented!()
    }
}

fn tmp_home(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "agend-cold-pr-{name}-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// #2800 T1: cold PR (no pr-state) → ensure_from_scm creates Pending pr-state.
#[test]
fn cold_pr_creates_pending_pr_state() {
    let home = tmp_home("t1-cold-create");
    let head = "a".repeat(40);
    let _guard = scm::set_test_scm_provider(Arc::new(ColdPrMock {
        pr_number: 42,
        head_sha: head.clone(),
        head_ref: "feat/x".into(),
        author: "dev".into(),
    }));

    let state = ensure_from_scm(&home, "o/r", "feat/x", 42, &head, ReviewClass::Dual)
        .expect("cold PR ensure must succeed");

    assert_eq!(state.repo, "o/r");
    assert_eq!(state.branch, "feat/x");
    assert_eq!(state.pr_number, 42);
    assert_eq!(state.head_sha, head);
    assert!(
        matches!(state.ci_state, CiState::Pending),
        "cold PR must start with CiState::Pending"
    );
    assert!(
        matches!(state.merge_state, MergeState::NotReady),
        "cold PR must not be merge-ready"
    );
    assert_eq!(state.review_class, ReviewClass::Dual);
    assert_eq!(state.pr_author, "dev");
    std::fs::remove_dir_all(&home).ok();
}

/// #2800 T2: cold PR with wrong head_sha → SCM mismatch → fails closed.
#[test]
fn cold_pr_wrong_head_fails_closed() {
    let home = tmp_home("t2-wrong-head");
    let _guard = scm::set_test_scm_provider(Arc::new(ColdPrMock {
        pr_number: 42,
        head_sha: "b".repeat(40),
        head_ref: "feat/x".into(),
        author: "dev".into(),
    }));

    let result = ensure_from_scm(
        &home,
        "o/r",
        "feat/x",
        42,
        &"a".repeat(40),
        ReviewClass::Dual,
    );

    assert!(result.is_err(), "head mismatch must fail closed");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("SCM head mismatch"),
        "error must mention head mismatch"
    );
    assert!(
        load(&home, "o/r", "feat/x").is_none(),
        "no pr-state file created on mismatch"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2800 T3: cold PR with non-existent PR number → SCM not found → fails closed.
#[test]
fn cold_pr_not_found_fails_closed() {
    let home = tmp_home("t3-not-found");
    let _guard = scm::set_test_scm_provider(Arc::new(NotFoundMock));

    let result = ensure_from_scm(
        &home,
        "o/r",
        "feat/x",
        999,
        &"a".repeat(40),
        ReviewClass::Dual,
    );

    assert!(result.is_err(), "non-existent PR must fail closed");
    assert!(
        load(&home, "o/r", "feat/x").is_none(),
        "no pr-state file created for non-existent PR"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2800 T4: existing pr-state with matching identity → ensure is no-op.
#[test]
fn existing_pr_state_is_noop() {
    let home = tmp_home("t4-existing");
    let head = "c".repeat(40);
    let mut s = new_for_branch("o/r", "feat/x", &head, ReviewClass::Single);
    s.pr_number = 10;
    s.ci_state = CiState::Green {
        sha: head.clone(),
        observed_at: "2026-07-15T00:00:00Z".into(),
    };
    save(&home, &s).unwrap();

    // No mock needed — should hit fast path without SCM call.
    let state = ensure_from_scm(&home, "o/r", "feat/x", 10, &head, ReviewClass::Single)
        .expect("existing pr-state must succeed");

    assert!(
        matches!(state.ci_state, CiState::Green { .. }),
        "existing CiState must be preserved (not overwritten to Pending)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2800 T5: idempotent — two ensure calls for the same cold PR converge.
#[test]
fn cold_pr_ensure_idempotent() {
    let home = tmp_home("t5-idempotent");
    let head = "d".repeat(40);
    let _guard = scm::set_test_scm_provider(Arc::new(ColdPrMock {
        pr_number: 7,
        head_sha: head.clone(),
        head_ref: "feat/x".into(),
        author: "dev".into(),
    }));

    let s1 = ensure_from_scm(&home, "o/r", "feat/x", 7, &head, ReviewClass::Dual).unwrap();
    let s2 = ensure_from_scm(&home, "o/r", "feat/x", 7, &head, ReviewClass::Dual).unwrap();

    assert_eq!(s1.head_sha, s2.head_sha);
    assert_eq!(s1.pr_number, s2.pr_number);
    std::fs::remove_dir_all(&home).ok();
}

/// #2800 T6: correct PR/SHA but wrong branch → SCM branch mismatch → fails closed.
/// Regression: a real PR with matching head_sha but different head_ref must not
/// create a mis-keyed pr-state file.
#[test]
fn cold_pr_wrong_branch_fails_closed() {
    let home = tmp_home("t6-wrong-branch");
    let head = "e".repeat(40);

    struct WrongBranchMock(String);
    impl ScmProvider for WrongBranchMock {
        fn pr_view(&self, _r: &str, _p: u64, _f: &[&str]) -> anyhow::Result<PrSummary> {
            Ok(PrSummary {
                number: 50,
                head_ref_oid: Some(self.0.clone()),
                head_ref: Some("other/branch".into()),
                author_login: Some("dev".into()),
                ..Default::default()
            })
        }
        fn pr_checks(&self, _r: &str, _p: u64) -> anyhow::Result<Vec<scm::CheckState>> {
            unimplemented!()
        }
        fn pr_list(
            &self,
            _r: &str,
            _f: &scm::ListFilter,
            _fl: &[&str],
            _c: Option<&Path>,
        ) -> anyhow::Result<Vec<PrSummary>> {
            unimplemented!()
        }
        fn pr_merge(
            &self,
            _r: &str,
            _p: u64,
            _o: &scm::MergeOpts,
        ) -> anyhow::Result<scm::MergeOutcome> {
            unimplemented!()
        }
        fn issue_view(&self, _r: &str, _n: u64, _f: &[&str]) -> anyhow::Result<scm::IssueSummary> {
            unimplemented!()
        }
        fn compare(&self, _r: &str, _b: &str, _h: &str) -> anyhow::Result<scm::CompareResult> {
            unimplemented!()
        }
    }

    let _guard = scm::set_test_scm_provider(Arc::new(WrongBranchMock(head.clone())));

    let result = ensure_from_scm(&home, "o/r", "feat/x", 50, &head, ReviewClass::Dual);

    assert!(result.is_err(), "branch mismatch must fail closed");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("SCM branch mismatch"),
        "error must mention branch mismatch"
    );
    assert!(
        load(&home, "o/r", "feat/x").is_none(),
        "no pr-state file created on branch mismatch"
    );
    std::fs::remove_dir_all(&home).ok();
}
