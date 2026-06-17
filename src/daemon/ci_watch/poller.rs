use crate::agent::{self, AgentRegistry};
use std::path::Path;
use std::sync::Arc;

use super::provider::{
    CiJob, CiPollResult, CiProvider, CiRun, MergeableState, PrState, RepoPollResult,
};

type PerKeySlot = Arc<tokio::sync::Mutex<Option<CiPollResult>>>;
type TickCache = Arc<std::sync::Mutex<std::collections::HashMap<(String, String), PerKeySlot>>>;

/// CR-2026-06-14 (xcut-concurrency F3): per-repo in-flight set bounding the
/// fire-and-forget batch-poll spawns. `check_ci_watches_with_provider` runs once
/// per tick and spawns one detached task per repo onto the 2-worker shared CI
/// runtime; under a provider/network stall, each tick could enqueue new repo
/// tasks faster than the workers drain them, growing the backlog without limit.
/// A repo's slot is claimed before spawning and released when the task finishes
/// (via [`RepoInFlightGuard`]'s `Drop`); a tick skips a repo whose prior cycle is
/// still running, so at most one batch task per repo is ever outstanding.
static IN_FLIGHT_REPOS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// RAII claim on a repo's in-flight slot. Released on `Drop` (covers EVERY async
/// return path of the spawned task), so a panicking or early-returning task can't
/// strand the slot and permanently wedge that repo's polling.
struct RepoInFlightGuard(String);

impl RepoInFlightGuard {
    /// Claim the slot for `repo`. Returns `None` if it's already in flight (the
    /// prior cycle's task hasn't finished) → caller skips spawning this tick.
    fn try_claim(repo: &str) -> Option<Self> {
        let mut set = IN_FLIGHT_REPOS.lock().unwrap_or_else(|e| e.into_inner());
        set.insert(repo.to_string()).then(|| Self(repo.to_string()))
    }
}

impl Drop for RepoInFlightGuard {
    fn drop(&mut self) {
        IN_FLIGHT_REPOS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

struct CachedCiProvider {
    inner: Box<dyn CiProvider>,
    poll_cache: TickCache,
}

#[async_trait::async_trait]
impl CiProvider for CachedCiProvider {
    async fn poll_runs(&self, repo: &str, branch: &str) -> anyhow::Result<CiPollResult> {
        let key = (repo.to_string(), branch.to_string());
        let slot = {
            let mut map = self.poll_cache.lock().unwrap_or_else(|e| e.into_inner());
            map.entry(key).or_default().clone()
        };
        let mut guard = slot.lock().await;
        if let Some(cached) = guard.as_ref() {
            return Ok(cached.clone());
        }
        let result = self.inner.poll_runs(repo, branch).await?;
        *guard = Some(result.clone());
        Ok(result)
    }

    async fn check_pr_terminal(&self, repo: &str, branch: &str) -> PrState {
        self.inner.check_pr_terminal(repo, branch).await
    }

    async fn check_pr_mergeable(&self, repo: &str, branch: &str) -> MergeableState {
        self.inner.check_pr_mergeable(repo, branch).await
    }

    async fn fetch_failure_summary(&self, repo: &str, run_id: u64) -> String {
        self.inner.fetch_failure_summary(repo, run_id).await
    }

    async fn fetch_run_jobs(&self, repo: &str, run_id: u64) -> Vec<super::provider::CiJob> {
        self.inner.fetch_run_jobs(repo, run_id).await
    }

    fn token_warning(&self) -> Option<&'static str> {
        self.inner.token_warning()
    }
}
use super::registry::{ci_watches_dir, flush_watch_state, remove_watch};
use super::sweep::{
    bump_repo_stall_and_maybe_notify, clear_repo_stall_and_maybe_resume, clear_stall_state,
};
use super::watch_state::WatchState;
use super::WATCH_TTL_HOURS;

// Test-only re-exports so the existing test module (moved verbatim from
// the pre-#701 single-file ci_watch.rs) can keep referencing siblings
// via `super::X` paths — `super` here resolves to `poller`, so these
// aliases preserve the original `super::ci_watch::X` semantics.
#[cfg(test)]
use super::provider::{
    detect_provider_from_remote, github_token_warning, BitbucketCiProvider, GitHubCiProvider,
    GitLabCiProvider,
};
#[cfg(test)]
use super::registry::{parse_subscribers, watch_filename};
#[cfg(test)]
use super::sweep::{
    clear_stall_and_maybe_notify_resumed, gc_stale_watches, startup_sweep, STALL_THRESHOLD,
};
#[cfg(test)]
use super::watcher::check_ci_watches;

// ---------------------------------------------------------------------------
// H2: Shared tokio runtime for CI watch — avoids spawning a new thread +
// runtime per poll cycle. Bounded to 2 worker threads.
// ---------------------------------------------------------------------------

pub(super) fn shared_ci_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ci-watch")
            .enable_all()
            .build()
            .expect("ci-watch runtime")
    })
}

/// Sprint 54 P0-2: adaptive backoff curve based on remaining quota.
///
/// Returns the effective polling interval (in seconds) given the
/// configured baseline `interval_secs` and the most recent
/// `X-RateLimit-Remaining` / `X-RateLimit-Limit` observation. The
/// curve has three zones:
///
/// | Zone     | `remaining_pct = remaining / limit` | Multiplier |
/// |----------|-------------------------------------|------------|
/// | Healthy  | `> 0.5`                             | `× 1`      |
/// | Cautious | `0.1 < … ≤ 0.5`                     | `× 2`      |
/// | Critical | `≤ 0.1`                             | `× 4`      |
///
/// **Floor + ceiling**: never below the configured baseline (so
/// operators can't accidentally tune themselves into permanent
/// throttle), never above `interval_secs * 4` (so a single critical
/// watch can't quietly stop polling for an hour).
///
/// **Missing headers**: if either `remaining` or `limit` is `None`, or
/// `limit` is zero, we fall back to the configured baseline. Providers
/// that don't expose the quota headers (currently GitLab + Bitbucket)
/// keep their existing behavior.
///
/// Pure helper — no IO, no state. The throttle path
/// ([`watch_is_due`]) consumes the result; the tick-loop persists
/// `effective_interval_secs` to the watch JSON for diagnostics.
pub(crate) fn adaptive_interval(
    interval_secs: u64,
    remaining: Option<u64>,
    limit: Option<u64>,
) -> u64 {
    // EMPIRICAL REGRESSION-PROOF FLIP (Sprint 54 P0-2): replace the
    // body below with `let _ = (remaining, limit); return interval_secs;`
    // to simulate scaling being disabled. Tests
    // `adaptive_interval_cautious_zone_doubles` and
    // `adaptive_interval_critical_zone_quadruples` both fail with the
    // signatures captured in the PR description.
    let (remaining, limit) = match (remaining, limit) {
        (Some(r), Some(l)) if l > 0 => (r, l),
        // Missing headers OR pathological limit=0 ⇒ baseline. We never
        // ceiling-multiply on absent data — that'd silently widen polls
        // for non-GitHub providers that don't ship the quota counters.
        _ => return interval_secs,
    };
    // Use *1000 to keep the comparison in integer space — avoids
    // floating-point edge cases at exact boundaries (0.5 / 0.1).
    let remaining_x1000 = remaining.saturating_mul(1000) / limit;
    if remaining_x1000 > 500 {
        interval_secs
    } else if remaining_x1000 > 100 {
        interval_secs.saturating_mul(2)
    } else {
        interval_secs.saturating_mul(4)
    }
}

/// Pure throttle decision for a CI watch. Returns `true` when the watch
/// is due for a GitHub poll given its `last_polled_at` (epoch millis,
/// `None` for a fresh watch), its configured `interval_secs`, and the
/// current wall-clock time.
///
/// Extracted from `check_ci_watches` so the first-poll-immediate rule
/// can be unit-tested without filesystem IO — the previous mtime-based
/// throttle was testable only via external side effects on file
/// modification time.
fn watch_is_due(last_polled_at: Option<i64>, interval_secs: u64, now_ms: i64) -> bool {
    match last_polled_at {
        // Never-polled watches (freshly registered, or pre-schema files
        // that don't carry the field) fire on the first check. The
        // handler writes `last_polled_at: null` to signal this.
        None => true,
        Some(ts) => now_ms.saturating_sub(ts) >= (interval_secs as i64) * 1000,
    }
}

// ── #813 ci_watch CONFLICTING PR detection ──

/// #813: emit a `[ci-conflict-detected]` headline to every subscriber's
/// inbox. Persists to JSONL via `crate::inbox::enqueue` so the
/// operator sees the alert on the next inbox read. NO in-band PTY
/// inject here (unlike the terminal-run fan-out) because the
/// `handle_watch_ci` caller doesn't carry an `&AgentRegistry`; the
/// inbox enqueue alone provides the durable signal and the next
/// inbox poll surfaces it within seconds.
///
/// `source` is recorded in the alert body so the operator can
/// distinguish on-watch-start triggers ("watch-start") from periodic
/// re-check triggers ("poll-transition"). C3 wires watch-start path;
/// C4 wires the periodic-poll re-check call site.
pub fn emit_ci_conflict_alert(
    home: &Path,
    repo: &str,
    branch: &str,
    subscribers: &[String],
    source: &str,
) {
    // #1032: enqueue_with_idle_hint replaces raw enqueue so idle
    // backends (codex / claude-code at the prompt) wake on the
    // [ci-conflict-detected] alert. Pre-#1032 the recipient saw the
    // entry only on next inbox poll — for a CI-trigger-blocking
    // condition the operator wants noticed immediately, that delay
    // was the bug.
    for sub in subscribers {
        persist_or_log!(
            crate::inbox::enqueue_with_idle_hint(
                home,
                sub,
                make_ci_conflict_alert_msg(repo, branch, source),
            ),
            "ci_conflict_alert",
            sub
        );
    }
}

/// #813: on-watch-start mergeable check. Queries the provider via
/// the blocking variant (sync caller — `handle_watch_ci` is non-async),
/// caches the observed state into the watch JSON, and emits a
/// `[ci-conflict-detected]` alert when CONFLICTING. Fail-open on
/// Unknown (no alert, no block — preserves behavior under transient
/// GH outages).
pub fn watch_start_check_mergeable(
    home: &Path,
    watch_path: &Path,
    repo: &str,
    branch: &str,
    subscribers: &[String],
    provider: &dyn CiProvider,
) {
    let state = provider.check_pr_mergeable_blocking(repo, branch);
    // Cache the observed state regardless of variant so the poll
    // cycle's transition detector has a baseline. UNKNOWN is cached
    // too — distinguishes "never checked" (field absent) from
    // "checked but uncertain" (field present, value UNKNOWN).
    let now_rfc3339 = chrono::Utc::now().to_rfc3339();
    if let Ok(content) = std::fs::read_to_string(watch_path) {
        if let Ok(mut w) = serde_json::from_str::<serde_json::Value>(&content) {
            w["last_mergeable_state"] = serde_json::json!(state.as_str());
            w["last_mergeable_check_at"] = serde_json::json!(now_rfc3339);
            if let Ok(out) = serde_json::to_string_pretty(&w) {
                let _ = crate::store::atomic_write(watch_path, out.as_bytes());
            }
        }
    }
    if matches!(state, MergeableState::Conflicting) {
        emit_ci_conflict_alert(home, repo, branch, subscribers, "watch-start");
    }
}

enum SkipReason {
    Invalid,
    Expired,
    InactivityTtl,
    RateLimited,
    NotDue,
}

struct PollContext {
    repo: String,
    subscribers: Vec<String>,
    stamped_watch: WatchState,
}

fn prepare_poll_context(
    watch: &WatchState,
    now_utc: chrono::DateTime<chrono::Utc>,
    now_ms: i64,
) -> Result<PollContext, SkipReason> {
    if watch.repo.is_empty() {
        return Err(SkipReason::Invalid);
    }
    let subscribers = watch.subscriber_names();
    if subscribers.is_empty() {
        return Err(SkipReason::Invalid);
    }
    if let Some(expires_at) = watch.expires_at.as_deref() {
        if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(expires_at) {
            if now_utc > exp.with_timezone(&chrono::Utc) {
                return Err(SkipReason::Expired);
            }
        }
    }
    if let Some(last_seen) = watch.last_terminal_seen_at.as_deref() {
        if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(last_seen) {
            let elapsed = now_utc.signed_duration_since(ts.with_timezone(&chrono::Utc));
            if elapsed > chrono::Duration::hours(WATCH_TTL_HOURS) {
                return Err(SkipReason::InactivityTtl);
            }
        }
    }
    if let Some(reset_epoch) = watch.rate_limit_until {
        if (now_utc.timestamp() as u64) < reset_epoch {
            return Err(SkipReason::RateLimited);
        }
    }
    let effective_interval = adaptive_interval(
        watch.interval_secs,
        watch.rate_limit_remaining,
        watch.rate_limit_limit,
    );
    if !watch_is_due(watch.last_polled_at, effective_interval, now_ms) {
        return Err(SkipReason::NotDue);
    }
    let mut stamped = watch.clone();
    stamped.last_polled_at = Some(now_ms);
    stamped.effective_interval_secs = Some(effective_interval);
    Ok(PollContext {
        repo: watch.repo.clone(),
        subscribers,
        stamped_watch: stamped,
    })
}

/// #1705 (codex REJECT fix): on a repo-level batch ApiError, the rate-limit backoff
/// must cover EVERY watch of the repo — including watches that
/// `prepare_poll_context` skipped this tick (NotDue / already RateLimited) and so
/// never entered the `by_repo` eligible slice. Re-enumerate the repo's watch files
/// on disk and stamp `rate_limit_until` on each, so a later-due watch honours the
/// repo-wide stall instead of polling per-branch. ApiError is rare (rate-limit), so
/// the dir scan here is acceptable. Non-watch files (e.g. `.stall` sidecars) fail
/// the `WatchState` parse and are skipped.
fn stamp_repo_backoff(home: &Path, repo: &str, reset_epoch: u64) {
    let Ok(entries) = std::fs::read_dir(ci_watches_dir(home)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(mut watch) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str::<WatchState>(&c).ok())
        else {
            continue;
        };
        if watch.repo != repo {
            continue;
        }
        watch.rate_limit_until = Some(reset_epoch);
        let _ = crate::store::atomic_write(
            &path,
            serde_json::to_string_pretty(&watch)
                .unwrap_or_default()
                .as_bytes(),
        );
    }
}

/// Inner implementation that accepts a provider factory for testability.
/// #1705 testable seam: one repo-level batch poll → pre-populate the per-tick
/// cache so the subsequent per-watch `ci_check_repo` calls hit the cache instead
/// of each issuing a per-branch GitHub request.
///
/// - `Runs`: clear the per-repo stall, group rows by head_branch, prefill the
///   cache slot for every watch whose expected `head_sha` is present in the batch
///   slice. A watch whose branch/sha is absent (pushed off the per_page=100 page)
///   is left unprefilled → `CachedCiProvider` miss → automatic per-branch fallback.
///   Returns `true` (proceed to fan-out).
/// - `ApiError`: rate-limit is a repo property → bump the per-repo stall ONCE and
///   back off every watch (`rate_limit_until`). Returns `false` (skip fan-out).
/// - `None` (provider has no batch support) / `Err` (transient): no prefill,
///   returns `true` → fan-out falls back to per-branch polling.
async fn batch_prefill_repo(
    home: &Path,
    repo: &str,
    watches: &[(std::path::PathBuf, PollContext)],
    tick_cache: &TickCache,
    provider: &dyn CiProvider,
    subscribers: &[String],
    display_timezone: Option<&str>,
) -> bool {
    match provider.poll_repo_runs(repo).await {
        Some(Ok(RepoPollResult::Runs {
            rows,
            rate_limit_remaining,
            rate_limit_limit,
        })) => {
            clear_repo_stall_and_maybe_resume(home, repo, subscribers);
            let mut by_branch: std::collections::HashMap<String, Vec<CiRun>> =
                std::collections::HashMap::new();
            for row in rows {
                by_branch.entry(row.head_branch).or_default().push(row.run);
            }
            if let Ok(mut cache) = tick_cache.lock() {
                for (_, ctx) in watches {
                    let branch = &ctx.stamped_watch.branch;
                    let slice = match by_branch.get(branch) {
                        Some(s) => s,
                        None => continue,
                    };
                    if let Some(expected) = &ctx.stamped_watch.head_sha {
                        if !slice.iter().any(|r| &r.head_sha == expected) {
                            continue; // expected head_sha not in batch → fallback
                        }
                    }
                    cache.insert(
                        (repo.to_string(), branch.clone()),
                        Arc::new(tokio::sync::Mutex::new(Some(CiPollResult::Runs {
                            runs: slice.clone(),
                            rate_limit_remaining,
                            rate_limit_limit,
                        }))),
                    );
                }
            }
            true
        }
        Some(Ok(RepoPollResult::ApiError {
            message,
            rate_limit_reset,
            ..
        })) => {
            bump_repo_stall_and_maybe_notify(
                home,
                repo,
                subscribers,
                rate_limit_reset,
                display_timezone,
            );
            if let Some(reset) = rate_limit_reset {
                // Stamp EVERY watch of the repo — not just the eligible `watches`
                // slice. `by_repo` only holds watches that passed
                // `prepare_poll_context`; NotDue / already-RateLimited watches were
                // skipped and never entered the slice. If they don't get the repo
                // backoff stamp, they wake when due and poll per-branch in defiance
                // of the repo-wide stall — re-introducing the per-branch API calls
                // #1705 removes. (codex REJECT fix.)
                stamp_repo_backoff(home, repo, reset);
            }
            tracing::warn!(repo = %repo, message = %message, "CI batch poll API error; repo backing off");
            false
        }
        Some(Err(e)) => {
            tracing::warn!(repo = %repo, error = %e, "CI batch poll failed; falling back to per-branch");
            true
        }
        None => true, // provider doesn't support batch → per-branch fallback
    }
}

pub(super) fn check_ci_watches_with_provider(
    home: &Path,
    registry: &AgentRegistry,
    make_provider: impl Fn(&WatchState) -> Option<Box<dyn CiProvider>> + Send + Sync + 'static,
) {
    let entries = match std::fs::read_dir(ci_watches_dir(home)) {
        Ok(e) => e,
        Err(_) => return,
    };
    let tick_cache: TickCache = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let display_timezone: Option<String> =
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .ok()
            .and_then(|c| c.display_timezone);
    // #1705 PASS 1: collect poll-eligible watches grouped by repo. TTL-expired →
    // remove; RateLimited → SILENT skip (the repo-level batch poll owns stall
    // notification now — a watch's rate_limit_until is set by the batch ApiError
    // arm below, or by a per-branch fallback ApiError, so it backs off without
    // emitting a per-watch [ci-watch-stalled]).
    let mut by_repo: std::collections::HashMap<String, Vec<(std::path::PathBuf, PollContext)>> =
        std::collections::HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let watch: WatchState = match std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
        {
            Some(v) => v,
            None => continue,
        };
        let now_utc = chrono::Utc::now();
        let now_ms = now_utc.timestamp_millis();
        match prepare_poll_context(&watch, now_utc, now_ms) {
            Ok(ctx) => {
                let _ = crate::store::atomic_write(
                    &path,
                    serde_json::to_string_pretty(&ctx.stamped_watch)
                        .unwrap_or_default()
                        .as_bytes(),
                );
                by_repo
                    .entry(ctx.repo.clone())
                    .or_default()
                    .push((path, ctx));
            }
            Err(SkipReason::Expired) => {
                let a = watch.subscriber_names().join(",");
                remove_watch(home, &path, &a, &watch.repo, &watch.branch, "expired");
                tracing::info!(repo = %watch.repo, branch = %watch.branch, subscribers = %a, reason = "expired", "CI watch removed: TTL expired");
            }
            Err(SkipReason::InactivityTtl) => {
                let a = watch.subscriber_names().join(",");
                remove_watch(
                    home,
                    &path,
                    &a,
                    &watch.repo,
                    &watch.branch,
                    "inactivity_ttl",
                );
                tracing::info!(repo = %watch.repo, branch = %watch.branch, subscribers = %a, hours = WATCH_TTL_HOURS, reason = "inactivity_ttl", "CI watch removed: inactivity TTL");
            }
            // #1705: per-watch rate-limit backoff is now a silent skip.
            Err(SkipReason::RateLimited) => {}
            Err(SkipReason::NotDue | SkipReason::Invalid) => {}
        }
    }

    // #1705 PASS 2: ONE batch poll per repo (1 GitHub API call instead of N),
    // pre-populate the per-tick cache from the batch, then fan out to per-watch
    // ci_check_repo (each hits the prefilled cache, or misses → per-branch fallback).
    let make_provider = std::sync::Arc::new(make_provider);
    for (repo, watches) in by_repo {
        // CR-2026-06-14 (xcut-concurrency F3): bound the detached spawns. Claim
        // this repo's in-flight slot; if the prior cycle's batch task is still
        // running (provider/network stall), skip spawning a new one so the
        // detached-task backlog on the 2-worker runtime can't grow without limit.
        let inflight_guard = match RepoInFlightGuard::try_claim(&repo) {
            Some(g) => g,
            None => {
                tracing::debug!(
                    repo = %repo,
                    "ci poll: prior cycle's batch task still in flight — skipping this tick to bound the detached-spawn backlog"
                );
                continue;
            }
        };
        // Union of subscribers across the repo's watches — recipients of the
        // single repo-level stalled/resumed health event.
        let mut subs_union: Vec<String> = Vec::new();
        for (_, ctx) in &watches {
            for s in &ctx.subscribers {
                if !subs_union.contains(s) {
                    subs_union.push(s.clone());
                }
            }
        }
        let home_buf = home.to_path_buf();
        let registry = Arc::clone(registry);
        let tick_cache = Arc::clone(&tick_cache);
        let make_provider = Arc::clone(&make_provider);
        let display_timezone = display_timezone.clone();
        // fire-and-forget: one batch-poll-then-fan-out task per repo per poll cycle
        // — bounded to at most one in-flight per repo by `inflight_guard`, which is
        // moved in below and released (via Drop) on EVERY return path of this task.
        shared_ci_runtime().spawn(async move {
            let _inflight_guard = inflight_guard;
            // Batch poll once for the whole repo, pre-populating the per-tick cache.
            // None provider → no prefill (per-branch fallback). Returns false on a
            // repo-level ApiError (backing off) → skip the fan-out this tick.
            let proceed = match make_provider(&watches[0].1.stamped_watch) {
                Some(p) => {
                    batch_prefill_repo(
                        &home_buf,
                        &repo,
                        &watches,
                        &tick_cache,
                        p.as_ref(),
                        &subs_union,
                        display_timezone.as_deref(),
                    )
                    .await
                }
                None => true, // no provider → per-branch fallback in fan-out
            };
            if !proceed {
                return;
            }
            // Fan out: per-watch ci_check_repo. Cache hit → no HTTP; miss → per-branch.
            for (path, ctx) in watches {
                let provider: Box<dyn CiProvider> = match make_provider(&ctx.stamped_watch) {
                    Some(p) => Box::new(CachedCiProvider {
                        inner: p,
                        poll_cache: Arc::clone(&tick_cache),
                    }),
                    None => {
                        tracing::warn!(repo = %ctx.repo, "ci_check: failed to build CI provider");
                        continue;
                    }
                };
                let PollContext {
                    repo: r,
                    subscribers,
                    stamped_watch,
                } = ctx;
                if let Err(e) = ci_check_repo(
                    &home_buf,
                    &path,
                    stamped_watch,
                    subscribers,
                    &registry,
                    provider.as_ref(),
                )
                .await
                {
                    tracing::warn!(repo = %r, error = %e, "CI check failed");
                }
            }
        });
    }
}

/// Select runs from a CI poll result that should trigger notifications.
/// Returns indices into `runs` of terminal runs ordered oldest-first
/// so notifications arrive chronologically. In-progress runs
/// (conclusion=None) are skipped.
///
/// #786 — rerun on same run_id (`gh run rerun --failed` re-executes
/// the same workflow attempt; run_id unchanged, conclusion transitions
/// failure→success). Pre-#786 logic dropped these because the filter
/// was strictly `run.id > last_run_id`. With this fix a run is also
/// included when `run.id == last_run_id` AND its conclusion differs
/// from `last_notified_conclusion` — bounded by conclusion change so a
/// stable terminal state doesn't re-spam subscribers.
pub(crate) fn select_runs_to_notify(
    runs: &[CiRun],
    last_run_id: Option<u64>,
    last_notified_conclusion: Option<&str>,
    last_notified_run_attempt: Option<u64>,
    last_notified_run_conclusion: Option<&str>,
) -> Vec<usize> {
    let threshold = last_run_id.unwrap_or(0);
    // #1991: the anchor run's own conclusion is the correct baseline for the
    // id==threshold comparison below. `last_notified_conclusion` is the per-sha
    // AGGREGATE — when the two legitimately differ (the max-id run succeeded
    // while a sibling workflow at the same sha carried the verdict), comparing
    // the run against the aggregate never matches, so the same terminal run
    // re-selected every poll (the #1991 ~60s storm). Legacy watch files
    // (pre-#1991, field absent) fall back to the aggregate — the old behavior —
    // and self-heal on the first notify that persists the new field.
    let per_run_baseline = last_notified_run_conclusion.or(last_notified_conclusion);
    let mut selected: Vec<(usize, u64)> = runs
        .iter()
        .enumerate()
        .filter_map(|(i, run)| {
            // Skip non-terminal (in-progress) runs first — conclusion
            // is the precondition for either inclusion path below.
            run.conclusion.as_ref()?;
            if run.id < threshold {
                // Strictly older than last seen — ignore.
                return None;
            }
            if run.id == threshold {
                // Same run_id as last notified. #786: include when the
                // conclusion changed (rerun changed outcome). #1859 Fix B: ALSO
                // include when the `run_attempt` advanced — a `gh run rerun`
                // keeps the id+conclusion and only bumps the attempt, so an equal
                // conclusion at a NEW attempt is a fresh event (flake re-run),
                // not a stable terminal state. Suppress only when BOTH are equal.
                let same_conclusion = run.conclusion.as_deref() == per_run_baseline;
                let attempt_advanced =
                    last_notified_run_attempt.is_some_and(|prev| run.run_attempt > prev);
                if same_conclusion && !attempt_advanced {
                    return None;
                }
            }
            Some((i, run.id))
        })
        .collect();
    // Sort oldest-first by run_id
    selected.sort_by_key(|&(_, id)| id);
    selected.into_iter().map(|(i, _)| i).collect()
}

/// Pure function: deduplicate terminal runs by head_sha.
/// Returns `(run_index, run_id, head_sha)` tuples, one per unique sha,
/// keeping the latest run_id per sha. Sorted by run_id (oldest first)
/// for chronological notification order.
///
/// #786 — precedence with `select_runs_to_notify`: that filter runs
/// FIRST and already enforces the `(run_id, conclusion)` change
/// invariant. This filter is the SECOND gate, keyed on `head_sha`. A
/// run with `head_sha == last_notified_sha` was previously dropped
/// unconditionally — that path swallowed rerun outcomes when a new
/// `run_id` re-executed the SAME commit (different attempt path, same
/// sha). The `last_notified_conclusion` arg makes this site
/// conclusion-aware in parallel with site 1: same sha is allowed
/// through only when at least one of the candidate runs has a
/// conclusion that differs from the last notified conclusion.
pub(crate) fn dedupe_notifications_by_head_sha<'a>(
    runs: &'a [CiRun],
    to_notify: &[usize],
    last_notified_sha: Option<&str>,
    last_notified_conclusion: Option<&str>,
    last_notified_run_attempt: Option<u64>,
) -> Vec<(usize, u64, &'a str)> {
    let mut best: std::collections::HashMap<&str, (usize, u64)> = std::collections::HashMap::new();
    for &idx in to_notify {
        let run = &runs[idx];
        let sha = run.head_sha.as_str();
        let id = run.id;
        best.entry(sha)
            .and_modify(|e| {
                if id > e.1 {
                    *e = (idx, id);
                }
            })
            .or_insert((idx, id));
    }
    let notify_set: std::collections::HashSet<usize> = to_notify.iter().copied().collect();
    let mut result: Vec<_> = best
        .into_iter()
        .filter(|(sha, (idx, _))| {
            if last_notified_sha != Some(*sha) {
                return true;
            }
            // #1307: aggregate only over to_notify runs so stale failed
            // runs (filtered by gate 1) don't poison the conclusion.
            let conclusion_changed = aggregate_conclusion_for_indices(runs, &notify_set, sha)
                != last_notified_conclusion;
            // #1859 Fix B: a same-sha, same-conclusion run whose `run_attempt`
            // advanced (a `gh run rerun`) is a fresh notifiable event, not a
            // stable terminal state to suppress.
            let attempt_advanced =
                last_notified_run_attempt.is_some_and(|prev| runs[*idx].run_attempt > prev);
            conclusion_changed || attempt_advanced
        })
        .map(|(sha, (idx, id))| (idx, id, sha))
        .collect();
    result.sort_by_key(|&(_, id, _)| id);
    result
}

/// Aggregate conclusion for all runs matching a given head_sha.
/// Returns None if any run is still in-progress (conclusion is None).
/// Returns Some("failure") if any run failed.
/// Returns Some("success") only if all runs succeeded.
/// Returns None if no runs match.
/// #972 reviewer-rejection fix: extract `review_class` from a ci-watch
/// JSON value. `"dual"` (case-insensitive) → `ReviewClass::Dual`; any
/// other value (including absent / null / unknown) → `ReviewClass::Single`.
/// Source of the field: `mcp_watch_ci` MCP handler accepts a
/// `review_class` argument and persists it into the watch file.
#[cfg(test)]
pub(crate) fn parse_review_class(
    watch: &serde_json::Value,
) -> crate::daemon::pr_state::ReviewClass {
    match watch
        .get("review_class")
        .and_then(|v| v.as_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("dual") => crate::daemon::pr_state::ReviewClass::Dual,
        _ => crate::daemon::pr_state::ReviewClass::Single,
    }
}

pub(crate) fn aggregate_conclusion_for_sha<'a>(runs: &'a [CiRun], sha: &str) -> Option<&'a str> {
    aggregate_conclusion_for_sha_filtered(runs, sha, None)
}

/// #1991: reduce a set of runs to ONE per workflow name — the latest attempt
/// (max `(id, run_attempt)`). GitHub keeps superseded runs visible at the same
/// sha (a concurrency-cancelled duplicate dispatch, an older attempt of a
/// rerun); letting them vote in the verdict reported a `cancelled` branch
/// verdict for an all-green head (the #1991 incident). Only the newest run of
/// each workflow speaks for that workflow.
fn latest_run_per_workflow<'a, I: IntoIterator<Item = &'a CiRun>>(runs: I) -> Vec<&'a CiRun> {
    let mut best: std::collections::HashMap<&str, &CiRun> = std::collections::HashMap::new();
    let mut unnamed: Vec<&CiRun> = Vec::new();
    for r in runs {
        if r.name.is_empty() {
            // An empty workflow name is UNKNOWN provenance — two unnamed runs
            // cannot be assumed to be the same workflow, so each keeps its
            // voice (the pre-#1991 behavior). GitHub always names runs; this
            // guards other providers / degraded parses against collapsing
            // unrelated runs into one.
            unnamed.push(r);
            continue;
        }
        best.entry(r.name.as_str())
            .and_modify(|e| {
                if (r.id, r.run_attempt) > (e.id, e.run_attempt) {
                    *e = r;
                }
            })
            .or_insert(r);
    }
    let mut out: Vec<&CiRun> = best.into_values().collect();
    out.extend(unnamed);
    out
}

/// #1991: pick the run that should REPRESENT an aggregate verdict in the
/// notification (URL, failure-log fetch). Must agree with the reported
/// conclusion — pre-#1991 the body always linked the highest-id run, which
/// produced "conclusion: cancelled" notifications linking a success run.
/// Highest `(id, run_attempt)` among latest-per-workflow runs whose own
/// conclusion equals the aggregate; None when nothing matches (caller keeps
/// its anchor run as fallback).
fn representative_run<'a>(runs: &'a [CiRun], sha: &str, aggregate: &str) -> Option<&'a CiRun> {
    latest_run_per_workflow(runs.iter().filter(|r| r.head_sha == sha))
        .into_iter()
        .filter(|r| r.conclusion.as_deref() == Some(aggregate))
        .max_by_key(|r| (r.id, r.run_attempt))
}

/// #1151: when `required_checks` is Some, only runs whose `name` matches
/// (case-insensitive) are considered. Non-matching runs are ignored entirely.
/// When None, all runs must pass (backward compat).
pub(crate) fn aggregate_conclusion_for_sha_filtered<'a>(
    runs: &'a [CiRun],
    sha: &str,
    required_checks: Option<&[String]>,
) -> Option<&'a str> {
    // #1991: one voice per workflow — latest attempt only.
    let matching: Vec<&CiRun> =
        latest_run_per_workflow(runs.iter().filter(|r| r.head_sha == sha).filter(|r| {
            required_checks
                .map(|checks| checks.iter().any(|c| c.eq_ignore_ascii_case(&r.name)))
                .unwrap_or(true)
        }));
    if matching.is_empty() {
        return None;
    }
    // Fail-fast: any failure → immediately report (don't wait for in-progress)
    if matching
        .iter()
        .any(|r| r.conclusion.as_deref() == Some("failure"))
    {
        return Some("failure");
    }
    // Still in-progress → wait for all to complete before reporting success
    if matching.iter().any(|r| r.conclusion.is_none()) {
        return None;
    }
    if let Some(r) = matching
        .iter()
        .find(|r| r.conclusion.as_deref() != Some("success"))
    {
        return r.conclusion.as_deref();
    }
    Some("success")
}

/// #1307: aggregate conclusion only over runs at the given indices.
/// Prevents stale failed runs (already filtered out by gate 1) from
/// poisoning the gate 2 dedup check on rerun.
fn aggregate_conclusion_for_indices<'a>(
    runs: &'a [CiRun],
    indices: &std::collections::HashSet<usize>,
    sha: &str,
) -> Option<&'a str> {
    // #1991: same latest-attempt-per-workflow reduction as the sha aggregate.
    let matching: Vec<&CiRun> = latest_run_per_workflow(
        runs.iter()
            .enumerate()
            .filter(|(i, r)| indices.contains(i) && r.head_sha == sha)
            .map(|(_, r)| r),
    );
    if matching.is_empty() {
        return None;
    }
    if matching
        .iter()
        .any(|r| r.conclusion.as_deref() == Some("failure"))
    {
        return Some("failure");
    }
    if matching.iter().any(|r| r.conclusion.is_none()) {
        return None;
    }
    if let Some(r) = matching
        .iter()
        .find(|r| r.conclusion.as_deref() != Some("success"))
    {
        return r.conclusion.as_deref();
    }
    Some("success")
}

/// Pure function: build the inbox body text for a CI notification.
/// Headline + optional failure detail + run URL.
/// CI-fail-notify: strip the trailing ` (Attempt N)` / ` (Retry N)` suffix
/// GitHub appends to re-run check names, so a rerun of the SAME checks hashes
/// identically and doesn't look like a changed failing set.
fn strip_attempt_suffix(name: &str) -> &str {
    if let Some(idx) = name.rfind(" (") {
        if let Some(inner) = name[idx + 2..].strip_suffix(')') {
            let lower = inner.to_ascii_lowercase();
            if lower.starts_with("attempt ") || lower.starts_with("retry ") {
                return name[..idx].trim_end();
            }
        }
    }
    name
}

/// CI-fail-notify: stable fingerprint of the SET of failing check names. The set
/// is normalized (attempt-suffix stripped, deduped, sorted) then FNV-1a hashed —
/// deterministic across processes/restarts (unlike `DefaultHasher`), so the
/// persisted `failed_set_fingerprint` compares correctly after a daemon restart.
/// Re-notify fires iff this changes → suppresses same-set re-polls and same-set
/// reruns, surfaces a genuinely different failing set.
pub(crate) fn failure_fingerprint(failed_checks: &[String]) -> String {
    use std::collections::BTreeSet;
    let set: BTreeSet<&str> = failed_checks
        .iter()
        .map(|c| strip_attempt_suffix(c))
        .collect();
    let joined = set.into_iter().collect::<Vec<_>>().join("\n");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a 64-bit
    for b in joined.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// CI-fail-notify: format a fetched failed-log blob for inline injection. Keeps
/// the LAST `max_lines` lines (the error is at the end), then UTF-8-safely
/// byte-truncates FROM THE FRONT if still over `max_bytes` (preserving the most
/// recent output), and appends a footer pointing at the full log.
pub(crate) fn format_log_tail(
    raw: &str,
    max_lines: usize,
    max_bytes: usize,
    run_id: u64,
) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let dropped_lines = lines.len() > max_lines;
    let kept = if dropped_lines {
        &lines[lines.len() - max_lines..]
    } else {
        &lines[..]
    };
    let mut tail = kept.join("\n");
    let mut dropped_bytes = false;
    if tail.len() > max_bytes {
        let mut cut = tail.len() - max_bytes;
        while cut < tail.len() && !tail.is_char_boundary(cut) {
            cut += 1; // advance to the next UTF-8 boundary
        }
        tail = tail[cut..].to_string();
        dropped_bytes = true;
    }
    if dropped_lines || dropped_bytes {
        format!("{tail}\n… (truncated; `gh run view {run_id} --log-failed` for full log)")
    } else {
        tail
    }
}

/// CI-fail-notify checklist footer. Step 1 adapts to whether the daemon
/// pre-fetched the failed-log tail (embedded above the checklist). When absent
/// — the early-fail path fires mid-run so `--log-failed` is empty, or a fetch
/// errored/timed out — point the reader at the manual command + the completion
/// notification instead of a tail that isn't there (#1537 follow-up: the old
/// unconditional "Read the failed-log tail above" was self-contradictory when
/// no tail was attached).
fn failure_checklist(run_id_str: &str, has_tail: bool) -> String {
    let step1 = if has_tail {
        format!("1. Read the failed-log tail above (full: `gh run view {run_id_str} --log-failed`)")
    } else {
        format!(
            "1. Daemon did not attach a failed-log tail (the run may still be in progress) — \
             fetch it with `gh run view {run_id_str} --log-failed` (the run-completion \
             notification will also carry it)"
        )
    };
    format!(
        "\n\n⚠ CI failure checklist:\n\
         {step1}\n\
         2. If infra flake → `gh run rerun {run_id_str} --failed`\n\
         3. If real failure → fix code, push, wait for green\n\
         4. Do NOT dismiss without evidence"
    )
}

pub(crate) fn build_inbox_body(
    headline: &str,
    conclusion: &str,
    failure_detail: Option<&str>,
    run_url: &str,
    run_id: Option<u64>,
    log_tail: Option<&str>,
) -> String {
    if conclusion == "failure" {
        let detail = failure_detail.unwrap_or("unknown step");
        let run_id_str = run_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "<run_id>".to_string());
        let mut body = format!("{headline}\nDetail: {detail}\nURL: {run_url}");
        // CI-fail-notify: embed the daemon-fetched failed-log tail inline so the
        // agent doesn't need an extra `gh run view` round-trip. None → fall back
        // to the original instruction-only body. The full-log command stays in
        // the checklist footer below.
        let has_tail = match log_tail.filter(|t| !t.is_empty()) {
            Some(tail) => {
                body.push_str(&format!("\n\n── failed log (tail) ──\n{tail}"));
                true
            }
            None => false,
        };
        body.push_str(&failure_checklist(&run_id_str, has_tail));
        body
    } else {
        format!(
            "{headline}\nURL: {run_url}\n\n\
             Next steps:\n\
             1. If review pending → reviewer picks up\n\
             2. If already reviewed → lead merges\n\
             3. Check task board for next assignment"
        )
    }
}

/// Build the notification message for a CI run conclusion.
/// Returns `None` for non-terminal states (in-progress / null conclusion).
/// Job/step detail is excluded from the headline — agents use `inbox` or
/// `gh run view` for details.
fn ci_notification_message(
    repo: &str,
    branch: &str,
    conclusion: Option<&str>,
    _failure_detail: Option<&str>,
    head_sha: Option<&str>,
) -> Option<String> {
    let conclusion = conclusion?;
    let sha_short = head_sha
        .map(|s| format!(" ({})", &s[..s.len().min(7)]))
        .unwrap_or_default();
    let msg = match conclusion {
        "failure" => format!("[ci-fail] {repo}@{branch}{sha_short}: failure"),
        "success" => format!("[ci-pass] {repo}@{branch}{sha_short}: passed ✓"),
        other => format!("[ci-ended] {repo}@{branch}{sha_short}: {other}"),
    };
    Some(msg)
}

/// Build the `[ci-conflict-detected]` inbox message produced by
/// `emit_ci_conflict_alert` (#1032). Pulled out of the inline construct
/// at the call site so the production emit and the deterministic-hint
/// test see identical bytes — same pattern as
/// [`make_ci_ready_for_action_msg`].
pub(crate) fn make_ci_conflict_alert_msg(
    repo: &str,
    branch: &str,
    source: &str,
) -> crate::inbox::InboxMessage {
    let body = format!(
        "[ci-conflict-detected] {repo}@{branch}: PR is CONFLICTING with base. \
         CI workflow trigger blocked until rebase. \
         URL: https://github.com/{repo}/pulls?q=is%3Apr+head%3A{branch} \
         (source: {source})"
    );
    crate::inbox::InboxMessage::new_system("system:ci", "ci-watch", body)
        // #946: stable grep target — canonical `{repo}@{branch}` form.
        .with_correlation_id(format!("{repo}@{branch}"))
}

/// Build the `[ci-ready-for-action]` inbox message handed to the
/// `next_after_ci` chain target on CI pass (#1030). Single construction
/// site so the production emit and the deterministic-hint test (T4)
/// see identical bytes.
///
/// #1031 enrichment: `head_sha` (full 40-char), `pr_number`, and
/// `task_id` are threaded through so reviewers receive a
/// directly-actionable payload without needing `gh pr list --head` or
/// `git rev-parse` lookups. All three fall through to `Option::None`
/// when the upstream cache (pr_state) doesn't yet have the data —
/// graceful degradation for fresh watches where the first poll hasn't
/// populated the aggregator.
pub(crate) fn make_ci_ready_for_action_msg(
    repo: &str,
    branch: &str,
    repo_branch_key: &str,
    head_sha: Option<&str>,
    pr_number: Option<u64>,
    task_id: Option<&str>,
) -> crate::inbox::InboxMessage {
    let mut msg = crate::inbox::InboxMessage::new_system(
        "system:ci",
        "ci-ready-for-action",
        format!("[ci-ready-for-action] {repo}@{branch}: CI passed, your turn."),
    )
    .with_correlation_id(repo_branch_key);
    if let Some(sha) = head_sha {
        msg = msg.with_reviewed_head(sha);
    }
    msg.task_id = task_id.map(String::from);
    msg.pr_number = pr_number;
    msg
}

// ── #1326 job-level early-fail ──

async fn check_early_job_failures(
    ctx: &CiCheckCtx<'_>,
    state: &mut WatchState,
    pr: &PollResult,
    provider: &dyn CiProvider,
) -> bool {
    if pr.current_sha.is_empty() {
        return false;
    }
    // CI-fail-notify: the SHA-only dedup moved AFTER the failing-set fingerprint
    // is computed (below) so a changed failing set on the same SHA re-notifies.
    let in_progress: Vec<&CiRun> = pr
        .runs
        .iter()
        .filter(|r| r.head_sha == pr.current_sha && r.conclusion.is_none())
        .collect();
    if in_progress.is_empty() {
        return false;
    }

    let mut all_failed: Vec<String> = Vec::new();
    let mut all_running: Vec<String> = Vec::new();
    let mut first_run_id: u64 = 0;
    let mut first_run_url = String::new();

    for run in &in_progress {
        let jobs = provider.fetch_run_jobs(ctx.repo, run.id).await;
        let failed: Vec<&CiJob> = jobs
            .iter()
            .filter(|j| j.conclusion.as_deref() == Some("failure"))
            .collect();
        if failed.is_empty() {
            continue;
        }
        if first_run_id == 0 {
            first_run_id = run.id;
            first_run_url.clone_from(&run.url);
        }
        for j in &failed {
            all_failed.push(j.name.clone());
        }
        for j in jobs.iter().filter(|j| j.conclusion.is_none()) {
            all_running.push(j.name.clone());
        }
    }

    if all_failed.is_empty() {
        return false;
    }

    // CI-fail-notify: re-notify only when the failing-check SET changes. Same
    // sha + same set (incl. same-set reruns, via the attempt-suffix strip) →
    // suppress; a different/larger failing set on the same sha → re-notify. A
    // missing stored fingerprint (legacy state notified before this field
    // existed) counts as "unchanged" so the original #1326 SHA-dedup is
    // preserved — we never spuriously re-notify an already-notified SHA.
    let fingerprint = failure_fingerprint(&all_failed);
    let already_this_sha =
        state.early_fail_notified_sha.as_deref() == Some(pr.current_sha.as_str());
    let fingerprint_unchanged = state
        .failed_set_fingerprint
        .as_deref()
        .is_none_or(|fp| fp == fingerprint);
    if already_this_sha && fingerprint_unchanged {
        return false;
    }

    let sha_short = &pr.current_sha[..pr.current_sha.len().min(7)];
    let headline = format!(
        "[ci-fail] {}@{} ({}): failure",
        ctx.repo, ctx.branch, sha_short
    );
    let detail = all_failed.join(", ");
    let mut body = format!("{headline}\nDetail: {detail}\nURL: {first_run_url}");
    if !all_running.is_empty() {
        body.push_str(&format!("\nStill running: {}", all_running.join(", ")));
    }
    // #1537 follow-up: the early-fail detector fires while the run is still in
    // progress (other jobs running, see `all_running`), so `gh run view
    // --log-failed` has nothing yet — skip the guaranteed-empty (20s-timeout-
    // prone) pre-fetch here. The run-completion notification re-fires for this
    // same failure WITH the tail (separate dedup state: this path writes only
    // `early_fail_notified_sha`, not `last_notified_conclusion`), so the agent
    // still receives the log. The no-tail checklist points there meanwhile.
    body.push_str(&failure_checklist(&first_run_id.to_string(), false));

    let repo_branch_key = format!("{}@{}", ctx.repo, ctx.branch);
    let supersede_token = format!("ci-early-{sha_short}");
    for sub in ctx.subscribers {
        crate::inbox::mark_ci_watch_superseded(ctx.home, sub, &repo_branch_key, &supersede_token);
        persist_or_log!(
            crate::inbox::enqueue_with_idle_hint(
                ctx.home,
                sub,
                crate::inbox::InboxMessage::new_system("system:ci", "ci-watch", body.clone())
                    .with_correlation_id(repo_branch_key.clone()),
            ),
            "ci_early_fail_notify",
            sub
        );
    }

    state.early_fail_notified_sha = Some(pr.current_sha.clone());
    state.failed_set_fingerprint = Some(fingerprint);
    true
}

// ── ci_check_repo decomposition (#1093) ──

struct CiCheckCtx<'a> {
    home: &'a Path,
    watch_path: &'a Path,
    repo: &'a str,
    branch: &'a str,
    subscribers: &'a [String],
}

struct RunTracking<'a> {
    last_run_id: Option<u64>,
    prev_head_sha: Option<&'a str>,
    last_notified_sha: Option<&'a str>,
    last_notified_conclusion: Option<&'a str>,
    last_notified_run_attempt: Option<u64>,
    // #1991: anchor run's own conclusion (vs the aggregate above).
    last_notified_run_conclusion: Option<&'a str>,
    last_stale_emitted_sha: Option<&'a str>,
}

struct PollResult {
    runs: Vec<CiRun>,
    current_sha: String,
    effective_last_run_id: Option<u64>,
}

/// Force-push invalidation (pure). When the branch head has moved since the
/// last poll (`prev_head_sha` is present and differs from `current_sha`), the
/// cached `last_run_id` points at a run for the OLD head and is stale, so it
/// must be reset to `None` for the new head's run to be picked up. Otherwise
/// the cached id is preserved. The production caller (`poll_ci_runs`) layers
/// the logging / progress-touch side effects on top of this decision.
pub(crate) fn effective_last_run_id(
    prev_head_sha: Option<&str>,
    current_sha: &str,
    last_run_id: Option<u64>,
) -> Option<u64> {
    if prev_head_sha.is_some_and(|prev| prev != current_sha) {
        None
    } else {
        last_run_id
    }
}

struct NotifyOutcome {
    max_notified_id: u64,
    new_notified_sha: Option<String>,
    new_notified_conclusion: Option<String>,
    new_notified_run_attempt: Option<u64>,
    // #1991: anchor run's own conclusion at notify time.
    new_notified_run_conclusion: Option<String>,
    new_stale_emitted_sha: Option<String>,
}

/// Fetch latest CI run and notify ALL subscribed agents on any
/// terminal conclusion (success, failure, cancelled, timed_out, etc.).
/// Also tracks `head_sha` — if the branch HEAD changes (e.g. force push),
/// `last_run_id` is reset so the new run is picked up.
/// On PR terminal states (merged/closed), the watcher is auto-cleared.
///
/// **Subscriber fan-out (Sprint 54 P0-1)**: `subscribers` is a slice of
/// instance names that all share this watch. The poll itself happens
/// once per cycle regardless of subscriber count (poll/subscriber split,
/// hard-contract item 1). Notifications, by contrast, fan out — every
/// subscriber receives the inbox message and the in-band agent inject
/// (item 2). The audit string for `remove_watch` joins all subscribers
/// with commas so event-log readers can see the full membership at the
/// moment of removal.
/// Single-load-per-tick entry point: receives the pre-loaded `WatchState`
/// from the caller, passes `&mut` to sub-functions, and flushes once at
/// the end when state has changed.
async fn ci_check_repo(
    home: &Path,
    watch_path: &Path,
    mut state: WatchState,
    subscribers: Vec<String>,
    registry: &AgentRegistry,
    provider: &dyn CiProvider,
) -> anyhow::Result<()> {
    let snapshot = state.clone();
    let repo = state.repo.clone();
    let branch = state.branch.clone();
    let ctx = CiCheckCtx {
        home,
        watch_path,
        repo: &repo,
        branch: &branch,
        subscribers: &subscribers,
    };
    if check_and_remove_terminal_pr(&ctx, &mut state, provider).await? {
        return Ok(());
    }
    check_and_alert_mergeable(&ctx, &mut state, provider).await;
    let prev_head_sha = state.head_sha.clone();
    let last_notified_sha = state.last_notified_head_sha.clone();
    let last_notified_conclusion = state.last_notified_conclusion.clone();
    let last_stale_emitted_sha = state.last_stale_emitted_sha.clone();
    let last_notified_run_conclusion = state.last_notified_run_conclusion.clone();
    let tracking = RunTracking {
        last_run_id: state.last_run_id,
        prev_head_sha: prev_head_sha.as_deref(),
        last_notified_sha: last_notified_sha.as_deref(),
        last_notified_conclusion: last_notified_conclusion.as_deref(),
        last_notified_run_attempt: state.last_notified_run_attempt,
        last_notified_run_conclusion: last_notified_run_conclusion.as_deref(),
        last_stale_emitted_sha: last_stale_emitted_sha.as_deref(),
    };
    let pr = match poll_ci_runs(&ctx, &tracking, &mut state, provider).await {
        Ok(Some(pr)) => pr,
        Ok(None) => {
            if state != snapshot {
                flush_watch_state(watch_path, &state);
            }
            // #1750 A2: do NOT refresh `expires_at` here. `Ok(None)` means the
            // poll found NO runs for the branch — a deleted/merged-away branch or
            // a branch CI never ran. Refreshing on this case is exactly what kept
            // 56 stale watches alive forever (each empty poll bumped expiry +72h).
            // Withholding the refresh lets a runless watch finally age past its
            // existing `expires_at` and get GC'd. A still-active watch (CI in
            // progress / runs present) refreshes via the run-bearing paths below.
            return Ok(());
        }
        Err(e) => {
            if state != snapshot {
                flush_watch_state(watch_path, &state);
            }
            return Err(e);
        }
    };
    // #2008: head-aware ci-handoff invalidation. The poll has the branch's CURRENT
    // head (`pr.current_sha`); if a pending ci-handoff track recorded an OLDER head
    // (a push/force-push has since moved the branch), its ci-ready obligation is
    // stale — resolve it so the handoff watchdog stops re-nudging a dead head until
    // merge/24h (the operator-observed renudge loop). Pre-#2008 tracks (no recorded
    // head) are left alone. This only ADDS a resolve condition — no new re-send path.
    if !pr.current_sha.is_empty() {
        crate::daemon::ci_handoff_track::resolve_head_advanced(
            ctx.home,
            &format!("{}@{}", ctx.repo, ctx.branch),
            &pr.current_sha,
        );
    }
    // #1326: check in-progress runs for early job-level failures before
    // the terminal-only notification gates below.
    check_early_job_failures(&ctx, &mut state, &pr, provider).await;

    let to_notify = select_runs_to_notify(
        &pr.runs,
        pr.effective_last_run_id,
        tracking.last_notified_conclusion,
        tracking.last_notified_run_attempt,
        tracking.last_notified_run_conclusion,
    );
    if to_notify.is_empty() {
        // #1991: did anything actually change this cycle? A branch whose runs
        // are all terminal and all already-notified produces an UNCHANGED
        // quiet poll — pre-#1991 it still re-stamped `last_terminal_seen_at`
        // and re-pushed `expires_at` +72h EVERY cycle, so a finished PR-less
        // branch (spike/audit) polled forever (the #1991 quota burn: 7 stale
        // watches × 60s). In-progress runs at the head keep the watch alive.
        let head_in_progress = pr
            .runs
            .iter()
            .any(|r| r.head_sha == pr.current_sha && r.conclusion.is_none());
        let activity = head_in_progress
            || state.last_run_id != pr.effective_last_run_id
            || state.head_sha.as_deref() != Some(pr.current_sha.as_str());
        if let Some(id) = pr.effective_last_run_id {
            state.last_run_id = Some(id);
            if !pr.current_sha.is_empty() {
                state.head_sha = Some(pr.current_sha.clone());
            }
            if activity {
                state.last_terminal_seen_at = Some(chrono::Utc::now().to_rfc3339());
            }
        }
        if state != snapshot {
            flush_watch_state(watch_path, &state);
        }
        if activity {
            refresh_expires_at(watch_path);
        }
        return Ok(());
    }
    let deduped = dedupe_notifications_by_head_sha(
        &pr.runs,
        &to_notify,
        tracking.last_notified_sha,
        tracking.last_notified_conclusion,
        tracking.last_notified_run_attempt,
    );
    let outcome =
        fan_out_notifications(&ctx, &state, &pr, &deduped, &tracking, registry, provider).await;
    persist_watch_state(&ctx, &pr, &outcome, &mut state);
    if state != snapshot {
        flush_watch_state(watch_path, &state);
    }
    refresh_expires_at(watch_path);
    Ok(())
}

async fn check_and_remove_terminal_pr(
    ctx: &CiCheckCtx<'_>,
    state: &mut WatchState,
    provider: &dyn CiProvider,
) -> anyhow::Result<bool> {
    let pr_state = provider.check_pr_terminal(ctx.repo, ctx.branch).await;

    let PrState::Terminal { merged } = pr_state else {
        if state.terminal_since.is_some() {
            state.terminal_since = None;
            tracing::info!(
                repo = ctx.repo,
                branch = ctx.branch,
                "PR no longer terminal — cleared terminal_since marker"
            );
        }
        return Ok(false);
    };

    if let Some(expires_at) = state.expires_at.as_deref() {
        if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(expires_at) {
            let watch_age =
                exp.with_timezone(&chrono::Utc) - chrono::Duration::hours(WATCH_TTL_HOURS);
            let since_creation = chrono::Utc::now().signed_duration_since(watch_age);
            if since_creation < chrono::Duration::seconds(60) {
                tracing::info!(
                    repo = ctx.repo,
                    branch = ctx.branch,
                    merged,
                    "skipping PR-terminal auto-clear — watch too young (<60s)"
                );
                return Ok(false);
            }
        }
    }

    // Two-consecutive-terminal guard (#1267)
    if state.terminal_since.is_none() {
        state.terminal_since = Some(chrono::Utc::now().to_rfc3339());
        tracing::info!(
            repo = ctx.repo,
            branch = ctx.branch,
            merged,
            "PR terminal (first observation) — deferring removal to next poll"
        );
        return Ok(false);
    }

    let audit_label = ctx.subscribers.join(",");
    let watch_age_str = state
        .expires_at
        .as_deref()
        .and_then(|e| chrono::DateTime::parse_from_rfc3339(e).ok())
        .map(|exp| {
            let created =
                exp.with_timezone(&chrono::Utc) - chrono::Duration::hours(WATCH_TTL_HOURS);
            let age = chrono::Utc::now().signed_duration_since(created);
            format!("{}s", age.num_seconds())
        })
        .unwrap_or_else(|| "unknown".to_string());
    remove_watch(
        ctx.home,
        ctx.watch_path,
        &audit_label,
        ctx.repo,
        ctx.branch,
        "pr_terminal",
    );
    tracing::info!(
        repo = ctx.repo,
        branch = ctx.branch,
        merged,
        subscribers = %audit_label,
        watch_age = %watch_age_str,
        reason = "pr_terminal",
        "CI watcher removed: PR terminal (two consecutive observations)"
    );
    if merged {
        crate::status_summary::auto_close_merged_tasks(ctx.home, ctx.branch);
        crate::daemon::auto_release::auto_release_for_merged_branch(ctx.home, ctx.repo, ctx.branch);
    }
    Ok(true)
}

async fn check_and_alert_mergeable(
    ctx: &CiCheckCtx<'_>,
    state: &mut WatchState,
    provider: &dyn CiProvider,
) {
    const MERGEABLE_RECHECK_INTERVAL_SECS: i64 = 300;
    let last = state
        .last_mergeable_check_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc));
    let now = chrono::Utc::now();
    let should_recheck = last
        .map(|d| now.signed_duration_since(d).num_seconds() >= MERGEABLE_RECHECK_INTERVAL_SECS)
        .unwrap_or(true);
    if !should_recheck {
        return;
    }
    let prev_mergeable = state.last_mergeable_state.clone();
    let new_state = provider.check_pr_mergeable(ctx.repo, ctx.branch).await;
    state.last_mergeable_state = Some(new_state.as_str().to_string());
    state.last_mergeable_check_at = Some(now.to_rfc3339());
    if matches!(new_state, MergeableState::Conflicting)
        && prev_mergeable.as_deref() != Some("CONFLICTING")
    {
        emit_ci_conflict_alert(
            ctx.home,
            ctx.repo,
            ctx.branch,
            ctx.subscribers,
            "poll-transition",
        );
    }
}

async fn poll_ci_runs(
    ctx: &CiCheckCtx<'_>,
    tracking: &RunTracking<'_>,
    state: &mut WatchState,
    provider: &dyn CiProvider,
) -> anyhow::Result<Option<PollResult>> {
    let poll_result = provider.poll_runs(ctx.repo, ctx.branch).await?;
    match poll_result {
        CiPollResult::ApiError {
            status,
            message,
            rate_limit_reset,
        } => {
            if let Some(reset_epoch) = rate_limit_reset {
                state.rate_limit_until = Some(reset_epoch);
            }
            let notify_msg = match rate_limit_reset {
                Some(reset) => format!(
                    "[ci-warn] {}@{}: {message} — backoff until reset (epoch {reset})\n\n\
                     Action checklist:\n\
                     1. Check GitHub status page (githubstatus.com)\n\
                     2. If rate-limited → wait, polling will auto-resume\n\
                     3. If persistent >30min → escalate to operator\n\
                     4. If token error → report to operator",
                    ctx.repo, ctx.branch
                ),
                None => format!(
                    "[ci-warn] {}@{}: {message}\n\n\
                     Action checklist:\n\
                     1. Check GitHub status page (githubstatus.com)\n\
                     2. If rate-limited → wait, polling will auto-resume\n\
                     3. If persistent >30min → escalate to operator\n\
                     4. If token error → report to operator",
                    ctx.repo, ctx.branch
                ),
            };
            if let Some(ch) = crate::channel::active_channel() {
                for sub in ctx.subscribers {
                    let _ = crate::channel::gated_notify(
                        ch.as_ref(),
                        sub,
                        crate::channel::NotifySeverity::Warn,
                        &notify_msg,
                        false,
                    );
                }
            }
            Err(anyhow::anyhow!("{status}: {message}"))
        }
        CiPollResult::Runs {
            runs,
            rate_limit_remaining,
            rate_limit_limit,
        } => {
            if let Some(r) = rate_limit_remaining {
                state.rate_limit_remaining = Some(r);
            }
            if let Some(l) = rate_limit_limit {
                state.rate_limit_limit = Some(l);
            }
            clear_stall_state(state, ctx.home, ctx.repo, ctx.branch, ctx.subscribers);
            if runs.is_empty() {
                return Ok(None);
            }
            let current_sha = runs
                .first()
                .map(|r| r.head_sha.as_str())
                .unwrap_or("")
                .to_string();
            let head_moved = tracking
                .prev_head_sha
                .is_some_and(|prev| prev != current_sha);
            let effective =
                effective_last_run_id(tracking.prev_head_sha, &current_sha, tracking.last_run_id);
            if head_moved {
                tracing::info!(
                    repo = ctx.repo,
                    branch = ctx.branch,
                    old_sha = ?tracking.prev_head_sha,
                    new_sha = %current_sha,
                    "head_sha changed, resetting run tracking"
                );
                let _ =
                    crate::daemon::task_progress::touch_progress_for_branch(ctx.home, ctx.branch);
            }
            Ok(Some(PollResult {
                runs,
                current_sha,
                effective_last_run_id: effective,
            }))
        }
    }
}

async fn fan_out_notifications(
    ctx: &CiCheckCtx<'_>,
    state: &WatchState,
    pr: &PollResult,
    deduped: &[(usize, u64, &str)],
    tracking: &RunTracking<'_>,
    registry: &AgentRegistry,
    provider: &dyn CiProvider,
) -> NotifyOutcome {
    let mut max_notified_id = pr.effective_last_run_id.unwrap_or(0);
    let mut new_notified_sha = tracking.last_notified_sha.map(String::from);
    let mut new_notified_conclusion = tracking.last_notified_conclusion.map(String::from);
    let mut new_notified_run_attempt = tracking.last_notified_run_attempt;
    let mut new_notified_run_conclusion = tracking.last_notified_run_conclusion.map(String::from);
    let mut new_stale_emitted_sha = tracking.last_stale_emitted_sha.map(String::from);

    for (idx, run_id, sha) in deduped {
        let run = &pr.runs[*idx];
        let conclusion = aggregate_conclusion_for_sha(&pr.runs, sha);
        if conclusion.is_none() {
            continue;
        }
        if *run_id > max_notified_id {
            max_notified_id = *run_id;
        }

        if *sha != pr.current_sha {
            if new_stale_emitted_sha.as_deref() == Some(*sha) {
                new_notified_sha = Some(sha.to_string());
                new_notified_conclusion = conclusion.map(String::from);
                new_notified_run_attempt = Some(run.run_attempt);
                new_notified_run_conclusion = run.conclusion.clone();
                continue;
            }
            tracing::info!(
                repo = ctx.repo,
                branch = ctx.branch,
                stale_sha = %sha,
                current_sha = %pr.current_sha,
                "dropping stale CI notification (newer commit on branch)"
            );
            new_notified_sha = Some(sha.to_string());
            new_notified_conclusion = conclusion.map(String::from);
            new_notified_run_attempt = Some(run.run_attempt);
            new_notified_run_conclusion = run.conclusion.clone();
            new_stale_emitted_sha = Some(sha.to_string());
            continue;
        }

        if let Some(headline) =
            ci_notification_message(ctx.repo, ctx.branch, conclusion, None, Some(sha))
        {
            // #1991: the linked URL / fetched failure logs must come from a run
            // whose own conclusion matches the verdict being reported. The
            // anchor (`run`, highest id at the sha) can be e.g. a success run
            // while a sibling workflow carried the failure — pre-#1991 the body
            // linked the wrong run (and fetched failure logs from a green run).
            let rep = conclusion
                .and_then(|c| representative_run(&pr.runs, sha, c))
                .unwrap_or(run);
            let failure_detail = if conclusion == Some("failure") {
                Some(provider.fetch_failure_summary(ctx.repo, rep.id).await)
            } else {
                None
            };
            // CI-fail-notify: daemon pre-fetches the failed-log tail so the
            // agent doesn't need an extra `gh run view` round-trip. Async on the
            // ci runtime (the provider impl uses `gh … --log-failed` via
            // tokio::process — never block_on the shared runtime, #1476).
            let log_tail = if conclusion == Some("failure") {
                provider.fetch_failure_log_tail(ctx.repo, rep.id, 120).await
            } else {
                None
            };
            let body = build_inbox_body(
                &headline,
                conclusion.unwrap_or(""),
                failure_detail.as_deref(),
                &rep.url,
                Some(rep.id),
                log_tail.as_deref(),
            );

            let repo_branch_key = format!("{}@{}", ctx.repo, ctx.branch);
            let supersede_token = format!("ci-{}-{}", run_id, sha);
            let action_target_on_success: Option<&str> = if conclusion == Some("success") {
                state.next_after_ci.as_deref().filter(|s| !s.is_empty())
            } else {
                None
            };
            let fleet_cfg =
                crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(ctx.home)).ok();
            // #event-bus (ci_watch, Option A): ONE skip-aware loop over subscribers.
            // The skips (action_target_on_success + #931 zombie-subscriber) are
            // applied ONCE. Per recipient: success/failure → emit Ci{Ready,Fail}
            // (the subscriber delivers via the shared `deliver_ci_watch`); a
            // non-pair "[ci-ended]" conclusion has no event kind → direct deliver.
            for sub in ctx.subscribers {
                if action_target_on_success == Some(sub.as_str()) {
                    continue;
                }
                // #1441: registry is UUID-keyed; resolve subscriber via fleet.yaml.
                let in_registry = crate::fleet::resolve_uuid(ctx.home, sub)
                    .is_some_and(|id| agent::lock_registry(registry).contains_key(&id));
                let fleet_known = fleet_cfg
                    .as_ref()
                    .map(|f| f.instances.contains_key(sub))
                    .unwrap_or(true);
                if !in_registry && !fleet_known {
                    tracing::debug!(
                        sub = %sub,
                        repo = %ctx.repo,
                        branch = %ctx.branch,
                        "#931 Fix 3: skipping inbox enqueue for zombie subscriber (not in registry, not in fleet roster)"
                    );
                    continue;
                }
                // #event-bus Step 2 (legacy-zero): the success/failure pair emits
                // (the subscriber delivers via deliver_ci_watch). A non-pair
                // "[ci-ended]" conclusion has no event kind → direct deliver.
                let emitted = match conclusion {
                    Some("failure") => {
                        crate::daemon::event_bus::global().emit(
                            ctx.home,
                            crate::daemon::event_bus::EventKind::CiFail {
                                target: sub.clone(),
                                body: body.clone(),
                                correlation_id: repo_branch_key.clone(),
                                supersede_token: supersede_token.clone(),
                            },
                        );
                        true
                    }
                    Some("success") => {
                        crate::daemon::event_bus::global().emit(
                            ctx.home,
                            crate::daemon::event_bus::EventKind::CiReady {
                                target: sub.clone(),
                                body: body.clone(),
                                correlation_id: repo_branch_key.clone(),
                                supersede_token: supersede_token.clone(),
                            },
                        );
                        true
                    }
                    _ => false,
                };
                if !emitted {
                    deliver_ci_watch(ctx.home, sub, &body, &repo_branch_key, &supersede_token);
                }
            }
        }
        new_notified_sha = Some(sha.to_string());
        new_notified_conclusion = conclusion.map(String::from);
        new_notified_run_attempt = Some(run.run_attempt);
        // #1991: persist the ANCHOR run's own conclusion — gate 1 compares the
        // id==threshold run against this (per-run vs per-run), not against the
        // aggregate (the pre-#1991 oscillation).
        new_notified_run_conclusion = run.conclusion.clone();
    }

    NotifyOutcome {
        max_notified_id,
        new_notified_sha,
        new_notified_conclusion,
        new_notified_run_attempt,
        new_notified_run_conclusion,
        new_stale_emitted_sha,
    }
}

/// #event-bus (ci_watch): shared deliver for one subscriber's CI notify — supersede
/// prior ci-watch msgs for this (sub, repo@branch), then enqueue the RENDERED body
/// to the subscriber's inbox (with the idle-hint wake). Called by BOTH the legacy
/// direct path AND the event-bus subscriber, so the inbox enqueue is byte-identical
/// by construction; the idle-hint PTY wake is covered by the same invariant.
/// Must NOT run under the registry lock (#1492 self-IPC-under-lock) — both callers
/// invoke it lock-free (the legacy loop's registry lock is a dropped temporary; the
/// subscriber fires from `emit`, outside any lock).
fn deliver_ci_watch(
    home: &std::path::Path,
    sub: &str,
    body: &str,
    repo_branch_key: &str,
    supersede_token: &str,
) {
    crate::inbox::mark_ci_watch_superseded(home, sub, repo_branch_key, supersede_token);
    persist_or_log!(
        crate::inbox::enqueue_with_idle_hint(
            home,
            sub,
            crate::inbox::InboxMessage::new_system("system:ci", "ci-watch", body.to_string())
                .with_correlation_id(repo_branch_key.to_string()),
        ),
        "ci_watch_notify",
        sub
    );
}

/// #event-bus (ci_watch) subscriber: re-deliver a `CiReady`/`CiFail` event via the
/// shared `deliver_ci_watch`. Both kinds deliver identically (the body already
/// encodes pass/fail); the kind split is purely semantic.
fn handle_event(event: &crate::daemon::event_bus::Event) -> bool {
    match &event.kind {
        crate::daemon::event_bus::EventKind::CiReady {
            target,
            body,
            correlation_id,
            supersede_token,
            ..
        }
        | crate::daemon::event_bus::EventKind::CiFail {
            target,
            body,
            correlation_id,
            supersede_token,
            ..
        } => {
            deliver_ci_watch(&event.home, target, body, correlation_id, supersede_token);
            true
        }
        _ => false,
    }
}

/// Register the ci-watch subscriber once at daemon startup (`run_core`).
/// Home-agnostic — the home travels on each event.
pub(crate) fn register_subscriber() {
    crate::daemon::event_bus::global().subscribe(handle_event);
}

fn persist_watch_state(
    ctx: &CiCheckCtx<'_>,
    pr: &PollResult,
    outcome: &NotifyOutcome,
    state: &mut WatchState,
) {
    if outcome.new_notified_sha.is_some() {
        // t-ci-ready-robust-fallback-design (PR-1, option C): the actionable
        // `[ci-ready-for-action]` is a CHAIN signal — it routes to `next_after_ci`
        // (the workflow's next agent). When there is NO chain, non-chain
        // subscribers ALREADY receive the informational `[ci-pass]` (correct
        // semantics: no "your turn" exists without a chain), so we do NOT forge an
        // actionable signal for them. (Verified not load-bearing: `[ci-pass]`
        // carries the SAME `repo@branch` correlation_id, nothing keys on the
        // ci-ready correlation for tracking, and CI-done tracking via
        // `record_ci_result` below is keyed on repo/branch independent of routing.)
        // The ONE genuinely-silent case — no `next_after_ci` AND no subscribers
        // (a malformed watch) — is loud-logged rather than dropped silently.
        if aggregate_conclusion_for_sha_filtered(
            &pr.runs,
            &pr.current_sha,
            state.required_checks.as_deref(),
        ) == Some("success")
        {
            match state.next_after_ci.as_deref().filter(|s| !s.is_empty()) {
                Some(next) => {
                    let repo_branch_key = format!("{}@{}", ctx.repo, ctx.branch);
                    let pr_state = crate::daemon::pr_state::load(ctx.home, ctx.repo, ctx.branch);
                    // #t-92758 P1(b): don't emit ci-ready for a merge-BLOCKED PR
                    // (REJECTED verdict / Draft) — the chain target can't act on it,
                    // so emitting only spawns a re-nudge loop. This handles the
                    // reject-before-CI ordering; the evict in `pr_state::scanner`
                    // handles the CI-green-then-reject ordering (#2297). The
                    // predicate NEVER suppresses VERIFIED/green/None — the normal
                    // "your turn" handoff stays live (is_ci_ready_merge_blocked iron
                    // rule).
                    if pr_state
                        .as_ref()
                        .is_some_and(crate::daemon::pr_state::is_ci_ready_merge_blocked)
                    {
                        tracing::info!(
                            target: "ci_watch",
                            repo = ctx.repo,
                            branch = ctx.branch,
                            "ci-ready suppressed — PR merge-blocked (REJECTED/Draft); no chain handoff, no track"
                        );
                    } else {
                        let pr_number = pr_state.as_ref().map(|s| s.pr_number);
                        let task_id = state.task_id.as_deref();
                        let msg = make_ci_ready_for_action_msg(
                            ctx.repo,
                            ctx.branch,
                            &repo_branch_key,
                            Some(&pr.current_sha),
                            pr_number,
                            task_id,
                        );
                        persist_or_log!(
                            crate::inbox::enqueue_with_idle_hint(ctx.home, next, msg),
                            "ci_watch_chain",
                            next
                        );
                        // #1888 phase-2: track the handoff until RESOLUTION (report /
                        // PR terminal / target claims the branch), decoupled from the
                        // inbox read-state the watchdog used to scan (any drain marked
                        // it read within seconds and blinded the re-nudge).
                        crate::daemon::ci_handoff_track::record(
                            ctx.home,
                            next,
                            &repo_branch_key,
                            &chrono::Utc::now().to_rfc3339(),
                            // #2008: anchor the track to the head it was recorded for
                            // so a later head move can invalidate it (head-aware
                            // resolve).
                            Some(&pr.current_sha),
                        );
                    }
                }
                None if state.subscriber_names().is_empty() => {
                    tracing::warn!(
                        target: "ci_watch",
                        repo = ctx.repo,
                        branch = ctx.branch,
                        "CI passed but watch has no next_after_ci AND no subscribers — \
                         no one to notify (malformed watch); not dropping silently"
                    );
                }
                None => {}
            }
        }

        let last_conclusion = aggregate_conclusion_for_sha(&pr.runs, &pr.current_sha);
        let conclusion = match last_conclusion {
            Some("success") => crate::daemon::pr_state::CiConclusion::Green,
            Some(other) => crate::daemon::pr_state::CiConclusion::Failed { conclusion: other },
            None => crate::daemon::pr_state::CiConclusion::Pending,
        };
        let subscriber_names = state.subscriber_names();
        let review_class = match state
            .review_class
            .as_deref()
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("dual") => crate::daemon::pr_state::ReviewClass::Dual,
            _ => crate::daemon::pr_state::ReviewClass::Single,
        };
        crate::daemon::pr_state::record_ci_result(
            ctx.home,
            ctx.repo,
            ctx.branch,
            &pr.current_sha,
            conclusion,
            subscriber_names,
            review_class,
        );
    }

    state.last_run_id = Some(outcome.max_notified_id);
    if !pr.current_sha.is_empty() {
        state.head_sha = Some(pr.current_sha.clone());
    }
    if let Some(sha) = &outcome.new_notified_sha {
        state.last_notified_head_sha = Some(sha.clone());
    }
    if let Some(c) = &outcome.new_notified_conclusion {
        state.last_notified_conclusion = Some(c.clone());
    }
    if let Some(a) = outcome.new_notified_run_attempt {
        state.last_notified_run_attempt = Some(a);
    }
    if let Some(c) = &outcome.new_notified_run_conclusion {
        state.last_notified_run_conclusion = Some(c.clone());
    }
    if let Some(s) = &outcome.new_stale_emitted_sha {
        state.last_stale_emitted_sha = Some(s.clone());
    }
    state.last_terminal_seen_at = Some(chrono::Utc::now().to_rfc3339());
}

/// Refresh `expires_at` to now + 72h after a successful poll (#1267).
fn refresh_expires_at(watch_path: &Path) {
    let lock_path = watch_path.with_extension("lock");
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(l) => l,
        Err(_) => return,
    };
    let Ok(content) = std::fs::read_to_string(watch_path) else {
        return;
    };
    let Ok(mut watch) = serde_json::from_str::<WatchState>(&content) else {
        return;
    };
    watch.expires_at =
        Some((chrono::Utc::now() + chrono::Duration::hours(WATCH_TTL_HOURS)).to_rfc3339());
    // #2004: a swallowed write here silently lets the watch expire early —
    // PR branches self-heal via PR-3 auto-arm, but non-PR branches genuinely
    // lose CI coverage. Surface it (non-fatal: next successful poll retries).
    if let Err(e) = crate::store::atomic_write(
        watch_path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    ) {
        // If this proves noisy in production (it fires per poll while the
        // write keeps failing), add a once-per-watch latch — visibility
        // first, rate-limit on evidence (#2008 discipline).
        tracing::warn!(path = %watch_path.display(), error = %e,
            "ci-watch expires_at refresh write failed — watch may expire early (non-PR branches lose coverage without auto-rearm)");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "poller_tests.rs"]
mod tests;
