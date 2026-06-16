//! #2140: deterministic merge-freshness gate — defense-in-depth alongside
//! `ci::base_drift_refusal`.
//!
//! **Division of labour** (the two gates are deliberately complementary):
//! - `base_drift_refusal` (ci/merge.rs) = a LOOSE, best-effort all-behind guard
//!   keyed on GitHub's `mergeStateStatus` (`BEHIND`/`DIRTY`). It defends the
//!   #1798 phantom-reversion class, but `mergeStateStatus` is GitHub's
//!   eventually-consistent cache — right after an interleaved merge it can still
//!   read `CLEAN`, which is exactly how #2137 slipped a stale-base PR onto main
//!   and broke a whole-tree invariant (file_size) → blocked every PR's CI.
//! - THIS gate = a TIGHT, DETERMINISTIC guard keyed on git commit-ancestry
//!   (`gh api .../compare`), NOT on `mergeStateStatus`. It refuses ONLY when the
//!   PR is behind main AND the change set (the PR's own files OR the behind-main
//!   commits' files) touches a whole-tree-invariant input — so the common case
//!   pays zero friction, but the #2137 interleave class is closed.
//!
//! Why BOTH sides of the diff: #2137 grew `ci/mod.rs` and #2130 set the ceiling
//! in `tests/file_size_invariant.rs`. The violation is the COMBINATION — when the
//! second PR merges behind the first, looking only at the second PR's own files
//! would miss it (the first PR's `ci/mod.rs` growth is in the *behind-main*
//! commits). So the gate inspects both the PR's files and the behind-main files.

use crate::scm::ScmProvider;
use serde_json::{json, Value};

/// A whole-tree invariant's INPUT files — the ones whose combined change across
/// interleaved PRs can violate a tree-wide check. The grandfathered handler
/// paths come from the single source of truth in
/// [`crate::invariant_inputs::GRANDFATHERED_OVERSIZED_HANDLERS`] (the same list
/// `tests/file_size_invariant.rs::KNOWN_OVERSIZED` records ceilings for), plus
/// every invariant *test* itself (a PR that tightens an invariant can RED main
/// the same way a ceiling-setter did).
pub(super) fn is_invariant_input(path: &str) -> bool {
    crate::invariant_inputs::GRANDFATHERED_OVERSIZED_HANDLERS.contains(&path)
        || (path.starts_with("tests/") && path.ends_with("_invariant.rs"))
}

/// Verdict of the freshness gate. `StaleRisky` carries the offending
/// invariant-input files (surfaced in the refusal so the operator sees WHY).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FreshnessVerdict {
    Fresh,
    StaleRisky { behind_by: u64, files: Vec<String> },
}

/// PURE classification — the testable core (tests drive it with synthetic compare
/// data, no `gh`). `pr_files` = the PR's own changed paths (`base...head`);
/// `behind_files` = the paths the behind-main commits changed (`head...base`).
/// `StaleRisky` iff the PR is behind AND either set touches a whole-tree-invariant
/// input. It NEVER consults `mergeStateStatus` — that is precisely the laggy
/// signal #2137 slipped past; the verdict is the deterministic ancestry alone.
pub(super) fn classify_freshness(
    behind_by: u64,
    pr_files: &[String],
    behind_files: &[String],
) -> FreshnessVerdict {
    if behind_by == 0 {
        return FreshnessVerdict::Fresh; // up-to-date
    }
    let mut hit: Vec<String> = pr_files
        .iter()
        .chain(behind_files.iter())
        .filter(|f| is_invariant_input(f))
        .cloned()
        .collect();
    if hit.is_empty() {
        // Behind, but no whole-tree-invariant input touched → not this class →
        // allow (zero friction; base_drift_refusal still best-effort-guards #1798).
        FreshnessVerdict::Fresh
    } else {
        hit.sort();
        hit.dedup();
        FreshnessVerdict::StaleRisky {
            behind_by,
            files: hit,
        }
    }
}

/// Orchestrator: two DETERMINISTIC `compare` calls → [`classify_freshness`].
/// Fail-OPEN on any API error — a transient gh/network failure must NOT block a
/// real merge (mirrors `base_drift_refusal`'s fail-open + the #813 mergeable
/// pattern). The second `compare` (behind-main files) is skipped when the PR is
/// already up-to-date (the common case → one API call).
pub(super) fn check_merge_freshness(
    provider: &dyn ScmProvider,
    repo: &str,
    head: &str,
    base: &str,
) -> FreshnessVerdict {
    let fwd = match provider.compare(repo, base, head) {
        Ok(c) => c,
        Err(_) => return FreshnessVerdict::Fresh, // fail-open (transient)
    };
    if fwd.behind_by == 0 {
        return FreshnessVerdict::Fresh; // up-to-date — skip the second call
    }
    let behind_files = provider
        .compare(repo, head, base)
        .map(|c| c.files)
        .unwrap_or_default();
    classify_freshness(fwd.behind_by, &fwd.files, &behind_files)
}

/// #2140: the `handle_merge_repo` entry point — kept all-in-one here so ci/mod.rs
/// (just trimmed to its LOC ceiling) gains only a single call. Resolves the PR
/// head SHA, runs the deterministic freshness check against `main`, and returns
/// the refusal JSON when the PR is stale on a whole-tree-invariant input — else
/// `None` (proceed to merge). Fail-open: an unresolvable head / API error → `None`
/// (a transient gh hiccup must not block a real merge; `check_merge_freshness`
/// already fails open internally).
pub(super) fn gate(repo: &str, pr: u64) -> Option<Value> {
    let provider = crate::scm::make_scm_provider(repo, None);
    let head = provider
        .pr_view(repo, pr, &["headRefOid"])
        .ok()?
        .head_ref_oid?;
    match check_merge_freshness(provider.as_ref(), repo, &head, "main") {
        FreshnessVerdict::Fresh => None,
        FreshnessVerdict::StaleRisky { behind_by, files } => Some(json!({
            "error": format!(
                "base is stale on a whole-tree-invariant input — merge refused (#2140): PR is \
                 behind main by {behind_by} commit(s) and the interleaved change set touches {}. \
                 Merging would risk RED-ing main's tree-wide checks (e.g. file_size_invariant).",
                files.join(", ")
            ),
            "hint": "rebase onto current main + let CI re-run: git fetch && git rebase origin/main \
                     && git push --force-with-lease; or force=true with force_reason to bypass",
            "behind_by": behind_by,
            "invariant_input_files": files,
        })),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::scm::{
        CheckState, CompareResult, IssueSummary, ListFilter, MergeOpts, MergeOutcome, PrSummary,
    };
    use std::path::Path;

    fn s(v: &str) -> String {
        v.to_string()
    }

    // ── is_invariant_input ──
    #[test]
    fn invariant_input_matches_known_oversized_and_invariant_tests() {
        assert!(is_invariant_input("src/mcp/handlers/dispatch_hook/mod.rs"));
        assert!(is_invariant_input("src/mcp/handlers/dispatch_hook/mod.rs"));
        assert!(is_invariant_input("tests/file_size_invariant.rs"));
        assert!(is_invariant_input("tests/git_subprocess_invariant.rs"));
        // Not invariant inputs.
        assert!(!is_invariant_input("src/daemon/pr_state/scanner.rs"));
        assert!(!is_invariant_input("tests/integration.rs"));
        assert!(!is_invariant_input("src/mcp/handlers/comms.rs"));
    }

    // ── §3.10 (d): up-to-date → Fresh (even if it touches an invariant input) ──
    #[test]
    fn up_to_date_is_fresh_even_touching_invariant_2140() {
        assert_eq!(
            classify_freshness(0, &[s("src/mcp/handlers/dispatch_hook/mod.rs")], &[]),
            FreshnessVerdict::Fresh
        );
    }

    // ── §3.10 (a): behind + the PR's OWN files touch an invariant → StaleRisky ──
    #[test]
    fn behind_and_pr_touches_invariant_is_stale_2140() {
        assert_eq!(
            classify_freshness(
                2,
                &[s("src/mcp/handlers/dispatch_hook/mod.rs")],
                &[s("src/other.rs")]
            ),
            FreshnessVerdict::StaleRisky {
                behind_by: 2,
                files: vec![s("src/mcp/handlers/dispatch_hook/mod.rs")]
            }
        );
    }

    // ── §3.10 (a'): behind + the BEHIND-MAIN files touch an invariant (the #2130
    // case: the SECOND PR's own files are clean, but the first PR it's behind grew
    // ci/mod.rs) → StaleRisky. Proves BOTH sides of the diff are inspected. ──
    #[test]
    fn behind_and_behind_main_touches_invariant_is_stale_2140() {
        assert_eq!(
            classify_freshness(
                1,
                &[s("tests/file_size_invariant.rs")], // PR's own (e.g. ceiling-setter)
                &[s("src/mcp/handlers/dispatch_hook/mod.rs")], // behind-main (the file-grower)
            ),
            FreshnessVerdict::StaleRisky {
                behind_by: 1,
                // both invariant-input files surfaced, deduped + sorted
                files: vec![
                    s("src/mcp/handlers/dispatch_hook/mod.rs"),
                    s("tests/file_size_invariant.rs")
                ]
            }
        );
    }

    // ── §3.10 (b): behind but NEITHER side touches an invariant → Fresh (zero
    // friction — the gate does not convoy ordinary churn). ──
    #[test]
    fn behind_but_no_invariant_input_is_fresh_2140() {
        assert_eq!(
            classify_freshness(5, &[s("src/daemon/foo.rs")], &[s("src/bar.rs")]),
            FreshnessVerdict::Fresh
        );
    }

    // ── A configurable mock provider — only `compare` is exercised; the other
    // ScmProvider verbs error (the gate never calls them). ──
    struct MockScm {
        // (base...head) and (head...base) responses, in call order.
        fwd: anyhow::Result<CompareResult>,
        rev: anyhow::Result<CompareResult>,
        calls: std::sync::atomic::AtomicU32,
    }
    impl MockScm {
        fn new(fwd: CompareResult, rev: CompareResult) -> Self {
            Self {
                fwd: Ok(fwd),
                rev: Ok(rev),
                calls: std::sync::atomic::AtomicU32::new(0),
            }
        }
        fn fwd_err() -> Self {
            Self {
                fwd: Err(anyhow::anyhow!("transient gh failure")),
                rev: Ok(CompareResult::default()),
                calls: std::sync::atomic::AtomicU32::new(0),
            }
        }
        fn call_count(&self) -> u32 {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    impl ScmProvider for MockScm {
        fn compare(&self, _r: &str, _b: &str, _h: &str) -> anyhow::Result<CompareResult> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            // 1st call = base...head (fwd), 2nd = head...base (rev).
            if n == 1 {
                self.fwd
                    .as_ref()
                    .map(Clone::clone)
                    .map_err(|e| anyhow::anyhow!("{e}"))
            } else {
                self.rev
                    .as_ref()
                    .map(Clone::clone)
                    .map_err(|e| anyhow::anyhow!("{e}"))
            }
        }
        fn pr_view(&self, _r: &str, _p: u64, _f: &[&str]) -> anyhow::Result<PrSummary> {
            anyhow::bail!("unused")
        }
        fn pr_checks(&self, _r: &str, _p: u64) -> anyhow::Result<Vec<CheckState>> {
            anyhow::bail!("unused")
        }
        fn pr_list(
            &self,
            _r: &str,
            _f: &ListFilter,
            _fl: &[&str],
            _c: Option<&Path>,
        ) -> anyhow::Result<Vec<PrSummary>> {
            anyhow::bail!("unused")
        }
        fn pr_merge(&self, _r: &str, _p: u64, _o: &MergeOpts) -> anyhow::Result<MergeOutcome> {
            anyhow::bail!("unused")
        }
        fn issue_view(&self, _r: &str, _n: u64, _f: &[&str]) -> anyhow::Result<IssueSummary> {
            anyhow::bail!("unused")
        }
    }

    // ── §3.10 (c) end-to-end: the orchestrator refuses a stale+invariant PR via
    // commit-ancestry ALONE. The whole point is that NO mergeStateStatus is
    // consulted (it may well still read CLEAN under the lag that fooled #2137) —
    // `check_merge_freshness` has no access to it; the verdict is the ancestry. ──
    #[test]
    fn orchestrator_refuses_stale_invariant_ignoring_laggy_mergestate_2140() {
        let mock = MockScm::new(
            CompareResult {
                behind_by: 1,
                files: vec![s("src/unrelated.rs")], // PR's own files: clean
            },
            CompareResult {
                behind_by: 0,
                files: vec![s("src/mcp/handlers/dispatch_hook/mod.rs")], // behind-main: the grower
            },
        );
        assert_eq!(
            check_merge_freshness(&mock, "owner/repo", "deadbeef", "main"),
            FreshnessVerdict::StaleRisky {
                behind_by: 1,
                files: vec![s("src/mcp/handlers/dispatch_hook/mod.rs")]
            }
        );
        assert_eq!(mock.call_count(), 2, "both compare directions queried");
    }

    // ── orchestrator: up-to-date short-circuits after ONE compare. ──
    #[test]
    fn orchestrator_up_to_date_one_call_fresh_2140() {
        let mock = MockScm::new(
            CompareResult {
                behind_by: 0,
                files: vec![],
            },
            CompareResult::default(),
        );
        assert_eq!(
            check_merge_freshness(&mock, "owner/repo", "abc", "main"),
            FreshnessVerdict::Fresh
        );
        assert_eq!(mock.call_count(), 1, "up-to-date skips the second compare");
    }

    // ── orchestrator: a transient compare error fails OPEN (never blocks a real
    // merge on a gh hiccup). ──
    #[test]
    fn orchestrator_fail_open_on_compare_error_2140() {
        let mock = MockScm::fwd_err();
        assert_eq!(
            check_merge_freshness(&mock, "owner/repo", "abc", "main"),
            FreshnessVerdict::Fresh
        );
    }
}
