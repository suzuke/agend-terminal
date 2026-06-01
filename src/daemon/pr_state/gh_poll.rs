//! #986: GitHub observation feeder for the [`super::PrState`] aggregator.
//!
//! Closes the (repo, branch) → (PR#, pr_author) resolution gap left by
//! #972: the aggregator's internal state machine works correctly but
//! cannot map a ci-watch observation to a GitHub PR identity without
//! talking to GitHub. This module is the IO surface that bridges them.
//!
//! ## Architecture
//!
//! - [`GhPoller`] trait — production [`CliGhPoller`] shells out to
//!   `gh pr list --json author,number,headRefName,isDraft,state,mergedAt
//!   --state all`; tests inject [`MockGhPoller`] with canned responses
//!   (§3.20 SOP 1 — no subprocess invocation in unit tests).
//! - Single batched call per repo per scanner tick (NOT per-PR
//!   `gh pr view`). Worst case 360 calls/hr at default cadence; 1440/hr
//!   at armed cadence — both well under 5000/hr authenticated budget.
//! - Tiered polling cadence (dev-2 Pushback 4): 15s when
//!   `PrState.auto_armed=true` (active flow — minimize [pr-merged]
//!   latency); 60s otherwise.
//! - Exponential backoff on failures: `2^failures × tick` capped at
//!   300s. Cleared on first success.
//! - Slip observability (dev-2 BLOCKING #1): `tracing::warn!` when the
//!   gh CLI call elapsed > 1s — operator visibility into scanner
//!   thread blocking. PR body commitment: if empirical slip > 10% of
//!   tick budget, follow-up refactors to fire-and-forget worker.
//!
//! ## Author resolution (dev-2 Pushback 2 — INVERTED chain)
//!
//! Operator-explicit `github_login` field on fleet.yaml InstanceConfig
//! wins over heuristic name match. Final fallback is `fixup-lead` with
//! a tracing::warn observability hook (reviewer MANDATORY).
//!
//! ## Webhook future-compat
//!
//! Webhook subscription (deferred per #972 docstring) is a drop-in
//! `GhPoller` impl OR sibling trait — webhook payload feeds the same
//! event variants. Polling becomes catch-up backup for missed webhooks.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Single PR's observation from `gh pr list`. Mirrors GitHub's
/// graphql-derived fields needed to drive the PrState reducer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhPrMetadata {
    pub number: u64,
    pub author_login: String,
    pub head_ref: String,
    pub is_draft: bool,
    pub state: GhPrState,
    pub merged_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GhPrState {
    Open,
    Merged,
    Closed,
}

/// IO surface for observing GitHub PR state. Production:
/// [`CliGhPoller`]; tests: [`MockGhPoller`].
pub trait GhPoller: Send + Sync {
    /// Batch-poll all PRs for `repo`. Returns Err on transport / CLI
    /// errors; Ok(vec) on success (empty vec when no PRs match).
    fn poll(&self, repo: &str) -> anyhow::Result<Vec<GhPrMetadata>>;
}

/// Production poller — invokes `gh pr list --json ... --state all`.
pub struct CliGhPoller;

impl GhPoller for CliGhPoller {
    fn poll(&self, repo: &str) -> anyhow::Result<Vec<GhPrMetadata>> {
        // #PR-B: route the `gh pr list` shell-out through `ScmProvider`
        // instead of calling `Command::new("gh")` directly. The emitted
        // argv is byte-identical to the prior inline call (pinned by
        // `scm::tests::pr_list_args_match_site7_byte_identical`); the
        // `--json` field set is passed verbatim. Behavior is unchanged:
        // on any failure pr_list returns Err (same as the prior all-Err
        // contract), Ok(vec) on success.
        let start = std::time::Instant::now();
        let result = crate::scm::make_scm_provider(repo, None).pr_list(
            repo,
            &crate::scm::ListFilter {
                state: Some("all"),
                limit: Some(100),
                ..Default::default()
            },
            &[
                "author",
                "number",
                "headRefName",
                "isDraft",
                "state",
                "mergedAt",
            ],
        );
        let elapsed = start.elapsed();
        // dev-2 BLOCKING #1: slip observability. >1s flags potential
        // scanner-thread blocking; >10% of tick budget triggers the
        // fire-and-forget refactor commitment in the PR body. Timing the
        // whole trait call preserves the original semantics (the prior
        // code warned after the gh process ran, regardless of exit).
        if elapsed > Duration::from_secs(1) {
            tracing::warn!(
                repo = %repo,
                elapsed_ms = elapsed.as_millis() as u64,
                "#986 gh-poll: scanner-thread slip > 1s — \
                 if recurrent, refactor to fire-and-forget worker"
            );
        }
        Ok(result?
            .into_iter()
            .filter_map(summary_to_gh_metadata)
            .collect())
    }
}

/// Map a provider-neutral [`crate::scm::PrSummary`] to [`GhPrMetadata`].
/// Reproduces the prior `parse_one` contract verbatim: `number`,
/// `author_login`, `head_ref` and a recognized `state` are required (a
/// missing/unknown one drops the entry — defensive skip rather than
/// aborting the batch), `is_draft` defaults to false, and `merged_at` is
/// the already-nonempty-filtered optional. (`PrSummary.number` is 0 only
/// when the JSON had no usable `number`, which a real PR never is, so it
/// stands in for `parse_one`'s `number?` presence check.)
fn summary_to_gh_metadata(s: crate::scm::PrSummary) -> Option<GhPrMetadata> {
    let state = match s.state.as_deref()? {
        "OPEN" => GhPrState::Open,
        "MERGED" => GhPrState::Merged,
        "CLOSED" => GhPrState::Closed,
        _ => return None,
    };
    if s.number == 0 {
        return None;
    }
    Some(GhPrMetadata {
        number: s.number,
        author_login: s.author_login?,
        head_ref: s.head_ref?,
        is_draft: s.is_draft.unwrap_or(false),
        state,
        merged_at: s.merged_at,
    })
}

// ─── cadence + backoff ─────────────────────────────────────────────────

/// Refresh cadence when `auto_armed` (active self-merge flow).
const ARMED_CADENCE: Duration = Duration::from_secs(15);
/// Default refresh cadence otherwise.
const DEFAULT_CADENCE: Duration = Duration::from_secs(60);
/// Backoff ceiling on consecutive failures.
const BACKOFF_CAP: Duration = Duration::from_secs(300);
/// Tick used as the backoff base unit.
const BACKOFF_TICK: Duration = Duration::from_secs(10);

/// Decide whether `state` is due for a gh-poll refresh based on the
/// last poll timestamp, tiered cadence, and exponential backoff.
///
/// Returns `true` if the scanner should issue a poll for this state's
/// repo on this tick.
pub fn should_poll(state: &super::PrState, now_rfc3339: &str) -> bool {
    // First observation: always poll.
    let Some(last) = state.last_gh_poll_at.as_deref() else {
        return true;
    };
    let Ok(last_t) = chrono::DateTime::parse_from_rfc3339(last) else {
        return true; // malformed timestamp — re-poll to recover
    };
    let Ok(now_t) = chrono::DateTime::parse_from_rfc3339(now_rfc3339) else {
        return false;
    };
    let elapsed = (now_t - last_t).num_seconds();
    if elapsed < 0 {
        return false;
    }
    let elapsed = Duration::from_secs(elapsed as u64);
    let base = if state.auto_armed {
        ARMED_CADENCE
    } else {
        DEFAULT_CADENCE
    };
    if state.gh_poll_failures > 0 {
        let backoff = backoff_window(state.gh_poll_failures);
        return elapsed >= backoff.max(base);
    }
    elapsed >= base
}

/// Exponential backoff: `2^failures × tick`, capped at [`BACKOFF_CAP`].
/// Saturates at the cap on overflow.
pub fn backoff_window(failures: u32) -> Duration {
    let shifted = BACKOFF_TICK
        .as_secs()
        .checked_shl(failures)
        .unwrap_or(u64::MAX);
    Duration::from_secs(shifted).min(BACKOFF_CAP)
}

// ─── author resolution (4-tier, INVERTED per dev-2 Pushback 2) ─────────

/// Resolve PR author given gh-poll observation + existing PrState.
/// Tier order (operator-explicit FIRST):
///   1. `fleet.yaml github_login` field match for `author.login`
///   2. `author.login` direct name lookup against fleet.yaml instances
///   3. `subscribers[0]` from ci_watch
///   4. `"fixup-lead"` final fallback (with `tracing::warn`)
///
/// `home` is needed for tier 1+2 fleet.yaml lookup. When `gh_author`
/// is None (pre-first-poll or poll-failed-no-match) the chain falls
/// straight through to tier 3.
pub fn resolve_author_with_gh(
    home: &std::path::Path,
    gh_author: Option<&str>,
    state: &super::PrState,
) -> String {
    if let Some(login) = gh_author {
        if let Some(name) = match_via_github_login_field(home, login) {
            return name;
        }
        if let Some(name) = match_via_instance_name(home, login) {
            return name;
        }
    }
    if let Some(first) = state.subscribers.first() {
        return first.clone();
    }
    // Tier 4 — reviewer MANDATORY: warn-log so operator sees the
    // chain falling through.
    tracing::warn!(
        repo = %state.repo,
        branch = %state.branch,
        gh_author = ?gh_author,
        "#986 author resolution fell through to fixup-lead fallback — \
         consider setting `github_login` on the author's fleet.yaml entry"
    );
    "fixup-lead".to_string()
}

fn match_via_github_login_field(home: &std::path::Path, gh_login: &str) -> Option<String> {
    let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok()?;
    cfg.instances.iter().find_map(|(name, inst)| {
        inst.github_login
            .as_deref()
            .filter(|gl| gl.eq_ignore_ascii_case(gh_login))
            .map(|_| name.clone())
    })
}

fn match_via_instance_name(home: &std::path::Path, gh_login: &str) -> Option<String> {
    let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok()?;
    cfg.instances
        .keys()
        .find(|name| name.eq_ignore_ascii_case(gh_login))
        .cloned()
}

// ─── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    /// Test seam — returns canned responses; records call count.
    pub struct MockGhPoller {
        responses: Arc<Mutex<Vec<anyhow::Result<Vec<GhPrMetadata>>>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl MockGhPoller {
        pub fn new(responses: Vec<anyhow::Result<Vec<GhPrMetadata>>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses)),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn call_count(&self) -> usize {
            self.calls.lock().len()
        }
    }

    impl GhPoller for MockGhPoller {
        fn poll(&self, repo: &str) -> anyhow::Result<Vec<GhPrMetadata>> {
            self.calls.lock().push(repo.to_string());
            self.responses
                .lock()
                .pop()
                .unwrap_or_else(|| Ok(Vec::new()))
        }
    }

    /// T1: summary_to_gh_metadata maps a canonical PrSummary to full
    /// GhPrMetadata. (gh-JSON → PrSummary parsing is covered by
    /// `scm::tests`; this pins the gh_poll-side mapping that replaced the
    /// old `parse_one`.)
    #[test]
    fn t1_map_canonical_summary() {
        let s = crate::scm::PrSummary {
            number: 984,
            state: Some("MERGED".into()),
            author_login: Some("suzuke".into()),
            head_ref: Some("fix/969-channel-dedup-mirror-skip".into()),
            is_draft: Some(false),
            merged_at: Some("2026-05-20T04:17:09Z".into()),
            ..Default::default()
        };
        let meta = summary_to_gh_metadata(s).expect("map OK");
        assert_eq!(meta.number, 984);
        assert_eq!(meta.author_login, "suzuke");
        assert_eq!(meta.head_ref, "fix/969-channel-dedup-mirror-skip");
        assert!(!meta.is_draft);
        assert_eq!(meta.state, GhPrState::Merged);
        assert_eq!(meta.merged_at.as_deref(), Some("2026-05-20T04:17:09Z"));
    }

    /// T1b: missing optional fields default (is_draft → false, merged_at
    /// → None), matching the prior parse_one tolerance.
    #[test]
    fn t1b_map_missing_optional_fields() {
        let s = crate::scm::PrSummary {
            number: 970,
            state: Some("OPEN".into()),
            author_login: Some("suzuke".into()),
            head_ref: Some("fix/x".into()),
            is_draft: None,
            merged_at: None,
            ..Default::default()
        };
        let meta = summary_to_gh_metadata(s).expect("map OK");
        assert!(!meta.is_draft, "missing isDraft → false");
        assert_eq!(meta.merged_at, None, "missing mergedAt → None");
    }

    /// T1c: unknown state string → None (drop), matching parse_one.
    #[test]
    fn t1c_map_unknown_state_returns_none() {
        let s = crate::scm::PrSummary {
            number: 1,
            state: Some("DRAFT".into()), // not a valid PR state
            author_login: Some("x".into()),
            head_ref: Some("br".into()),
            ..Default::default()
        };
        assert!(summary_to_gh_metadata(s).is_none());
    }

    /// T1d: a missing required field (number absent → 0, or author/head
    /// None) drops the entry — the parse_one `?` semantics.
    #[test]
    fn t1d_map_missing_required_fields_drop() {
        // number 0 (no usable number in JSON) → drop.
        let no_num = crate::scm::PrSummary {
            number: 0,
            state: Some("OPEN".into()),
            author_login: Some("x".into()),
            head_ref: Some("br".into()),
            ..Default::default()
        };
        assert!(summary_to_gh_metadata(no_num).is_none());
        // missing author_login → drop.
        let no_author = crate::scm::PrSummary {
            number: 5,
            state: Some("OPEN".into()),
            author_login: None,
            head_ref: Some("br".into()),
            ..Default::default()
        };
        assert!(summary_to_gh_metadata(no_author).is_none());
    }

    fn fresh_state(branch: &str) -> super::super::PrState {
        let now = chrono::Utc::now().to_rfc3339();
        super::super::PrState {
            repo: "owner/repo".to_string(),
            pr_number: 0,
            branch: branch.to_string(),
            head_sha: "sha-A".to_string(),
            pr_author: String::new(),
            subscribers: vec!["dev".to_string()],
            ci_state: super::super::CiState::Pending,
            verdict_state: super::super::VerdictState::None,
            merge_state: super::super::MergeState::NotReady,
            draft_state: super::super::DraftState::Ready,
            review_class: super::super::ReviewClass::Single,
            ready_emitted_for_sha: None,
            auto_armed: false,
            auto_armed_for_sha: None,
            auto_armed_at: None,
            last_gh_poll_at: None,
            gh_poll_failures: 0,
            last_gh_state: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// T7-a: backoff window math. 0 failures → tick (10s). 1 → 20s.
    /// 2 → 40s. 5 → 320s capped to 300s.
    #[test]
    fn t7a_backoff_window_curve() {
        assert_eq!(backoff_window(0), Duration::from_secs(10));
        assert_eq!(backoff_window(1), Duration::from_secs(20));
        assert_eq!(backoff_window(2), Duration::from_secs(40));
        assert_eq!(backoff_window(5), Duration::from_secs(300), "capped");
        assert_eq!(backoff_window(20), Duration::from_secs(300), "saturated");
        assert_eq!(
            backoff_window(64),
            Duration::from_secs(300),
            "overflow safe"
        );
    }

    /// T7-b: should_poll honors backoff on failures. Backoff EXTENDS
    /// cadence: effective wait is `max(base_cadence, backoff_window)`.
    /// failures=1 + base=60s → 60s wins (backoff=20s < base).
    /// failures=3 + base=60s → backoff=80s wins.
    #[test]
    fn t7b_should_poll_honors_backoff_after_failure() {
        let mut state = fresh_state("feat/x");
        let now = chrono::Utc::now();
        // Pretend a poll just happened 70s ago (past 60s default
        // cadence) + failed 3 times (80s backoff).
        state.last_gh_poll_at = Some((now - chrono::Duration::seconds(70)).to_rfc3339());
        state.gh_poll_failures = 3;
        assert!(
            !should_poll(&state, &now.to_rfc3339()),
            "70s elapsed < 80s backoff with failures=3 → wait"
        );
        // 15s more (85s total) — past 80s backoff window.
        let now2 = now + chrono::Duration::seconds(15);
        assert!(
            should_poll(&state, &now2.to_rfc3339()),
            "85s elapsed >= 80s backoff → poll"
        );
    }

    /// T7-c: tiered cadence honors auto_armed.
    #[test]
    fn t7c_armed_cadence_is_tighter() {
        let mut state = fresh_state("feat/x");
        let now = chrono::Utc::now();
        // Last poll was 20s ago.
        state.last_gh_poll_at = Some((now - chrono::Duration::seconds(20)).to_rfc3339());
        // Default cadence: 60s — should NOT poll yet.
        state.auto_armed = false;
        assert!(
            !should_poll(&state, &now.to_rfc3339()),
            "default 60s not yet"
        );
        // Armed cadence: 15s — should poll (20 > 15).
        state.auto_armed = true;
        assert!(
            should_poll(&state, &now.to_rfc3339()),
            "armed 15s cadence reached"
        );
    }

    /// T7-d: first poll always fires (last_gh_poll_at None).
    #[test]
    fn t7d_first_poll_always_due() {
        let state = fresh_state("feat/x");
        assert!(state.last_gh_poll_at.is_none());
        assert!(should_poll(&state, &chrono::Utc::now().to_rfc3339()));
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agend-986-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// T8-a: tier 1 — `github_login` field wins over name match.
    #[test]
    fn t8a_github_login_field_wins_over_name_match() {
        let home = tmp_home("auth-tier1");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  dev:\n    backend: claude\n    github_login: alice\n  alice:\n    backend: claude\n",
        )
        .unwrap();
        let state = fresh_state("br");
        let resolved = resolve_author_with_gh(&home, Some("alice"), &state);
        assert_eq!(
            resolved, "dev",
            "github_login=alice on `dev` MUST beat name-match on `alice`"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-b: tier 2 — direct name lookup when no github_login set.
    #[test]
    fn t8b_direct_name_match_when_no_github_login() {
        let home = tmp_home("auth-tier2");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  suzuke:\n    backend: claude\n",
        )
        .unwrap();
        let state = fresh_state("br");
        let resolved = resolve_author_with_gh(&home, Some("suzuke"), &state);
        assert_eq!(resolved, "suzuke");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-c: tier 3 — subscribers[0] when no fleet.yaml match.
    #[test]
    fn t8c_subscribers_fallback() {
        let home = tmp_home("auth-tier3");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  someone-else:\n    backend: claude\n",
        )
        .unwrap();
        let state = fresh_state("br"); // subscribers = ["dev"]
        let resolved = resolve_author_with_gh(&home, Some("nobody-matches"), &state);
        assert_eq!(resolved, "dev");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-d / T11: tier 4 — fixup-lead final fallback + tracing::warn
    /// observability hook. Reviewer #990 BLOCKING #2: assert the
    /// `tracing::warn` event actually fires (operator visibility into
    /// fall-through), not just the return value.
    #[test]
    #[tracing_test::traced_test]
    fn t8d_fixup_lead_final_fallback_emits_tracing_warn() {
        let home = tmp_home("auth-tier4");
        std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();
        let mut state = fresh_state("br");
        state.subscribers.clear();
        let resolved = resolve_author_with_gh(&home, Some("nobody"), &state);
        assert_eq!(resolved, "fixup-lead");
        assert!(
            logs_contain("#986 author resolution fell through to fixup-lead"),
            "tracing::warn MUST fire when chain falls through to tier 4 — \
             operator needs visibility to fix fleet.yaml github_login"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T8-e: gh_author=None still falls through to subscribers/fallback.
    #[test]
    fn t8e_none_gh_author_falls_to_subscribers() {
        let home = tmp_home("auth-none");
        std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();
        let state = fresh_state("br"); // subscribers = ["dev"]
        let resolved = resolve_author_with_gh(&home, None, &state);
        assert_eq!(resolved, "dev");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// MockGhPoller smoke — verifies the test seam itself works.
    #[test]
    fn mock_poller_returns_canned_responses() {
        let responses = vec![Ok(vec![GhPrMetadata {
            number: 970,
            author_login: "suzuke".into(),
            head_ref: "fix/x".into(),
            is_draft: false,
            state: GhPrState::Open,
            merged_at: None,
        }])];
        let poller = MockGhPoller::new(responses);
        let r = poller.poll("owner/repo").unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(poller.call_count(), 1);
    }
}
