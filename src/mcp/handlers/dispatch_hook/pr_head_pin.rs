//! #3675: managed acquisition of an external (fork) PR head whose commit is not
//! present in the local source repository.
//!
//! A formal reviewer-assignment dispatch pins the exact reviewed head, but the
//! head of a cross-repository (fork) PR is often absent locally — the prior
//! `rev-parse --verify` precondition then failed closed with
//! `expected_head_mismatch` and no reviewer workspace could be provisioned.
//!
//! This helper is the SINGLE shared availability check behind both entry points:
//! - the dispatch/send side ([`super::exact_head::resolve`]), and
//! - the checkout side (`ci::checkout_helpers::validate_expected_head`).
//!
//! Ordering is load-bearing: the LOCAL object is checked first, so every
//! existing local-head dispatch stays byte-identical and never performs a
//! network call. Only on a local miss, and only with a `(repository,
//! pr_number)` context, does the managed path run:
//!
//! 1. PIN the provider's current `headRefOid`/`state` (`ScmProvider::pr_view`);
//!    a non-OPEN PR or a head that differs from `expected` fails closed BEFORE
//!    any write.
//! 2. FETCH the server-advertised `refs/pull/<N>/head` into a short-lived
//!    namespaced ref (`refs/agend/review-pr-<N>-<short>`), bounded by
//!    [`DISPATCH_FETCH_TIMEOUT`]. A bare-SHA fetch is deliberately NOT used (it
//!    depends on `uploadpack.allowAnySHA1InWant`, which is not portable).
//! 3. VERIFY the fetched object equals `expected` (`rev-parse` + `cat-file -e`);
//!    a mismatch deletes the synthetic ref and fails closed.
//! 4. RE-PIN to close the fetch→verify window; drift deletes the ref and fails
//!    closed.
//!
//! The fetch writes only objects plus the transient namespaced ref (removed
//! before returning); canonical `refs/heads/*`, `refs/remotes/*`, the index and
//! any user's dirty files are never touched. Exact-head review semantics and
//! the merge gate remain untouched.

use std::path::Path;

use super::{DispatchError, ErrorCode, Stage, DISPATCH_FETCH_TIMEOUT};

/// Namespace for the transient ref that anchors a just-fetched fork head while
/// it is verified. Never a canonical branch/remote ref, and always removed
/// before this module returns.
const SYNTHETIC_REF_PREFIX: &str = "refs/agend/review-pr-";

/// Fields pinned from the provider for an external PR head. Kept in lockstep
/// with the P0 exact-head merge pin (`ci::merge::acquire_head_base`) so both
/// identity reads request the same underlying facts.
const PIN_FIELDS: &[&str] = &[
    "headRefOid",
    "headRefName",
    "baseRefOid",
    "state",
    "isCrossRepository",
];

/// Why an external PR head could not be made locally available. Every variant
/// fails closed — a reviewer is never provisioned at an unverified head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrHeadError {
    /// The commit is absent locally and no `(repository, pr_number)` context was
    /// supplied, so no managed acquisition is possible.
    LocalMiss { expected: String },
    /// The SCM provider (`gh pr view`) failed or returned no usable head.
    ProviderUnavailable { expected: String, raw: String },
    /// The PR is not OPEN (MERGED / CLOSED / unknown) — fail before any fetch.
    NotOpen { expected: String, state: String },
    /// The provider's current head differs from `expected` — fail before any fetch.
    HeadMismatch { expected: String, observed: String },
    /// The `git fetch` subprocess failed, timed out, or exited non-zero.
    FetchFailed { expected: String, raw: String },
    /// The fetched object did not equal `expected`.
    ObjectMismatch { expected: String, fetched: String },
    /// The PR head moved between the pre-fetch pin and the post-fetch re-pin.
    HeadDrifted { expected: String, observed: String },
}

impl PrHeadError {
    /// Provider-neutral `code` string (mirrors the structured response shape both
    /// entry points already emit).
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::LocalMiss { .. } | Self::NotOpen { .. } | Self::HeadMismatch { .. } => {
                "expected_head_mismatch"
            }
            Self::ProviderUnavailable { .. } | Self::FetchFailed { .. } => "provider_fetch_failed",
            Self::ObjectMismatch { .. } => "object_mismatch",
            Self::HeadDrifted { .. } => "head_drifted",
        }
    }

    /// `true` when this failure happened after a `git fetch` was actually issued.
    fn fetch_attempted(&self) -> bool {
        matches!(
            self,
            Self::FetchFailed { .. } | Self::ObjectMismatch { .. } | Self::HeadDrifted { .. }
        )
    }

    /// Human-readable, path-redaction-safe message.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::LocalMiss { expected } => {
                format!("expected_head {expected} does not exist as a commit in the repository")
            }
            Self::ProviderUnavailable { expected, raw } => {
                format!("could not pin external PR head {expected} from the SCM provider: {raw}")
            }
            Self::NotOpen { expected, state } => {
                format!("external PR head {expected} does not belong to an OPEN PR (state={state})")
            }
            Self::HeadMismatch { expected, observed } => {
                format!("expected_head {expected} does not match the PR head {observed}")
            }
            Self::FetchFailed { expected, raw } => {
                format!("could not fetch external PR head {expected}: {raw}")
            }
            Self::ObjectMismatch { expected, fetched } => {
                format!("fetched PR head object {fetched} does not match expected {expected}")
            }
            Self::HeadDrifted { expected, observed } => {
                format!("PR head drifted from {expected} to {observed} during acquisition")
            }
        }
    }

    /// Structured checkout-side error response (the `repo action=checkout` shape).
    pub(crate) fn checkout_value(&self) -> serde_json::Value {
        use serde_json::json;
        let code = self.code();
        let error = self.message();
        match self {
            Self::LocalMiss { expected } => json!({
                "error": error,
                "code": code,
                "expected_head": expected,
                "actual_head": "",
            }),
            Self::ProviderUnavailable { expected, raw } => json!({
                "error": error,
                "code": code,
                "expected_head": expected,
                "raw": raw,
            }),
            Self::NotOpen { expected, .. } => json!({
                "error": error,
                "code": code,
                "expected_head": expected,
                "actual_head": "",
            }),
            Self::HeadMismatch { expected, observed } => json!({
                "error": error,
                "code": code,
                "expected_head": expected,
                "actual_head": observed,
            }),
            Self::FetchFailed { expected, raw } => json!({
                "error": error,
                "code": code,
                "expected_head": expected,
                "raw": raw,
            }),
            Self::ObjectMismatch { expected, fetched } => json!({
                "error": error,
                "code": code,
                "expected_head": expected,
                "actual_head": fetched,
            }),
            Self::HeadDrifted { expected, observed } => json!({
                "error": error,
                "code": code,
                "expected_head": expected,
                "actual_head": observed,
            }),
        }
    }

    /// Structured dispatch-side error.
    pub(crate) fn into_dispatch_error(self) -> DispatchError {
        let code = match self {
            Self::LocalMiss { .. } | Self::NotOpen { .. } | Self::HeadMismatch { .. } => {
                ErrorCode::ExpectedHeadMismatch
            }
            Self::ProviderUnavailable { .. } | Self::FetchFailed { .. } => {
                ErrorCode::ProviderFetchFailed
            }
            Self::ObjectMismatch { .. } => ErrorCode::ObjectMismatch,
            Self::HeadDrifted { .. } => ErrorCode::HeadDrifted,
        };
        let fetch_attempted = self.fetch_attempted();
        let message = self.message();
        DispatchError {
            message,
            code,
            stage: Stage::ValidateExpectedHead,
            fetch_attempted,
            raw: None,
        }
    }
}

/// The shared exact-head availability check. `Ok(())` means `expected` resolves
/// to a commit in `source` (it already did, or it was just fetched and verified).
///
/// The local-hit fast path performs NO network I/O, so every pre-existing
/// local-head dispatch is byte-identical.
pub(crate) fn ensure_commit_available(
    source: &Path,
    expected: &str,
    repository: Option<&str>,
    pr_number: Option<u64>,
) -> Result<(), PrHeadError> {
    if local_commit_present(source, expected) {
        return Ok(());
    }
    match (repository, pr_number) {
        (Some(repo), Some(pr)) => provision_from_pr(source, repo, pr, expected),
        _ => Err(PrHeadError::LocalMiss {
            expected: expected.to_string(),
        }),
    }
}

/// `true` iff `expected` already resolves to a local commit object.
fn local_commit_present(source: &Path, expected: &str) -> bool {
    crate::git_helpers::git_cmd(
        source,
        &["rev-parse", "--verify", &format!("{expected}^{{commit}}")],
    )
    .is_ok()
}

/// Deterministic, per-(PR, head) namespaced ref. The short head fragment keeps
/// two different heads of the same PR from colliding on a stale partial.
fn synthetic_ref(pr: u64, expected: &str) -> String {
    let short = &expected[..expected.len().min(12)];
    format!("{SYNTHETIC_REF_PREFIX}{pr}-{short}")
}

fn delete_synthetic_ref(source: &Path, synthetic_ref: &str) {
    // Best-effort: the ref is transient bookkeeping, not authoritative state.
    let _ = crate::git_helpers::git_bypass(source, &["update-ref", "-d", synthetic_ref]);
}

/// Pin the provider's current head and fail closed unless it is an OPEN PR at
/// exactly `expected`.
fn pin_open_head(repo: &str, pr: u64, expected: &str) -> Result<(), PrHeadError> {
    let provider = crate::scm::make_scm_provider(repo, None);
    let summary = provider.pr_view(repo, pr, PIN_FIELDS).map_err(|error| {
        PrHeadError::ProviderUnavailable {
            expected: expected.to_string(),
            raw: error.to_string(),
        }
    })?;
    let head = summary
        .head_ref_oid
        .filter(|head| crate::daemon::ci_watch::is_full_commit_sha(head))
        .ok_or_else(|| PrHeadError::ProviderUnavailable {
            expected: expected.to_string(),
            raw: "SCM returned no usable headRefOid".to_string(),
        })?;
    let state = summary.state.unwrap_or_default();
    if !state.eq_ignore_ascii_case("OPEN") {
        return Err(PrHeadError::NotOpen {
            expected: expected.to_string(),
            state,
        });
    }
    if !head.eq_ignore_ascii_case(expected) {
        return Err(PrHeadError::HeadMismatch {
            expected: expected.to_string(),
            observed: head,
        });
    }
    Ok(())
}

/// Fetch, verify, and re-pin the external PR head. The transient synthetic ref
/// is removed on every exit path.
fn provision_from_pr(
    source: &Path,
    repo: &str,
    pr: u64,
    expected: &str,
) -> Result<(), PrHeadError> {
    // 1. Pin BEFORE any write — a moved/closed PR never triggers a fetch.
    pin_open_head(repo, pr, expected)?;

    let synthetic_ref = synthetic_ref(pr, expected);
    let refspec = format!("+refs/pull/{pr}/head:{synthetic_ref}");
    let fetch = crate::git_helpers::git_bypass_timeout(
        source,
        &["fetch", "--no-tags", "origin", &refspec],
        DISPATCH_FETCH_TIMEOUT,
    );
    match fetch {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            delete_synthetic_ref(source, &synthetic_ref);
            return Err(PrHeadError::FetchFailed {
                expected: expected.to_string(),
                raw: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Err(error) => {
            delete_synthetic_ref(source, &synthetic_ref);
            return Err(PrHeadError::FetchFailed {
                expected: expected.to_string(),
                raw: error.to_string(),
            });
        }
    }

    // 2. Verify the fetched object IS expected (rev-parse + cat-file both).
    let fetched = crate::git_helpers::git_cmd(
        source,
        &[
            "rev-parse",
            "--verify",
            &format!("{synthetic_ref}^{{commit}}"),
        ],
    )
    .map(|sha| sha.trim().to_string())
    .unwrap_or_default();
    let object_matches = !fetched.is_empty()
        && fetched.eq_ignore_ascii_case(expected)
        && crate::git_helpers::git_bypass(
            source,
            &["cat-file", "-e", &format!("{expected}^{{commit}}")],
        )
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !object_matches {
        delete_synthetic_ref(source, &synthetic_ref);
        return Err(PrHeadError::ObjectMismatch {
            expected: expected.to_string(),
            fetched,
        });
    }

    // 3. Re-pin to close the fetch→verify window.
    if let Err(error) = pin_open_head(repo, pr, expected) {
        delete_synthetic_ref(source, &synthetic_ref);
        return Err(match error {
            PrHeadError::NotOpen { state, .. } => PrHeadError::HeadDrifted {
                expected: expected.to_string(),
                observed: format!("state={state}"),
            },
            PrHeadError::HeadMismatch { observed, .. } => PrHeadError::HeadDrifted {
                expected: expected.to_string(),
                observed,
            },
            other => other,
        });
    }

    // 4. Verified. Drop the transient ref; the object stays reachable once the
    //    caller creates the local review branch at `expected` moments later, and
    //    git's unreachable-object grace window covers the gap.
    delete_synthetic_ref(source, &synthetic_ref);
    Ok(())
}
