//! #2749 3b: OFF-TICK freshness populator — the PRODUCER for the read-only
//! scanner gate.
//!
//! `freshness_gate` (mod.rs) CONSUMES a deterministic latest-main ancestry tuple
//! (`freshness_checked_*` + `freshness_behind_by`). This module PRODUCES it, on
//! the #986 gh-poll WORKER thread (OFF the scanner tick, so there is no per-tick
//! compare storm), via the authoritative REMOTE forge compare
//! (`ScmProvider::compare`; a local merge-base can be stale / shallow / absent /
//! wrong for forks — never a production authority). The base of truth is the
//! atomically-observed remote base tip (`observed_base_sha`, written by
//! `apply_gh_observations`), so `freshness_checked_base == observed_base` by
//! construction and the gate's three-head agreement + base equality detect a
//! torn / stale observation. Compare is bounded: ONCE per changed
//! (repo, PR, head, observed-base) tuple (plus a pre-TTL refresh); a compare
//! failure stamps `freshness_error` WITHOUT clobbering the last-good tuple.

use std::path::Path;

use super::{gh_poll, load, with_pr_state, FRESHNESS_TTL_SECS};

/// Re-compute only as the cached tuple ages toward the gate TTL, so a STABLE
/// fresh PR keeps its pr-ready gate open instead of lapsing after
/// `FRESHNESS_TTL_SECS`. Half the TTL leaves ample headroom for the worker
/// cadence to land a refresh before the gate would go stale.
const FRESHNESS_REFRESH_SECS: i64 = FRESHNESS_TTL_SECS / 2;

/// Should the populator run the (remote-I/O) compare for this state? True when
/// the checked tuple is not current for the observed (head, base), a prior
/// compare errored, or the cached tuple is aging past the refresh window. False
/// when the observation is torn/stale (wait for a fresh one) or the tuple is
/// already current and fresh (bounded — compare once per changed tuple).
fn needs_recompute(
    state: &super::PrState,
    observed_head: &str,
    observed_base: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if observed_head != state.head_sha {
        return false; // torn/stale observation — a re-observe must land first
    }
    if state.freshness_error {
        return true;
    }
    if state.freshness_checked_head_sha.as_deref() != Some(state.head_sha.as_str())
        || state.freshness_checked_base_sha.as_deref() != Some(observed_base)
    {
        return true;
    }
    match state
        .freshness_checked_at
        .as_deref()
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
    {
        Some(t) => {
            now.signed_duration_since(t.with_timezone(&chrono::Utc))
                .num_seconds()
                > FRESHNESS_REFRESH_SECS
        }
        None => true,
    }
}

/// Stamp the deterministic-ancestry freshness tuple for every open PR observed
/// this worker cycle. Called from `gh_poll::worker_poll_and_act` (off-tick).
pub(crate) fn stamp_freshness_off_tick(home: &Path, repo: &str, prs: &[gh_poll::GhPrMetadata]) {
    let now = chrono::Utc::now();
    for meta in prs
        .iter()
        .filter(|m| matches!(m.state, gh_poll::GhPrState::Open))
    {
        let branch = &meta.head_ref;
        let Some(state) = load(home, repo, branch) else {
            continue;
        };
        let (Some(observed_head), Some(observed_base)) = (
            state.observed_head_sha.as_deref(),
            state.observed_base_sha.as_deref(),
        ) else {
            continue; // no atomic observation yet (the scanner stamps it first)
        };
        if !needs_recompute(&state, observed_head, observed_base, now) {
            continue;
        }
        // The compare is REMOTE I/O — run it OUTSIDE any pr-state flock (#1617
        // lock-while-blocking class), then stamp under the flock with a re-check
        // so a concurrent head/base move can't let a stale compare through.
        let head = state.head_sha.clone();
        let base = observed_base.to_string();
        let provider = crate::scm::make_scm_provider(repo, None);
        match provider.compare(repo, &base, &head) {
            Ok(result) => {
                let ts = chrono::Utc::now().to_rfc3339();
                let _ = with_pr_state(home, repo, branch, |s| {
                    if s.head_sha == head
                        && s.observed_head_sha.as_deref() == Some(head.as_str())
                        && s.observed_base_sha.as_deref() == Some(base.as_str())
                    {
                        s.freshness_checked_head_sha = Some(head.clone());
                        s.freshness_checked_base_sha = Some(base.clone());
                        s.freshness_checked_at = Some(ts.clone());
                        s.freshness_behind_by = Some(result.behind_by);
                        s.freshness_error = false;
                    }
                });
            }
            Err(e) => {
                tracing::warn!(
                    repo = %repo, branch = %branch, error = %e,
                    "#2749 3b: ancestry compare failed — freshness_error (last-good tuple preserved)"
                );
                // Flag the error WITHOUT clobbering the last-good checked tuple.
                let _ = with_pr_state(home, repo, branch, |s| {
                    if s.head_sha == head {
                        s.freshness_error = true;
                    }
                });
            }
        }
    }
}
