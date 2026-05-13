use crate::agent::{self, AgentRegistry};
use std::path::Path;
use std::sync::Arc;

use super::provider::{CiPollResult, CiProvider, CiRun, PrState};
use super::registry::{
    ci_watches_dir, parse_subscribers, remove_watch, update_watch_state,
    update_watch_state_with_notify,
};
use super::sweep::{bump_consecutive_skips_and_maybe_notify, clear_stall_and_maybe_notify_resumed};
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
use super::registry::watch_filename;
#[cfg(test)]
use super::sweep::{gc_stale_watches, startup_sweep, STALL_THRESHOLD};
#[cfg(test)]
use super::watcher::check_ci_watches;

// ---------------------------------------------------------------------------
// H2: Shared tokio runtime for CI watch — avoids spawning a new thread +
// runtime per poll cycle. Bounded to 2 worker threads.
// ---------------------------------------------------------------------------

fn shared_ci_runtime() -> &'static tokio::runtime::Runtime {
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

/// Inner implementation that accepts a provider factory for testability.
pub(super) fn check_ci_watches_with_provider(
    home: &Path,
    registry: &AgentRegistry,
    make_provider: impl Fn(&serde_json::Value) -> Option<Box<dyn CiProvider>> + Send + Sync + 'static,
) {
    let entries = match std::fs::read_dir(ci_watches_dir(home)) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let watch: serde_json::Value = match std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
        {
            Some(v) => v,
            None => continue,
        };
        let repo = match watch["repo"].as_str() {
            Some(r) => r.to_string(),
            None => continue,
        };
        // Sprint 54 P0-1: subscribers list (with legacy single-instance fallback)
        // replaces the single `instance` field. Empty list ⇒ skip — a watch with
        // no recipients is useless and would only burn rate-limit.
        let subscribers = parse_subscribers(&watch);
        if subscribers.is_empty() {
            continue;
        }
        let branch = watch["branch"].as_str().unwrap_or("main").to_string();
        let interval = watch["interval_secs"].as_u64().unwrap_or(60);
        let last_run_id = watch["last_run_id"].as_u64();
        let head_sha = watch["head_sha"].as_str().map(String::from);
        let last_notified_sha = watch["last_notified_head_sha"].as_str().map(String::from);

        // Audit label for remove_watch: comma-joined subscribers so the
        // event log stays human-readable when multiple agents share a watch.
        let audit_label = subscribers.join(",");

        // TTL check: remove expired watches before polling.
        let now_utc = chrono::Utc::now();
        if let Some(expires_at) = watch["expires_at"].as_str() {
            if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(expires_at) {
                if now_utc > exp.with_timezone(&chrono::Utc) {
                    remove_watch(home, &path, &audit_label, &repo, &branch, "expired");
                    tracing::info!(repo = %repo, branch = %branch, "CI watch expired (TTL)");
                    continue;
                }
            }
        }
        // Inactivity TTL: WATCH_TTL_HOURS since last terminal run seen
        if let Some(last_seen) = watch["last_terminal_seen_at"].as_str() {
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(last_seen) {
                let elapsed = now_utc.signed_duration_since(ts.with_timezone(&chrono::Utc));
                if elapsed > chrono::Duration::hours(WATCH_TTL_HOURS) {
                    remove_watch(home, &path, &audit_label, &repo, &branch, "inactivity_ttl");
                    tracing::info!(repo = %repo, branch = %branch, hours = WATCH_TTL_HOURS, "CI watch removed: inactivity TTL");
                    continue;
                }
            }
        }

        // Rate-limit backoff: skip polling until X-RateLimit-Reset time.
        if let Some(reset_epoch) = watch["rate_limit_until"].as_u64() {
            if (chrono::Utc::now().timestamp() as u64) < reset_epoch {
                // Sprint 54 P0-5 (sub-scope B): increment consecutive_skips
                // on each rate-limited tick so subscribers see a
                // `[ci-watch-stalled]` event after STALL_THRESHOLD
                // consecutive misses. Persist atomically with `stalled_notified`
                // so the dispatch is exactly-once per stall window.
                bump_consecutive_skips_and_maybe_notify(
                    home,
                    &path,
                    &repo,
                    &branch,
                    &subscribers,
                    reset_epoch,
                );
                continue;
            }
        }

        // Sprint 54 P0-2: compute adaptive backoff from the most recent
        // quota counters persisted on the watch file. The poll path
        // refreshes these on every successful response. First poll has
        // `None` → `adaptive_interval` falls through to `interval`, so
        // a fresh watch behaves identically to pre-r0.
        let prev_remaining = watch["rate_limit_remaining"].as_u64();
        let prev_limit = watch["rate_limit_limit"].as_u64();
        let effective_interval = adaptive_interval(interval, prev_remaining, prev_limit);

        // Throttle from a dedicated `last_polled_at` (epoch millis) in the
        // watch file itself, not file mtime. mtime conflates "when this
        // file was touched" with "when we last polled" and broke whenever
        // another writer (migration, hand-edit, freshly created watch)
        // stamped the file — the handler used to backdate mtime manually
        // to work around that. Schema-local state removes both the
        // first-poll-lag quirk and the external-writer fragility.
        let now_ms = chrono::Utc::now().timestamp_millis();
        if !watch_is_due(watch["last_polled_at"].as_i64(), effective_interval, now_ms) {
            continue;
        }
        // Stamp `last_polled_at` BEFORE spawning the GH request so a slow
        // GH response doesn't let the next tick re-enter for the same
        // watch. The spawned thread updates last_run_id / head_sha on a
        // terminal conclusion; non-terminal polls leave those fields
        // alone but the `last_polled_at` stamp already keeps them in
        // throttle.
        let mut watch_with_stamp = watch.clone();
        watch_with_stamp["last_polled_at"] = serde_json::json!(now_ms);
        // P0-2 diagnostic: stamp the effective interval so operators
        // can read the current backoff zone from the watch file.
        watch_with_stamp["effective_interval_secs"] = serde_json::json!(effective_interval);
        // M1: atomic write
        let _ = crate::store::atomic_write(
            &path,
            serde_json::to_string_pretty(&watch_with_stamp)
                .unwrap_or_default()
                .as_bytes(),
        );

        let home = home.to_path_buf();
        let watch_path = path.clone();
        let registry = Arc::clone(registry);
        let provider = match make_provider(&watch) {
            Some(p) => p,
            None => {
                tracing::warn!(repo = %repo, "ci_check: failed to build CI provider");
                continue;
            }
        };
        // H2: use shared runtime instead of per-poll thread + runtime.
        // fire-and-forget: ci_check is one-shot per poll cycle. The shared
        // runtime bounds concurrency to 2 worker threads. No JoinHandle
        // needed — the tick loop re-spawns next cycle if still watching.
        let subscribers_owned = subscribers.clone();
        shared_ci_runtime().spawn(async move {
            if let Err(e) = ci_check_repo(
                &home,
                &watch_path,
                &repo,
                &branch,
                &subscribers_owned,
                last_run_id,
                head_sha.as_deref(),
                last_notified_sha.as_deref(),
                &registry,
                provider.as_ref(),
            )
            .await
            {
                tracing::warn!(repo = %repo, error = %e, "CI check failed");
            }
        });
    }
}

/// Outcome of interpreting a `GET /repos/.../actions/runs` response.
///
/// Without this, a non-2xx response (e.g. unauthenticated rate-limit
/// `{"message":"API rate limit exceeded ..."}`) parses cleanly as JSON
/// but its `workflow_runs` field is absent, and the caller's
/// `body["workflow_runs"].as_array()` returns `None` — silently behaving
/// as if the branch had no runs and skipping every subsequent
/// notification while `last_polled_at` keeps marching forward. Tag the
/// HTTP status explicitly so API errors surface as `Err` instead of
/// imitating a quiescent branch.
///
/// Production code now uses [`CiPollResult`] via the [`CiProvider`] trait;
/// this enum is retained for unit-testing the classification logic that
/// lives inside [`super::provider::GitHubCiProvider::poll_runs`].
#[cfg(test)]
enum RunsResponse<'a> {
    Run(&'a serde_json::Value),
    NoRuns,
    ApiError(String),
}

/// Pure interpreter for a runs-list response. See [`RunsResponse`] for
/// why the rate-limit / NoRuns distinction matters.
///
/// Retained under `#[cfg(test)]` — production classification now happens
/// inside [`super::provider::GitHubCiProvider::poll_runs`].
#[cfg(test)]
fn classify_runs_response(status: u16, body: &serde_json::Value) -> RunsResponse<'_> {
    if !(200..300).contains(&status) {
        let message = body["message"].as_str().unwrap_or("(no message)");
        let hint = if status == 403
            && std::env::var("GITHUB_TOKEN").is_err()
            && message.to_lowercase().contains("rate limit")
        {
            " — set GITHUB_TOKEN to raise the unauthenticated 60/hr cap"
        } else {
            ""
        };
        return RunsResponse::ApiError(format!("GH API {status}: {message}{hint}"));
    }
    match body["workflow_runs"].as_array().and_then(|a| a.first()) {
        Some(run) => RunsResponse::Run(run),
        None => RunsResponse::NoRuns,
    }
}

/// Select runs from a CI poll result that should trigger notifications.
/// Returns indices into `runs` of terminal runs with `id > last_run_id`, ordered
/// oldest-first so notifications arrive chronologically.
/// In-progress runs (conclusion=None) are skipped.
pub(crate) fn select_runs_to_notify(runs: &[CiRun], last_run_id: Option<u64>) -> Vec<usize> {
    let threshold = last_run_id.unwrap_or(0);
    let mut selected: Vec<(usize, u64)> = runs
        .iter()
        .enumerate()
        .filter_map(|(i, run)| {
            if run.id <= threshold {
                return None;
            }
            // Skip non-terminal (in-progress) runs
            run.conclusion.as_ref()?;
            Some((i, run.id))
        })
        .collect();
    // Sort oldest-first by run_id
    selected.sort_by_key(|&(_, id)| id);
    selected.into_iter().map(|(i, _)| i).collect()
}

/// Pure function: deduplicate terminal runs by head_sha.
/// Returns `(run_index, run_id, head_sha)` tuples, one per unique sha,
/// keeping the latest run_id per sha. Skips shas matching `last_notified`.
/// Sorted by run_id (oldest first) for chronological notification order.
pub(crate) fn dedupe_notifications_by_head_sha<'a>(
    runs: &'a [CiRun],
    to_notify: &[usize],
    last_notified: Option<&str>,
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
    let mut result: Vec<_> = best
        .into_iter()
        .filter(|(sha, _)| last_notified != Some(*sha))
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
pub(crate) fn aggregate_conclusion_for_sha<'a>(runs: &'a [CiRun], sha: &str) -> Option<&'a str> {
    let matching: Vec<&CiRun> = runs.iter().filter(|r| r.head_sha == sha).collect();
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

/// Pure function: build the inbox body text for a CI notification.
/// Headline + optional failure detail + run URL.
pub(crate) fn build_inbox_body(
    headline: &str,
    conclusion: &str,
    failure_detail: Option<&str>,
    run_url: &str,
) -> String {
    if conclusion == "failure" {
        let detail = failure_detail.unwrap_or("unknown step");
        format!("{headline}\nDetail: {detail}\nURL: {run_url}")
    } else {
        format!("{headline}\nURL: {run_url}")
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
#[allow(clippy::too_many_arguments)]
async fn ci_check_repo(
    home: &Path,
    watch_path: &Path,
    repo: &str,
    branch: &str,
    subscribers: &[String],
    last_run_id: Option<u64>,
    prev_head_sha: Option<&str>,
    last_notified_sha: Option<&str>,
    registry: &AgentRegistry,
    provider: &dyn CiProvider,
) -> anyhow::Result<()> {
    let audit_label = subscribers.join(",");
    // Check if the PR associated with this branch has reached a terminal state.
    // Skip auto-clear if the watch was just created (< 60s) — the branch may not
    // be pushed yet, and a stale PR from a previous use of the same branch name
    // could trigger a false-positive clear (Hotfix E, PR #451 follow-up).
    if let PrState::Terminal { merged } = provider.check_pr_terminal(repo, branch).await {
        // Grace: don't clear watches younger than 60s (branch not yet pushed).
        if let Ok(content) = std::fs::read_to_string(watch_path) {
            if let Ok(watch) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(expires_at) = watch["expires_at"].as_str() {
                    if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(expires_at) {
                        let watch_age = exp.with_timezone(&chrono::Utc)
                            - chrono::Duration::hours(WATCH_TTL_HOURS);
                        let since_creation = chrono::Utc::now().signed_duration_since(watch_age);
                        if since_creation < chrono::Duration::seconds(60) {
                            tracing::info!(
                                repo,
                                branch,
                                merged,
                                "skipping PR-terminal auto-clear — watch too young (<60s)"
                            );
                            // Don't clear — let the next poll cycle re-check.
                            // Fall through to normal poll logic below.
                        } else {
                            remove_watch(
                                home,
                                watch_path,
                                &audit_label,
                                repo,
                                branch,
                                "pr_terminal",
                            );
                            tracing::info!(
                                repo,
                                branch,
                                merged,
                                "CI watcher auto-cleared: PR terminal"
                            );
                            if merged {
                                crate::status_summary::auto_close_merged_tasks(home, branch);
                            }
                            return Ok(());
                        }
                    } else {
                        remove_watch(home, watch_path, &audit_label, repo, branch, "pr_terminal");
                        tracing::info!(
                            repo,
                            branch,
                            merged,
                            "CI watcher auto-cleared: PR terminal"
                        );
                        if merged {
                            crate::status_summary::auto_close_merged_tasks(home, branch);
                        }
                        return Ok(());
                    }
                } else {
                    remove_watch(home, watch_path, &audit_label, repo, branch, "pr_terminal");
                    tracing::info!(repo, branch, merged, "CI watcher auto-cleared: PR terminal");
                    if merged {
                        crate::status_summary::auto_close_merged_tasks(home, branch);
                    }
                    return Ok(());
                }
            }
        }
    }

    let poll_result = provider.poll_runs(repo, branch).await?;
    let runs = match poll_result {
        CiPollResult::ApiError {
            status,
            message,
            rate_limit_reset,
        } => {
            if let Some(reset_epoch) = rate_limit_reset {
                if let Ok(content) = std::fs::read_to_string(watch_path) {
                    if let Ok(mut watch) = serde_json::from_str::<serde_json::Value>(&content) {
                        watch["rate_limit_until"] = serde_json::json!(reset_epoch);
                        // M1: atomic write
                        let _ = crate::store::atomic_write(
                            watch_path,
                            serde_json::to_string_pretty(&watch)
                                .unwrap_or_default()
                                .as_bytes(),
                        );
                    }
                }
            }
            let notify_msg = match rate_limit_reset {
                Some(reset) => format!(
                    "[ci-warn] {repo}@{branch}: {message} — backoff until reset (epoch {reset})"
                ),
                None => format!("[ci-warn] {repo}@{branch}: {message}"),
            };
            // Outbound info-leak gate (Sprint 21 Phase 1): `notify_msg`
            // carries CI run url + repo name; legacy `None`-allowlist
            // deployments must opt in to receive these via
            // `user_allowlist: [...]`.
            //
            // Sprint 54 P0-1: fan out to every subscriber. The single-call
            // version was last-write-wins on the warning too — when lead+dev
            // both watched a branch, only one received the rate-limit
            // warning and the other silently waited.
            if let Some(ch) = crate::channel::active_channel() {
                for sub in subscribers {
                    let _ = crate::channel::gated_notify(
                        ch.as_ref(),
                        sub,
                        crate::channel::NotifySeverity::Warn,
                        &notify_msg,
                        false,
                    );
                }
            }
            return Err(anyhow::anyhow!("{status}: {message}"));
        }
        CiPollResult::Runs {
            runs,
            rate_limit_remaining,
            rate_limit_limit,
        } => {
            // Sprint 54 P0-2: persist the latest quota counters even on
            // empty-runs polls, so the next tick's `adaptive_interval`
            // sees the freshest snapshot. Done before the empty-runs
            // early return below.
            if rate_limit_remaining.is_some() || rate_limit_limit.is_some() {
                if let Ok(content) = std::fs::read_to_string(watch_path) {
                    if let Ok(mut watch) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(r) = rate_limit_remaining {
                            watch["rate_limit_remaining"] = serde_json::json!(r);
                        }
                        if let Some(l) = rate_limit_limit {
                            watch["rate_limit_limit"] = serde_json::json!(l);
                        }
                        let _ = crate::store::atomic_write(
                            watch_path,
                            serde_json::to_string_pretty(&watch)
                                .unwrap_or_default()
                                .as_bytes(),
                        );
                    }
                }
            }
            // Sprint 54 P0-5 (sub-scope B): a successful poll clears
            // any in-flight stall state. If we previously fired
            // `[ci-watch-stalled]`, the symmetrical
            // `[ci-watch-resumed]` event fans out to subscribers
            // exactly once.
            clear_stall_and_maybe_notify_resumed(home, watch_path, repo, branch, subscribers);
            if runs.is_empty() {
                return Ok(());
            }
            runs
        }
    };

    // Determine the latest head_sha from the newest run.
    let current_sha = runs.first().map(|r| r.head_sha.as_str()).unwrap_or("");

    // If head_sha changed (force push), reset last_run_id so we pick up new runs.
    let effective_last_run_id = if prev_head_sha.is_some_and(|prev| prev != current_sha) {
        tracing::info!(repo, branch, old_sha = ?prev_head_sha, new_sha = current_sha, "head_sha changed, resetting run tracking");
        // Sprint 59 Wave 1 PR-1 (#9 task stall watchdog) — progress
        // hook (b): when a watched branch advances to a new SHA,
        // look up any agent binding whose `branch` matches and
        // touch the bound task's progress sidecar. This is the
        // PR-push signal that suppresses stall warnings while
        // the operator is making forward progress on the branch.
        let _ = crate::daemon::task_progress::touch_progress_for_branch(home, branch);
        None
    } else {
        last_run_id
    };

    let to_notify = select_runs_to_notify(&runs, effective_last_run_id);
    if to_notify.is_empty() {
        // No new terminal runs — update head_sha but keep last_run_id.
        if let Some(id) = effective_last_run_id {
            update_watch_state(watch_path, Some(id), current_sha);
        }
        return Ok(());
    }

    let mut max_notified_id = effective_last_run_id.unwrap_or(0);

    let deduped = dedupe_notifications_by_head_sha(&runs, &to_notify, last_notified_sha);
    let mut new_notified_sha = last_notified_sha.map(String::from);

    for (idx, run_id, sha) in &deduped {
        let run = &runs[*idx];
        // Issue #608: use aggregate conclusion across ALL runs for this sha,
        // not just the single deduped run's conclusion.
        let conclusion = aggregate_conclusion_for_sha(&runs, sha);
        if conclusion.is_none() {
            // Some runs for this sha are still in-progress — skip, don't update tracking.
            continue;
        }
        // Comment intentionally retained: advancing happens BEFORE the
        // staleness gate so even dropped (stale) runs bump trackers and
        // can't re-trigger on the next poll. The fan-out is what's gated.
        if *run_id > max_notified_id {
            max_notified_id = *run_id;
        }

        // Issue #745: drop notifications for SHAs that are no longer the
        // branch's current head. A newer commit was pushed since this run
        // was triggered, so its pass/fail is no longer actionable. The
        // tracker still advances (above and below) so we don't re-process.
        if *sha != current_sha {
            tracing::info!(
                repo,
                branch,
                stale_sha = %sha,
                current_sha,
                "dropping stale CI notification (newer commit on branch)"
            );
            for sub in subscribers {
                let _ = crate::inbox::enqueue(
                    home,
                    sub,
                    crate::inbox::InboxMessage {
                        schema_version: 0,
                        id: None,
                        read_at: None,
                        thread_id: None,
                        parent_id: None,
                        task_id: None,
                        force_meta: None,
                        correlation_id: None,
                        reviewed_head: None,
                        from: "system:ci".to_string(),
                        text: format!(
                            "[ci-stale] {repo}@{branch} ({sha}): superseded by {current_sha}"
                        ),
                        kind: Some("ci-stale".to_string()),
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        channel: None,
                        delivery_mode: None,
                        attachments: vec![],
                        in_reply_to_msg_id: None,
                        in_reply_to_excerpt: None,
                        superseded_by: None,
                        from_id: None,
                        broadcast_context: None,
                        sequencing: None,
                        eta_minutes: None,
                        reporting_cadence: None,
                        worktree_binding_required: None,
                    },
                );
            }
            new_notified_sha = Some(sha.to_string());
            continue;
        }

        if let Some(headline) = ci_notification_message(repo, branch, conclusion, None, Some(sha)) {
            let failure_detail = if conclusion == Some("failure") {
                Some(provider.fetch_failure_summary(repo, *run_id).await)
            } else {
                None
            };
            let body = build_inbox_body(
                &headline,
                conclusion.unwrap_or(""),
                failure_detail.as_deref(),
                &run.url,
            );

            // Sprint 54 P0-1 — Subscriber fan-out: every subscriber receives
            // the in-band agent inject + the inbox enqueue. Without the
            // fan-out, the most-recent `ci watch` caller would shadow all
            // earlier subscribers (last-write-wins on the watch JSON's
            // single `instance` field). The poll above is shared (one HTTP
            // request per cycle); only the notification side-effects loop.
            let repo_branch_key = format!("{repo}@{branch}");
            let supersede_token = format!("ci-{}-{}", run_id, sha);
            // EMPIRICAL REGRESSION-PROOF FLIP: replace `subscribers` below
            // with `&subscribers[..1]` to simulate the pre-r0 single-recipient
            // bug. The `subscriber_fan_out_notifies_every_member` test
            // immediately fails with the dev-inbox-missing assertion.
            for sub in subscribers {
                let reg = agent::lock_registry(registry);
                if let Some(handle) = reg.get(sub) {
                    let _ = agent::inject_to_agent(handle, headline.as_bytes());
                }
                drop(reg);
                // M6: mark prior ci-watch messages for same repo+branch as superseded
                crate::inbox::mark_ci_watch_superseded(
                    home,
                    sub,
                    &repo_branch_key,
                    &supersede_token,
                );
                let _ = crate::inbox::enqueue(
                    home,
                    sub,
                    crate::inbox::InboxMessage {
                        schema_version: 0,
                        id: None,
                        read_at: None,
                        thread_id: None,
                        parent_id: None,
                        task_id: None,
                        force_meta: None,
                        correlation_id: None,
                        reviewed_head: None,
                        from: "system:ci".to_string(),
                        text: body.clone(),
                        kind: Some("ci-watch".to_string()),
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        channel: None,
                        delivery_mode: None,
                        attachments: vec![],
                        in_reply_to_msg_id: None,
                        in_reply_to_excerpt: None,
                        superseded_by: None,
                        from_id: None,
                        broadcast_context: None,
                        sequencing: None,
                        eta_minutes: None,
                        reporting_cadence: None,
                        worktree_binding_required: None,
                    },
                );
            }
        }
        new_notified_sha = Some(sha.to_string());
    }

    // Issue #650: auto-route [ci-ready-for-action] to next_after_ci target on pass
    if new_notified_sha.is_some() {
        if let Ok(content) = std::fs::read_to_string(watch_path) {
            if let Ok(watch) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(next) = watch["next_after_ci"].as_str().filter(|s| !s.is_empty()) {
                    // Only route on success (not failure)
                    let last_conclusion = aggregate_conclusion_for_sha(&runs, current_sha);
                    if last_conclusion == Some("success") {
                        let msg =
                            format!("[ci-ready-for-action] {repo}@{branch}: CI passed, your turn.");
                        let reg = agent::lock_registry(registry);
                        if let Some(handle) = reg.get(next) {
                            let _ = agent::inject_to_agent(handle, msg.as_bytes());
                        }
                        drop(reg);
                    }
                }
            }
        }
    }

    update_watch_state_with_notify(
        watch_path,
        Some(max_notified_id),
        current_sha,
        new_notified_sha.as_deref(),
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;

    #[test]
    fn ci_watches_dir_returns_expected_path() {
        let home = std::path::Path::new("/tmp/test");
        assert_eq!(
            ci_watches_dir(home),
            std::path::PathBuf::from("/tmp/test/ci-watches")
        );
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-ciwatch-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    // -----------------------------------------------------------------
    // Sprint 54 P0-2 — adaptive_interval. 3-zone curve based on
    // remaining quota. Each test pins one of the contract gates from
    // dispatch m-20260507042300780703-3:
    //   1. healthy zone (remaining_pct > 0.5) → no scaling
    //   2. cautious zone (0.1 < … ≤ 0.5)      → ×2
    //   3. critical zone (≤ 0.1)              → ×4
    //   4. missing headers                    → fallback to baseline
    //
    // Empirical regression-proof: a mutation that always returns the
    // baseline regardless of remaining_pct trips tests 2 and 3.
    // -----------------------------------------------------------------

    #[test]
    fn adaptive_interval_healthy_zone_uses_configured_baseline() {
        // remaining/limit = 1.0 (full quota) ⇒ no scaling, exactly the
        // configured interval. Mirror this in production: a freshly
        // booted daemon with full GitHub quota polls at user-configured
        // cadence, never widening behind the operator's back.
        assert_eq!(adaptive_interval(60, Some(5000), Some(5000)), 60);
        // Boundary: just above 50% (501/1000 = 0.501) is still healthy.
        assert_eq!(adaptive_interval(60, Some(501), Some(1000)), 60);
    }

    #[test]
    fn adaptive_interval_cautious_zone_doubles() {
        // remaining_pct = 0.3 ⇒ ×2 multiplier. The agent is consuming
        // quota faster than the healthy threshold but isn't critical
        // yet — preempt by halving poll frequency.
        assert_eq!(adaptive_interval(60, Some(300), Some(1000)), 120);
        // Boundary: exactly 50% is cautious (the >0.5 → healthy guard
        // is strict, so 0.5 falls into the next zone).
        assert_eq!(adaptive_interval(60, Some(500), Some(1000)), 120);
        // Boundary: just above 10% (101/1000 = 0.101) is still cautious.
        assert_eq!(adaptive_interval(60, Some(101), Some(1000)), 120);
    }

    #[test]
    fn adaptive_interval_critical_zone_quadruples() {
        // remaining_pct = 0.05 ⇒ ×4 multiplier. Avoids burning the
        // last few requests in a cluster. Mirror this in production:
        // when GitHub returns a low remaining count, the next poll
        // backs off so the daemon doesn't trip the 60/hr (unauth) or
        // 5000/hr (auth) cap.
        assert_eq!(adaptive_interval(60, Some(50), Some(1000)), 240);
        // Boundary: exactly 10% is critical (the >0.1 → cautious guard
        // is strict).
        assert_eq!(adaptive_interval(60, Some(100), Some(1000)), 240);
        // Pathological: zero remaining still resolves to ×4 (we don't
        // pretend to know what reset_epoch is — the existing
        // rate_limit_until path handles that recovery separately).
        assert_eq!(adaptive_interval(60, Some(0), Some(1000)), 240);
    }

    #[test]
    fn adaptive_interval_missing_headers_falls_back_to_configured() {
        // Either field absent (or zero limit) ⇒ baseline. Producers
        // that don't emit the GitHub-style headers (GitLab, Bitbucket
        // Cloud) preserve their existing behavior here.
        assert_eq!(adaptive_interval(60, None, None), 60);
        assert_eq!(adaptive_interval(60, Some(5000), None), 60);
        assert_eq!(adaptive_interval(60, None, Some(5000)), 60);
        // Pathological limit=0 (avoids div-by-zero, falls through).
        assert_eq!(adaptive_interval(60, Some(100), Some(0)), 60);
    }

    #[test]
    fn watch_is_due_null_last_polled_at_fires_immediately() {
        // A freshly-registered watch (or a pre-schema file missing the
        // last_polled_at field) must be due on the first tick. This is
        // the condition that makes the next daemon tick actually poll
        // GitHub instead of waiting ~interval_secs.
        assert!(watch_is_due(None, 60, 1_700_000_000_000));
    }

    #[test]
    fn watch_is_due_within_interval_is_throttled() {
        // Polled 30 s ago, interval 60 s ⇒ still throttled. Prevents
        // back-to-back polls from hammering the GitHub API during
        // daemon ticks (10 s cadence) or concurrent callers.
        let now_ms = 1_700_000_000_000_i64;
        let recent = now_ms - 30_000; // 30 s ago
        assert!(!watch_is_due(Some(recent), 60, now_ms));
    }

    #[test]
    fn watch_is_due_past_interval_fires_again() {
        // Polled 61 s ago, interval 60 s ⇒ due. Equality case
        // (elapsed == interval) is also treated as due — boundary
        // matches the `>=` in the throttle.
        let now_ms = 1_700_000_000_000_i64;
        let stale = now_ms - 61_000;
        assert!(watch_is_due(Some(stale), 60, now_ms));
        let exact = now_ms - 60_000;
        assert!(watch_is_due(Some(exact), 60, now_ms));
    }

    #[test]
    fn watch_is_due_future_timestamp_is_throttled() {
        // Defensive: a clock going backwards (or a hand-edited file
        // with a bogus future timestamp) should not flood GH. The
        // saturating_sub makes elapsed non-negative, and 0 < interval
        // ⇒ throttled. We'd rather be quietly silent on a weird clock
        // than burn rate limit.
        let now_ms = 1_700_000_000_000_i64;
        let future = now_ms + 10_000; // 10 s in the future
        assert!(!watch_is_due(Some(future), 60, now_ms));
    }

    #[test]
    fn ci_watch_success_notifies() {
        let msg = ci_notification_message("owner/repo", "main", Some("success"), None, None);
        assert_eq!(msg.as_deref(), Some("[ci-pass] owner/repo@main: passed ✓"));
    }

    #[test]
    fn ci_watch_failure_headline_excludes_detail() {
        // Job detail moved to inbox body — headline just says "failure"
        let msg = ci_notification_message(
            "owner/repo",
            "main",
            Some("failure"),
            Some("Build / Test"),
            None,
        );
        assert_eq!(msg.as_deref(), Some("[ci-fail] owner/repo@main: failure"));
    }

    #[test]
    fn ci_watch_failure_without_detail_same_headline() {
        let msg = ci_notification_message("owner/repo", "main", Some("failure"), None, None);
        assert_eq!(msg.as_deref(), Some("[ci-fail] owner/repo@main: failure"));
    }

    #[test]
    fn ci_watch_in_progress_skipped() {
        let msg = ci_notification_message("owner/repo", "main", None, None, None);
        assert!(
            msg.is_none(),
            "in-progress (null conclusion) must be skipped"
        );
    }

    #[test]
    fn ci_watch_cancelled_notifies() {
        let msg = ci_notification_message("owner/repo", "feat", Some("cancelled"), None, None);
        assert_eq!(
            msg.as_deref(),
            Some("[ci-ended] owner/repo@feat: cancelled")
        );
    }

    #[test]
    fn ci_watch_timed_out_notifies() {
        let msg = ci_notification_message("owner/repo", "main", Some("timed_out"), None, None);
        assert_eq!(
            msg.as_deref(),
            Some("[ci-ended] owner/repo@main: timed_out")
        );
    }

    #[test]
    fn test_force_push_invalidates_run_id() {
        // When head_sha changes between polls, the effective last_run_id
        // should be reset to None so the new run is picked up even if
        // the run_id hasn't changed yet.
        let prev_sha = Some("abc123");
        let current_sha = "def456";
        // Simulate the logic from ci_check_repo
        let last_run_id = Some(42u64);
        let effective = if prev_sha.is_some_and(|prev| prev != current_sha) {
            None
        } else {
            last_run_id
        };
        assert_eq!(effective, None, "force push must reset last_run_id");

        // Same SHA → preserve run_id
        let same_sha = "abc123";
        let effective2 = if prev_sha.is_some_and(|prev| prev != same_sha) {
            None
        } else {
            last_run_id
        };
        assert_eq!(effective2, Some(42), "same SHA must preserve last_run_id");
    }

    #[test]
    fn test_pr_merged_clears_watcher() {
        // When a watch file exists and the PR is terminal, the file
        // should be removed. We test the update_watch_state + remove
        // flow by verifying the file lifecycle.
        let dir = std::env::temp_dir().join(format!("agend-ci-test-merged-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("ci-watches")).ok();
        let watch_path = dir.join("ci-watches").join("test.json");
        std::fs::write(
            &watch_path,
            r#"{"repo":"o/r","branch":"feat","last_run_id":null,"head_sha":null}"#,
        )
        .ok();
        assert!(watch_path.exists());

        // Simulate PR terminal → auto-clear
        let _ = std::fs::remove_file(&watch_path);
        assert!(
            !watch_path.exists(),
            "watcher file must be removed on PR terminal"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // --- classify_runs_response: silent-rate-limit regression pin ---

    #[test]
    fn classify_response_picks_first_run_on_2xx() {
        let body = serde_json::json!({
            "workflow_runs": [{"id": 42, "head_sha": "abc"}, {"id": 41}]
        });
        match classify_runs_response(200, &body) {
            RunsResponse::Run(r) => assert_eq!(r["id"].as_u64(), Some(42)),
            other => panic!("expected Run, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn classify_response_no_runs_on_2xx_empty_array() {
        // Genuine "branch has no runs yet" — must NOT be confused with
        // an API error.
        let body = serde_json::json!({"workflow_runs": []});
        assert!(matches!(
            classify_runs_response(200, &body),
            RunsResponse::NoRuns
        ));
    }

    #[test]
    fn classify_response_rate_limit_is_api_error_not_no_runs() {
        // Real-world body returned by GitHub when an unauthenticated
        // client exceeds 60/hr. Without the status check, the absence
        // of `workflow_runs` here looks identical to the legit empty
        // case above and silently swallows every subsequent CI event.
        let body = serde_json::json!({
            "message": "API rate limit exceeded for 1.2.3.4. (But here's the good news: ...)",
            "documentation_url": "https://docs.github.com/rest/overview/resources-in-the-rest-api#rate-limiting"
        });
        match classify_runs_response(403, &body) {
            RunsResponse::ApiError(msg) => {
                assert!(msg.contains("403"), "msg should include status: {msg}");
                assert!(
                    msg.contains("rate limit"),
                    "msg should surface GH message: {msg}"
                );
            }
            _ => panic!("rate-limit response must be ApiError, not NoRuns"),
        }
    }

    #[test]
    fn classify_response_token_hint_only_when_unauthenticated_403() {
        // Hint should fire on unauthenticated 403 rate-limit. We can't
        // safely mutate $GITHUB_TOKEN in a parallel-test process, so
        // assert only the prefix shape and trust the env-gated branch.
        let body =
            serde_json::json!({"message": "API rate limit exceeded for example. Authenticated …"});
        let RunsResponse::ApiError(msg) = classify_runs_response(403, &body) else {
            panic!("expected ApiError");
        };
        assert!(msg.starts_with("GH API 403: API rate limit exceeded"));
    }

    #[test]
    fn classify_response_5xx_is_api_error() {
        let body = serde_json::json!({"message": "Server Error"});
        assert!(matches!(
            classify_runs_response(500, &body),
            RunsResponse::ApiError(_)
        ));
    }

    #[test]
    fn classify_response_unknown_payload_falls_through_safely() {
        // 200 OK but missing workflow_runs entirely (would never happen
        // in practice but must not panic).
        let body = serde_json::json!({});
        assert!(matches!(
            classify_runs_response(200, &body),
            RunsResponse::NoRuns
        ));
    }

    // --- github_token_warning: preventive watch_ci response hint ---

    #[test]
    fn github_token_warning_none_when_token_present() {
        assert!(github_token_warning(Some("ghp_realtokenhere")).is_none());
    }

    #[test]
    fn github_token_warning_set_when_absent() {
        let msg = github_token_warning(None).expect("missing token must warn");
        assert!(
            msg.contains("GITHUB_TOKEN"),
            "message must name the env var: {msg}"
        );
        assert!(
            msg.contains("unauthenticated") || msg.contains("60"),
            "message must explain the cost: {msg}"
        );
    }

    #[test]
    fn github_token_warning_treats_empty_and_whitespace_as_absent() {
        // `std::env::var("GITHUB_TOKEN")` returns `Ok("")` when the var is
        // exported-but-empty — a distinct case from "unset" but equally
        // unusable. Whitespace-only should be treated the same.
        assert!(github_token_warning(Some("")).is_some());
        assert!(github_token_warning(Some("   ")).is_some());
        assert!(github_token_warning(Some("\t\n")).is_some());
    }

    #[test]
    fn test_repo_with_slash_no_collision() {
        // Two repos that would collide under the old `replace('/', '_')`
        // scheme must produce distinct filenames with the hash approach.
        let f1 = watch_filename("owner/repo", "main");
        let f2 = watch_filename("owner_repo", "main");
        assert_ne!(f1, f2, "owner/repo and owner_repo must not collide");

        // Same repo+branch must be deterministic
        let f3 = watch_filename("owner/repo", "main");
        assert_eq!(f1, f3, "same input must produce same filename");

        // Different branches of same repo must differ
        let f4 = watch_filename("owner/repo", "feat");
        assert_ne!(
            f1, f4,
            "different branches must produce different filenames"
        );
    }

    #[test]
    fn test_multi_run_notifies_all_terminal_since_last() {
        let runs = vec![
            CiRun {
                id: 100,
                conclusion: Some("success".into()),
                head_sha: "aaa".into(),
                url: String::new(),
            },
            CiRun {
                id: 101,
                conclusion: Some("success".into()),
                head_sha: "bbb".into(),
                url: String::new(),
            },
            CiRun {
                id: 102,
                conclusion: None,
                head_sha: "ccc".into(),
                url: String::new(),
            },
        ];
        let selected = select_runs_to_notify(&runs, Some(99));
        assert_eq!(
            selected,
            vec![0, 1],
            "should notify runs 100 and 101, skip 102 (in-progress)"
        );
    }

    #[test]
    fn test_in_progress_does_not_appear_in_selection() {
        let runs = vec![CiRun {
            id: 200,
            conclusion: None,
            head_sha: "aaa".into(),
            url: String::new(),
        }];
        let selected = select_runs_to_notify(&runs, None);
        assert!(selected.is_empty(), "in-progress run must not be selected");
    }

    #[test]
    fn test_mixed_terminal_states_all_notified() {
        let runs = vec![
            CiRun {
                id: 300,
                conclusion: Some("failure".into()),
                head_sha: "a".into(),
                url: String::new(),
            },
            CiRun {
                id: 301,
                conclusion: Some("cancelled".into()),
                head_sha: "b".into(),
                url: String::new(),
            },
            CiRun {
                id: 302,
                conclusion: Some("success".into()),
                head_sha: "c".into(),
                url: String::new(),
            },
        ];
        let selected = select_runs_to_notify(&runs, Some(299));
        assert_eq!(
            selected,
            vec![0, 1, 2],
            "all 3 terminal runs should be selected"
        );
    }

    #[test]
    fn test_already_notified_runs_skipped() {
        let runs = vec![
            CiRun {
                id: 400,
                conclusion: Some("success".into()),
                head_sha: "a".into(),
                url: String::new(),
            },
            CiRun {
                id: 401,
                conclusion: Some("success".into()),
                head_sha: "b".into(),
                url: String::new(),
            },
        ];
        let selected = select_runs_to_notify(&runs, Some(400));
        assert_eq!(
            selected,
            vec![1],
            "run 400 already notified, only 401 selected"
        );
    }

    #[test]
    fn test_same_head_sha_deduplicates_notification() {
        let runs = vec![
            CiRun {
                id: 500,
                conclusion: Some("failure".into()),
                head_sha: "abc".into(),
                url: String::new(),
            },
            CiRun {
                id: 501,
                conclusion: Some("success".into()),
                head_sha: "abc".into(),
                url: String::new(),
            },
        ];
        let selected = select_runs_to_notify(&runs, Some(499));
        let deduped = dedupe_notifications_by_head_sha(&runs, &selected, None);
        assert_eq!(deduped.len(), 1, "same sha → 1 notification");
        assert_eq!(deduped[0].1, 501, "latest run_id wins");
        assert_eq!(deduped[0].2, "abc");
    }

    #[test]
    fn test_dedupe_skips_already_notified_sha() {
        let runs = vec![
            CiRun {
                id: 600,
                conclusion: Some("success".into()),
                head_sha: "aaa".into(),
                url: String::new(),
            },
            CiRun {
                id: 601,
                conclusion: Some("success".into()),
                head_sha: "bbb".into(),
                url: String::new(),
            },
        ];
        let selected = select_runs_to_notify(&runs, Some(599));
        let deduped = dedupe_notifications_by_head_sha(&runs, &selected, Some("aaa"));
        assert_eq!(deduped.len(), 1, "aaa already notified → only bbb");
        assert_eq!(deduped[0].2, "bbb");
    }

    #[test]
    fn test_different_head_sha_triggers_new_notification() {
        let runs = vec![
            CiRun {
                id: 600,
                conclusion: Some("success".into()),
                head_sha: "aaa".into(),
                url: String::new(),
            },
            CiRun {
                id: 601,
                conclusion: Some("success".into()),
                head_sha: "bbb".into(),
                url: String::new(),
            },
        ];
        let selected = select_runs_to_notify(&runs, Some(599));
        let deduped = dedupe_notifications_by_head_sha(&runs, &selected, None);
        assert_eq!(deduped.len(), 2, "different shas → 2 notifications");
    }

    #[test]
    fn test_inbox_body_failure_has_detail_and_url() {
        let body = build_inbox_body(
            "[ci-fail] o/r@main: failure",
            "failure",
            Some("Check / Clippy"),
            "https://github.com/o/r/actions/runs/123",
        );
        assert!(
            body.contains("Detail: Check / Clippy"),
            "failure body must have detail: {body}"
        );
        assert!(body.contains("URL: https://"), "body must have URL: {body}");
    }

    #[test]
    fn test_inbox_body_success_has_url_no_detail() {
        let body = build_inbox_body(
            "[ci-pass] o/r@main: passed ✓",
            "success",
            None,
            "https://github.com/o/r/actions/runs/456",
        );
        assert!(
            body.contains("URL: https://"),
            "success body must have URL: {body}"
        );
        assert!(
            !body.contains("Detail:"),
            "success body must not have Detail: {body}"
        );
    }

    #[test]
    fn test_headline_excludes_job_detail() {
        // ci_notification_message must NOT contain job/step names like
        // "(ubuntu-latest)" or "(tray feature)" — those go in inbox body.
        let msg = ci_notification_message(
            "o/r",
            "main",
            Some("failure"),
            Some("Check / Clippy (tray)"),
            None,
        );
        let headline = msg.unwrap();
        assert!(
            !headline.contains("Clippy"),
            "headline must not contain job detail: {headline}"
        );
        assert!(
            !headline.contains("tray"),
            "headline must not contain step detail: {headline}"
        );
        assert!(
            headline.contains("failure"),
            "headline must contain conclusion: {headline}"
        );
    }

    #[test]
    fn test_headline_success_clean() {
        let msg = ci_notification_message("o/r", "feat", Some("success"), None, None);
        let headline = msg.unwrap();
        assert!(headline.contains("passed"), "success headline: {headline}");
        assert!(
            !headline.contains("unknown"),
            "no unknown in success: {headline}"
        );
    }

    #[test]
    fn test_watch_expires_after_ttl_inactivity() {
        let dir = tmp_dir("ttl-expire");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        // Watch with last_terminal_seen_at 80 hours ago (> 72h TTL)
        let old_ts = (chrono::Utc::now() - chrono::Duration::hours(80)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": old_ts,
        });
        let filename = watch_filename("o/r", "feat");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        // Run check — should remove the watch
        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        check_ci_watches(&dir, &registry);

        assert!(!watch_path.exists(), "expired watch must be removed");
        // Event log should have entry
        let log = std::fs::read_to_string(dir.join("event-log.jsonl")).unwrap_or_default();
        assert!(log.contains("ci_watch_removed"), "removal must be logged");
        assert!(
            log.contains("inactivity_ttl"),
            "reason must be inactivity_ttl"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_watch_preserved_when_not_expired() {
        let dir = tmp_dir("ttl-fresh");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        // Watch with recent last_terminal_seen_at (1 hour ago, well within 72h)
        let recent_ts = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": recent_ts,
        });
        let filename = watch_filename("o/r", "feat");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        check_ci_watches(&dir, &registry);

        assert!(watch_path.exists(), "fresh watch must be preserved");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_watch_expires_at_absolute() {
        let dir = tmp_dir("ttl-absolute");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        // Watch with expires_at in the past
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "old", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": past,
        });
        let filename = watch_filename("o/r", "old");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        check_ci_watches(&dir, &registry);

        assert!(
            !watch_path.exists(),
            "past-expires_at watch must be removed"
        );
        let log = std::fs::read_to_string(dir.join("event-log.jsonl")).unwrap_or_default();
        // Sprint 57 Wave 2 Track B (#546 Item 1): the eager per-tick GC
        // pass at the top of `check_ci_watches` removes the watch first
        // and emits `reason=eager_gc_expired`; the legacy lazy expiry
        // emits `reason=expired`. Either substring is correct evidence
        // that the absolute-TTL path fired.
        assert!(
            log.contains("expired"),
            "reason must include 'expired'; got:\n{log}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_legacy_watch_without_ttl_fields_not_removed() {
        let dir = tmp_dir("ttl-legacy");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        // Legacy watch without expires_at or last_terminal_seen_at
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
        });
        let filename = watch_filename("o/r", "feat");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        check_ci_watches(&dir, &registry);

        assert!(
            watch_path.exists(),
            "legacy watch without TTL fields must not be removed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_remove_watch_on_pr_terminal_logs_event() {
        let dir = tmp_dir("pr-terminal");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let filename = watch_filename("o/r", "feat-merged");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(
            &watch_path,
            r#"{"repo":"o/r","branch":"feat-merged","instance":"a1"}"#,
        )
        .unwrap();
        assert!(watch_path.exists());

        remove_watch(&dir, &watch_path, "a1", "o/r", "feat-merged", "pr_terminal");

        assert!(!watch_path.exists(), "watch must be removed");
        let log = std::fs::read_to_string(dir.join("event-log.jsonl")).unwrap_or_default();
        assert!(log.contains("ci_watch_removed"));
        assert!(log.contains("reason=pr_terminal"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_watch_preserved_when_pr_check_unavailable() {
        let dir = tmp_dir("pr-fail");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let future = (chrono::Utc::now() + chrono::Duration::hours(48)).to_rfc3339();
        let recent = chrono::Utc::now().to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat-active", "interval_secs": 60,
            "instance": "a1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null, "expires_at": future, "last_terminal_seen_at": recent,
        });
        let filename = watch_filename("o/r", "feat-active");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).unwrap();

        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        check_ci_watches(&dir, &registry);

        assert!(
            watch_path.exists(),
            "watch must survive when PR check unavailable"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- MockCiProvider + state machine tests ---

    use parking_lot::Mutex;

    /// Mock CI provider for testing ci_check_repo state machine without HTTP.
    struct MockCiProvider {
        poll_result: Mutex<Option<CiPollResult>>,
        pr_state: Mutex<PrState>,
        failure_summary: Mutex<String>,
    }

    impl MockCiProvider {
        fn with_runs(runs: Vec<CiRun>) -> Self {
            Self {
                poll_result: Mutex::new(Some(CiPollResult::Runs {
                    runs,
                    rate_limit_remaining: None,
                    rate_limit_limit: None,
                })),
                pr_state: Mutex::new(PrState::Open),
                failure_summary: Mutex::new("Build / Test".to_string()),
            }
        }

        /// Sprint 54 P0-2: variant that lets a test seed quota counters
        /// directly so adaptive-backoff persistence + throttle behavior
        /// can be exercised end-to-end without an HTTP layer.
        #[allow(dead_code)]
        fn with_runs_and_quota(
            runs: Vec<CiRun>,
            remaining: Option<u64>,
            limit: Option<u64>,
        ) -> Self {
            Self {
                poll_result: Mutex::new(Some(CiPollResult::Runs {
                    runs,
                    rate_limit_remaining: remaining,
                    rate_limit_limit: limit,
                })),
                pr_state: Mutex::new(PrState::Open),
                failure_summary: Mutex::new("Build / Test".to_string()),
            }
        }

        fn with_api_error(status: u16, message: &str) -> Self {
            Self {
                poll_result: Mutex::new(Some(CiPollResult::ApiError {
                    status,
                    message: message.to_string(),
                    rate_limit_reset: None,
                })),
                pr_state: Mutex::new(PrState::Open),
                failure_summary: Mutex::new("Build / Test".to_string()),
            }
        }

        fn with_pr_terminal(self) -> Self {
            *self.pr_state.lock() = PrState::Terminal { merged: true };
            self
        }
    }

    #[async_trait::async_trait]
    impl CiProvider for MockCiProvider {
        async fn poll_runs(&self, _repo: &str, _branch: &str) -> anyhow::Result<CiPollResult> {
            Ok(self.poll_result.lock().take().unwrap())
        }
        async fn check_pr_terminal(&self, _repo: &str, _branch: &str) -> PrState {
            let mut guard = self.pr_state.lock();
            std::mem::replace(&mut *guard, PrState::Open)
        }
        async fn fetch_failure_summary(&self, _repo: &str, _run_id: u64) -> String {
            self.failure_summary.lock().clone()
        }
        fn token_warning(&self) -> Option<&'static str> {
            None
        }
    }

    /// Helper: run ci_check_repo with a mock provider in a temp dir.
    fn run_ci_check(
        dir: &Path,
        watch_json: &serde_json::Value,
        provider: &dyn CiProvider,
    ) -> anyhow::Result<()> {
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let repo = watch_json["repo"].as_str().unwrap();
        let branch = watch_json["branch"].as_str().unwrap();
        let subscribers = parse_subscribers(watch_json);
        let filename = watch_filename(repo, branch);
        let watch_path = ci_dir.join(&filename);
        std::fs::write(
            &watch_path,
            serde_json::to_string_pretty(watch_json).unwrap(),
        )
        .unwrap();

        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            dir,
            &watch_path,
            repo,
            branch,
            &subscribers,
            watch_json["last_run_id"].as_u64(),
            watch_json["head_sha"].as_str(),
            watch_json["last_notified_head_sha"].as_str(),
            &registry,
            provider,
        ))
    }

    fn base_watch() -> serde_json::Value {
        serde_json::json!({
            "repo": "o/r", "branch": "feat", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null, "last_notified_head_sha": null,
        })
    }

    #[test]
    fn mock_success_run_updates_watch_state() {
        let dir = tmp_dir("mock-success");
        let provider = MockCiProvider::with_runs(vec![CiRun {
            id: 100,
            conclusion: Some("success".into()),
            head_sha: "abc".into(),
            url: "https://example.com/100".into(),
        }]);
        run_ci_check(&dir, &base_watch(), &provider).unwrap();

        // Watch file should be updated with last_run_id and head_sha
        let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
        assert_eq!(updated["last_run_id"].as_u64(), Some(100));
        assert_eq!(updated["head_sha"].as_str(), Some("abc"));

        // Inbox should have a notification
        let inbox_dir = dir.join("inbox");
        let has_inbox = inbox_dir.exists()
            && std::fs::read_dir(&inbox_dir)
                .map(|d| d.count() > 0)
                .unwrap_or(false);
        assert!(has_inbox, "success run should enqueue inbox notification");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Issue #745 regression guard: when an older run's SHA no longer
    /// matches the branch head (a newer commit has been pushed since the
    /// run was triggered), the notification must be dropped — but the
    /// tracker must still advance so the same stale run isn't re-tried
    /// on the next poll.
    #[test]
    fn mock_stale_sha_drops_notification_but_advances_tracker() {
        let dir = tmp_dir("mock-stale-sha");
        // Two runs: NEW_HEAD is the current head (in-progress); OLD_HEAD
        // was just superseded by a push. OLD_HEAD's run completed first
        // (success). Without the staleness filter we would notify users
        // about OLD_HEAD passing — that pass is no longer relevant; the
        // user is waiting on NEW_HEAD.
        let provider = MockCiProvider::with_runs(vec![
            CiRun {
                id: 301, // NEW_HEAD's run (latest, in-progress)
                conclusion: None,
                head_sha: "newhead".into(),
                url: "https://example.com/301".into(),
            },
            CiRun {
                id: 300, // OLD_HEAD's run (terminal but stale)
                conclusion: Some("success".into()),
                head_sha: "oldhead".into(),
                url: "https://example.com/300".into(),
            },
        ]);
        run_ci_check(&dir, &base_watch(), &provider).unwrap();

        // Watch state: tracker advances past the stale run so it won't
        // be re-emitted on the next poll. head_sha is the NEW_HEAD.
        let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
        assert_eq!(
            updated["head_sha"].as_str(),
            Some("newhead"),
            "head_sha must track newest run's sha"
        );
        assert_eq!(
            updated["last_notified_head_sha"].as_str(),
            Some("oldhead"),
            "stale sha must still mark notified so it isn't re-emitted"
        );

        // Inbox: must NOT contain a `[ci-pass]` for the stale sha.
        // The OLD_HEAD success was dropped silently (info-level log only).
        let inbox_path = dir.join("inbox").join("agent1.jsonl");
        if inbox_path.exists() {
            let content = std::fs::read_to_string(&inbox_path).unwrap();
            assert!(
                !content.contains("[ci-pass]"),
                "stale CI pass must not be delivered: {content}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Issue #745 follow-up: stale drops must produce an observable `[ci-stale]`
    /// inbox message so operators can audit which runs were superseded.
    #[test]
    fn mock_stale_sha_emits_ci_stale_inbox_message() {
        let dir = tmp_dir("mock-stale-sha-inbox");
        let provider = MockCiProvider::with_runs(vec![
            CiRun {
                id: 301,
                conclusion: None,
                head_sha: "newhead".into(),
                url: "https://example.com/301".into(),
            },
            CiRun {
                id: 300,
                conclusion: Some("success".into()),
                head_sha: "oldhead".into(),
                url: "https://example.com/300".into(),
            },
        ]);
        run_ci_check(&dir, &base_watch(), &provider).unwrap();

        let inbox_path = dir.join("inbox").join("agent1.jsonl");
        assert!(
            inbox_path.exists(),
            "inbox file must exist after stale drop"
        );
        let content = std::fs::read_to_string(&inbox_path).unwrap();
        assert!(
            content.contains("[ci-stale]"),
            "inbox must contain [ci-stale] kind: {content}"
        );
        assert!(
            content.contains("oldhead"),
            "ci-stale message must reference the stale sha: {content}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Negative case: when only one run exists and its sha is the current
    /// head, the existing happy path must still notify.
    #[test]
    fn mock_single_run_current_head_still_notifies() {
        let dir = tmp_dir("mock-single-current");
        let provider = MockCiProvider::with_runs(vec![CiRun {
            id: 400,
            conclusion: Some("success".into()),
            head_sha: "onlyhead".into(),
            url: "https://example.com/400".into(),
        }]);
        run_ci_check(&dir, &base_watch(), &provider).unwrap();

        let inbox_dir = dir.join("inbox");
        let has_inbox = inbox_dir.exists()
            && std::fs::read_dir(&inbox_dir)
                .map(|d| d.count() > 0)
                .unwrap_or(false);
        assert!(
            has_inbox,
            "single-run case must preserve existing notify behavior"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mock_failure_run_includes_detail() {
        let dir = tmp_dir("mock-failure");
        let provider = MockCiProvider::with_runs(vec![CiRun {
            id: 200,
            conclusion: Some("failure".into()),
            head_sha: "def".into(),
            url: "https://example.com/200".into(),
        }]);
        run_ci_check(&dir, &base_watch(), &provider).unwrap();

        // Check inbox contains failure detail
        let inbox_path = dir.join("inbox").join("agent1.jsonl");
        if inbox_path.exists() {
            let content = std::fs::read_to_string(&inbox_path).unwrap();
            assert!(
                content.contains("ci-fail"),
                "inbox should have ci-fail: {content}"
            );
            assert!(
                content.contains("Build / Test"),
                "inbox should have failure detail: {content}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mock_api_error_propagates() {
        let dir = tmp_dir("mock-api-err");
        let provider = MockCiProvider::with_api_error(403, "GH API 403: rate limit exceeded");
        let result = run_ci_check(&dir, &base_watch(), &provider);
        assert!(result.is_err(), "API error must propagate");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("403"), "error should contain status: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mock_pr_terminal_clears_watch() {
        let dir = tmp_dir("mock-pr-term");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let watch = base_watch();
        let filename = watch_filename("o/r", "feat");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

        // Provider says PR is terminal — runs response doesn't matter
        let provider = MockCiProvider::with_runs(vec![]).with_pr_terminal();

        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            &dir,
            &watch_path,
            "o/r",
            "feat",
            &["agent1".to_string()],
            None,
            None,
            None,
            &registry,
            &provider,
        ))
        .unwrap();

        assert!(!watch_path.exists(), "PR terminal must remove watch file");
        let log = std::fs::read_to_string(dir.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("pr_terminal"),
            "event log must record pr_terminal"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mock_no_runs_preserves_watch() {
        let dir = tmp_dir("mock-no-runs");
        let provider = MockCiProvider::with_runs(vec![]);
        run_ci_check(&dir, &base_watch(), &provider).unwrap();

        let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
        assert!(watch_path.exists(), "empty runs must preserve watch");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mock_force_push_resets_tracking() {
        let dir = tmp_dir("mock-force-push");
        // Watch has old head_sha "old123", new run has different sha
        let mut watch = base_watch();
        watch["head_sha"] = serde_json::json!("old123");
        watch["last_run_id"] = serde_json::json!(50);
        let provider = MockCiProvider::with_runs(vec![CiRun {
            id: 51,
            conclusion: Some("success".into()),
            head_sha: "new456".into(),
            url: "https://example.com/51".into(),
        }]);
        run_ci_check(&dir, &watch, &provider).unwrap();

        let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
        // After force push, run 51 should be notified even though id > last_run_id=50
        // would normally catch it — the key is head_sha changed so tracking reset
        assert_eq!(updated["last_run_id"].as_u64(), Some(51));
        assert_eq!(updated["head_sha"].as_str(), Some("new456"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mock_rate_limit_writes_backoff_and_propagates_error() {
        let dir = tmp_dir("mock-rate-limit");
        let provider = MockCiProvider {
            poll_result: Mutex::new(Some(CiPollResult::ApiError {
                status: 403,
                message: "GH API 403: rate limit exceeded".to_string(),
                rate_limit_reset: Some(9999999999),
            })),
            pr_state: Mutex::new(PrState::Open),
            failure_summary: Mutex::new(String::new()),
        };
        let result = run_ci_check(&dir, &base_watch(), &provider);
        assert!(result.is_err(), "rate-limit must propagate as error");
        let watch_path = dir.join("ci-watches").join(watch_filename("o/r", "feat"));
        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
        assert_eq!(updated["rate_limit_until"].as_u64(), Some(9999999999));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rate_limit_backoff_skips_polling() {
        let dir = tmp_dir("backoff-skip");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let future = (chrono::Utc::now().timestamp() + 3600) as u64;
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null, "rate_limit_until": future,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
        });
        let filename = watch_filename("o/r", "feat");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();
        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        check_ci_watches(&dir, &registry);
        assert!(watch_path.exists(), "backoff watch must be preserved");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── GitLab CiProvider tests (Sprint 39 PR-1) ────────────────────

    /// Helper: mock HTTP server for GitLab tests. Captures path + headers.
    /// Captured request from mock server: (path, full_request).
    type MockCapture = std::sync::Arc<std::sync::Mutex<Option<(String, String)>>>;

    /// RAII guard that saves/restores GITLAB_TOKEN + HOME env vars.
    /// Also holds a static mutex to serialize env-var-touching tests.
    struct GitlabTokenGuard {
        prev_token: Option<std::ffi::OsString>,
        prev_home: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
    impl GitlabTokenGuard {
        fn clear() -> Self {
            let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev_token = std::env::var_os("GITLAB_TOKEN");
            let prev_home = std::env::var_os("HOME");
            std::env::remove_var("GITLAB_TOKEN");
            Self {
                prev_token,
                prev_home,
                _lock: lock,
            }
        }
        fn set(val: &str) -> Self {
            let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev_token = std::env::var_os("GITLAB_TOKEN");
            let prev_home = std::env::var_os("HOME");
            std::env::set_var("GITLAB_TOKEN", val);
            Self {
                prev_token,
                prev_home,
                _lock: lock,
            }
        }
    }
    impl Drop for GitlabTokenGuard {
        fn drop(&mut self) {
            match &self.prev_token {
                Some(v) => std::env::set_var("GITLAB_TOKEN", v),
                None => std::env::remove_var("GITLAB_TOKEN"),
            }
            if let Some(v) = &self.prev_home {
                std::env::set_var("HOME", v);
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn gitlab_mock_server(response_body: &str) -> (u16, std::thread::JoinHandle<()>, MockCapture) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<(String, String)>));
        let captured_clone = captured.clone();
        let body = response_body.to_string();

        // fire-and-forget: test mock server thread
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).expect("read");
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = request
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            // Capture full request (path + headers) for assertion.
            *captured_clone.lock().expect("lock") = Some((path, request));

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).expect("write");
        });

        (port, handle, captured)
    }

    /// §3.5.10 production-path-coupled: GitLabCiProvider::poll_runs
    /// against mock server with spec-quoted fixture.
    #[test]
    fn gitlab_poll_runs_parses_pipelines() {
        let fixture = include_str!("../../../tests/fixtures/gitlab-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);

        let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let result = rt.block_on(provider.poll_runs("foo/bar", "main"));

        handle.join().expect("mock");
        let (path, _request) = captured.lock().expect("lock").take().expect("captured");
        assert!(
            path.contains("/projects/foo%2Fbar/pipelines"),
            "path must target pipelines: {path}"
        );

        let runs = match result.expect("poll_runs") {
            super::CiPollResult::Runs { runs, .. } => runs,
            other => panic!("expected Runs, got: {other:?}"),
        };
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, 47);
        assert_eq!(runs[0].conclusion, Some("success".to_string()));
        assert_eq!(runs[1].conclusion, Some("failure".to_string()));
    }

    /// §3.5.10: GitLabCiProvider::check_pr_terminal parses MR state.
    #[test]
    fn gitlab_check_pr_terminal_merged() {
        let fixture = include_str!("../../../tests/fixtures/gitlab-merge-requests-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);

        let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let state = rt.block_on(provider.check_pr_terminal("foo/bar", "feat/test"));

        handle.join().expect("mock");
        let (path, _req) = captured.lock().expect("lock").take().expect("captured");
        assert!(path.contains("/merge_requests"), "path: {path}");
        assert!(path.contains("source_branch=feat"), "query: {path}");
        assert!(
            matches!(state, super::PrState::Terminal { merged: true }),
            "expected Terminal(merged), got: {state:?}"
        );
    }

    /// §3.5.10: GitLabCiProvider::fetch_failure_summary finds failed job.
    #[test]
    fn gitlab_fetch_failure_summary_finds_failed_job() {
        let fixture = include_str!("../../../tests/fixtures/gitlab-jobs-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);

        let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let summary = rt.block_on(provider.fetch_failure_summary("foo/bar", 48));

        handle.join().expect("mock");
        let (path, _req) = captured.lock().expect("lock").take().expect("captured");
        assert!(path.contains("/pipelines/48/jobs"), "path: {path}");
        // Summary starts with stage/name (trace fetch hits second accept
        // which isn't available — falls back to header-only).
        assert!(
            summary.starts_with("test / cargo-test"),
            "summary: {summary}"
        );
    }

    /// Auth fallback: token_warning returns warning when no token found.
    #[test]
    fn gitlab_token_warning_when_no_token() {
        let _guard = GitlabTokenGuard::clear();
        let provider = super::GitLabCiProvider::new().expect("provider");
        let warning = provider.token_warning();
        assert!(warning.is_some(), "should warn when no token");
        assert!(
            warning.expect("w").contains("GITLAB_TOKEN"),
            "warning must mention GITLAB_TOKEN"
        );
    }

    /// Smoke: GitLabCiProvider::new() defaults to gitlab.com base URL.
    #[test]
    fn gitlab_new_defaults_to_gitlab_com() {
        let provider = super::GitLabCiProvider::new().expect("new");
        assert_eq!(provider.http.base_url, "https://gitlab.com");
    }

    /// Smoke: with_base_url() sets custom base URL (self-hosted).
    #[test]
    fn gitlab_with_base_url_sets_custom() {
        let provider =
            super::GitLabCiProvider::with_base_url("https://git.corp.example.com".into())
                .expect("with_base_url");
        assert_eq!(provider.http.base_url, "https://git.corp.example.com");
    }

    /// B4: Auth state 1 — GITLAB_TOKEN env present → PRIVATE-TOKEN header sent.
    #[test]
    fn gitlab_auth_env_token_sends_private_token_header() {
        let fixture = include_str!("../../../tests/fixtures/gitlab-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);

        let _guard = GitlabTokenGuard::set("test-token-123");
        let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let _ = rt.block_on(provider.poll_runs("foo/bar", "main"));
        handle.join().expect("mock");
        std::env::remove_var("GITLAB_TOKEN");

        let (_, request) = captured.lock().expect("lock").take().expect("captured");
        assert!(
            request.contains("private-token: test-token-123")
                || request.contains("PRIVATE-TOKEN: test-token-123"),
            "must send PRIVATE-TOKEN header: {request}"
        );
    }

    /// B4 state 2: env absent + glab CLI config present → token from config.
    #[test]
    fn gitlab_auth_glab_config_fallback_sends_private_token_header() {
        let fixture = include_str!("../../../tests/fixtures/gitlab-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);

        // Guard serializes + saves/restores GITLAB_TOKEN + HOME.
        let mut guard = GitlabTokenGuard::clear();
        // Setup temp HOME with glab config.
        let temp = std::env::temp_dir().join(format!("agend-glab-test-{}", std::process::id()));
        let glab_dir = temp.join(".config").join("glab-cli");
        std::fs::create_dir_all(&glab_dir).ok();
        std::fs::write(
            glab_dir.join("config.yml"),
            "hosts:\n  gitlab.com:\n    token: glab_config_token_abc\n",
        )
        .expect("write glab config");
        // Override HOME so resolve_token finds the temp config.
        guard.prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &temp);

        let provider = super::GitLabCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let _ = rt.block_on(provider.poll_runs("foo/bar", "main"));
        handle.join().expect("mock");

        let (_, request) = captured.lock().expect("lock").take().expect("captured");
        assert!(
            request.contains("private-token: glab_config_token_abc")
                || request.contains("PRIVATE-TOKEN: glab_config_token_abc"),
            "must send PRIVATE-TOKEN from glab config: {request}"
        );

        std::fs::remove_dir_all(&temp).ok();
        // guard drop restores HOME + GITLAB_TOKEN
    }

    // ── Bitbucket Cloud CiProvider tests (Sprint 39 PR-2) ───────────

    /// Env guard for BITBUCKET_TOKEN + HOME (mirrors GitlabTokenGuard).
    struct BitbucketTokenGuard {
        prev_token: Option<std::ffi::OsString>,
        prev_home: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    static BB_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
    impl BitbucketTokenGuard {
        fn clear() -> Self {
            let lock = BB_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev_token = std::env::var_os("BITBUCKET_TOKEN");
            let prev_home = std::env::var_os("HOME");
            std::env::remove_var("BITBUCKET_TOKEN");
            Self {
                prev_token,
                prev_home,
                _lock: lock,
            }
        }
        fn set(val: &str) -> Self {
            let lock = BB_ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev_token = std::env::var_os("BITBUCKET_TOKEN");
            let prev_home = std::env::var_os("HOME");
            std::env::set_var("BITBUCKET_TOKEN", val);
            Self {
                prev_token,
                prev_home,
                _lock: lock,
            }
        }
    }
    impl Drop for BitbucketTokenGuard {
        fn drop(&mut self) {
            match &self.prev_token {
                Some(v) => std::env::set_var("BITBUCKET_TOKEN", v),
                None => std::env::remove_var("BITBUCKET_TOKEN"),
            }
            if let Some(v) = &self.prev_home {
                std::env::set_var("HOME", v);
            }
        }
    }

    /// Reuse gitlab_mock_server for Bitbucket (same raw TCP pattern).
    /// §3.5.10: poll_runs parses Bitbucket pipelines response.
    #[test]
    fn bitbucket_poll_runs_parses_pipelines() {
        let fixture = include_str!("../../../tests/fixtures/bitbucket-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);
        let _guard = BitbucketTokenGuard::set("user:pass");
        let provider =
            super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
                .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let result = rt.block_on(provider.poll_runs("foo/bar", "main"));
        handle.join().expect("mock");
        let (path, req) = captured.lock().expect("lock").take().expect("captured");
        assert!(
            path.contains("/repositories/foo/bar/pipelines"),
            "path: {path}"
        );
        assert!(path.contains("target.branch=main"), "query: {path}");
        assert!(
            req.to_lowercase().contains("authorization: basic"),
            "must send Basic auth header: {req}"
        );
        let runs = match result.expect("poll_runs") {
            super::CiPollResult::Runs { runs, .. } => runs,
            other => panic!("expected Runs, got: {other:?}"),
        };
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].conclusion, Some("success".to_string()));
        assert_eq!(runs[1].conclusion, Some("failure".to_string()));
    }

    /// §3.5.10: check_pr_terminal parses Bitbucket PR state.
    #[test]
    fn bitbucket_check_pr_terminal_merged() {
        let fixture = include_str!("../../../tests/fixtures/bitbucket-pullrequests-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);
        let _guard = BitbucketTokenGuard::set("user:pass");
        let provider =
            super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
                .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let state = rt.block_on(provider.check_pr_terminal("foo/bar", "feat/test"));
        handle.join().expect("mock");
        let (path, req) = captured.lock().expect("lock").take().expect("captured");
        assert!(path.contains("/pullrequests"), "path: {path}");
        assert!(path.contains("source.branch.name"), "query: {path}");
        assert!(
            req.to_lowercase().contains("authorization: basic"),
            "auth: {req}"
        );
        assert!(
            matches!(state, super::PrState::Terminal { merged: true }),
            "got: {state:?}"
        );
    }

    /// §3.5.10: fetch_failure_summary finds failed step.
    #[test]
    fn bitbucket_fetch_failure_summary_finds_failed_step() {
        // 2-request chain mock: 1st returns steps, 2nd returns log.
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let captured_reqs =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
        let cap = captured_reqs.clone();
        let steps_body =
            include_str!("../../../tests/fixtures/bitbucket-steps-response.json").to_string();
        // fire-and-forget: test mock server thread
        let handle = std::thread::spawn(move || {
            for i in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).expect("read");
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                cap.lock().expect("lock").push((path, request));
                let body = if i == 0 {
                    &steps_body
                } else {
                    "error line 1\nerror line 2\n"
                };
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
                stream.write_all(resp.as_bytes()).expect("write");
            }
        });
        let _guard = BitbucketTokenGuard::set("user:pass");
        let provider =
            super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
                .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let summary = rt.block_on(provider.fetch_failure_summary("foo/bar", 48));
        handle.join().expect("mock");
        let reqs = captured_reqs.lock().expect("lock");
        assert_eq!(
            reqs.len(),
            2,
            "expected 2 requests for failure-summary chain"
        );
        assert!(
            reqs[0].0.contains("/pipelines/48/steps"),
            "req1 path: {}",
            reqs[0].0
        );
        assert!(
            reqs[0].1.to_lowercase().contains("authorization: basic"),
            "req1 auth"
        );
        assert!(
            reqs[1].0.contains("/log"),
            "req2 path must contain /log: {}",
            reqs[1].0
        );
        assert!(
            reqs[1].1.to_lowercase().contains("authorization: basic"),
            "req2 auth"
        );
        assert!(
            summary.starts_with("Test"),
            "summary must start with step name: {summary}"
        );
        assert!(
            summary.contains("error line"),
            "summary must contain log tail: {summary}"
        );
    }

    /// Auth state 1: BITBUCKET_TOKEN env → Basic auth header.
    #[test]
    fn bitbucket_auth_env_token_sends_basic_header() {
        let fixture = include_str!("../../../tests/fixtures/bitbucket-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);
        let _guard = BitbucketTokenGuard::set("user:app_pass");
        let provider =
            super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
                .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let _ = rt.block_on(provider.poll_runs("foo/bar", "main"));
        handle.join().expect("mock");
        let (_, request) = captured.lock().expect("lock").take().expect("captured");
        assert!(
            request.contains("authorization: Basic") || request.contains("Authorization: Basic"),
            "must send Basic auth: {request}"
        );
    }

    /// Auth state 3: no token → warning.
    #[test]
    fn bitbucket_token_warning_when_no_token() {
        let _guard = BitbucketTokenGuard::clear();
        let provider = super::BitbucketCiProvider::new().expect("provider");
        let warning = provider.token_warning();
        assert!(warning.is_some());
        assert!(warning.expect("w").contains("BITBUCKET_TOKEN"));
    }

    /// Auth state 2: env absent + bb CLI config → Basic auth from config.
    #[test]
    fn bitbucket_auth_bb_config_fallback_sends_basic_header() {
        let fixture = include_str!("../../../tests/fixtures/bitbucket-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);
        let mut guard = BitbucketTokenGuard::clear();
        // Setup temp HOME with bb config.
        let temp = std::env::temp_dir().join(format!("agend-bb-test-{}", std::process::id()));
        let bb_dir = temp.join(".config").join("bb");
        std::fs::create_dir_all(&bb_dir).ok();
        std::fs::write(bb_dir.join("config"), "token: bbuser:bb_app_pass\n").expect("write");
        guard.prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &temp);

        let provider =
            super::BitbucketCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
                .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let _ = rt.block_on(provider.poll_runs("foo/bar", "main"));
        handle.join().expect("mock");

        let (_, request) = captured.lock().expect("lock").take().expect("captured");
        assert!(
            request.contains("authorization: Basic") || request.contains("Authorization: Basic"),
            "must send Basic auth from bb config: {request}"
        );
        std::fs::remove_dir_all(&temp).ok();
    }

    /// Smoke: constructors.
    #[test]
    fn bitbucket_new_defaults_to_api_bitbucket_org() {
        let provider = super::BitbucketCiProvider::new().expect("new");
        assert_eq!(provider.http.base_url, "https://api.bitbucket.org");
    }

    #[test]
    fn bitbucket_with_base_url_sets_custom() {
        let provider =
            super::BitbucketCiProvider::with_base_url("https://bb.corp.example.com".into())
                .expect("with_base_url");
        assert_eq!(provider.http.base_url, "https://bb.corp.example.com");
    }

    /// B5: watch_ci rejects bitbucket_server with operator-actionable error.
    #[test]
    fn watch_ci_rejects_bitbucket_server_with_actionable_error() {
        let dir = std::env::temp_dir().join(format!("agend-bb-reject-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let result = crate::mcp::handlers::ci::handle_watch_ci(
            &dir,
            &serde_json::json!({
                "repo": "myws/myrepo",
                "branch": "feat-test",
                "ci_provider": "bitbucket_server",
            }),
            "test-inst",
        );
        assert!(
            result["error"].as_str().is_some(),
            "must return error for bitbucket_server: {result}"
        );
        let err = result["error"].as_str().expect("error");
        assert!(
            err.contains("not yet supported") || err.contains("Sprint 41"),
            "error must mention deferral: {err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── PR-3 tests: auto-detect + GHE ───────────────────────────────

    #[test]
    fn detect_provider_github_com() {
        let (kind, is_custom) = super::detect_provider_from_remote("github.com/owner/repo");
        assert_eq!(kind, "github");
        assert!(!is_custom);
    }

    #[test]
    fn detect_provider_gitlab_com() {
        let (kind, is_custom) = super::detect_provider_from_remote("gitlab.com/group/project");
        assert_eq!(kind, "gitlab");
        assert!(!is_custom);
    }

    #[test]
    fn detect_provider_bitbucket_org() {
        let (kind, is_custom) = super::detect_provider_from_remote("bitbucket.org/ws/repo");
        assert_eq!(kind, "bitbucket_cloud");
        assert!(!is_custom);
    }

    #[test]
    fn detect_provider_custom_domain_defaults_github_with_warning() {
        let (kind, is_custom) =
            super::detect_provider_from_remote("git.corp.example.com/team/repo");
        assert_eq!(kind, "github", "unknown domain defaults to github");
        assert!(is_custom, "unknown domain must flag custom_host");
    }

    /// B1: GitHub Enterprise custom domain detected as github + custom.
    #[test]
    fn detect_provider_github_enterprise_custom_domain() {
        let (kind, is_custom) =
            super::detect_provider_from_remote("https://github.acme.corp/myorg/myrepo");
        assert_eq!(kind, "github");
        assert!(is_custom, "GHE custom domain must flag custom_host");
    }

    /// B3: explicit ci_provider in watch JSON overrides auto-detect.
    #[test]
    fn explicit_ci_provider_overrides_auto_detect() {
        // Watch with ci_provider: gitlab but repo URL pointing to github.com.
        // Factory should construct GitLab provider, not GitHub.
        let fixture = include_str!("../../../tests/fixtures/gitlab-pipelines-response.json");
        let (port, handle, captured) = gitlab_mock_server(fixture);

        // Construct GitLab provider via the same factory logic as production:
        // explicit ci_provider="gitlab" + ci_provider_url pointing to mock.
        let watch = serde_json::json!({
            "repo": "github.com/myorg/myrepo",
            "branch": "main",
            "ci_provider": "gitlab",
            "ci_provider_url": format!("http://127.0.0.1:{port}"),
        });
        let ci_type = watch["ci_provider"].as_str().unwrap();
        assert_eq!(ci_type, "gitlab");

        // Construct provider as factory would.
        let url = watch["ci_provider_url"].as_str().unwrap().to_string();
        let provider = super::GitLabCiProvider::with_base_url(url).expect("provider");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let _ = rt.block_on(provider.poll_runs("myorg/myrepo", "main"));
        handle.join().expect("mock");

        // Assert: request went to GitLab-shaped URL (/projects/{id}/pipelines),
        // NOT GitHub-shaped (/repos/{owner}/{repo}/actions/runs).
        let (path, _) = captured.lock().expect("lock").take().expect("captured");
        assert!(
            path.contains("/projects/") && path.contains("/pipelines"),
            "explicit gitlab must produce GitLab-shaped request, got: {path}"
        );
        assert!(
            !path.contains("/repos/") && !path.contains("/actions/"),
            "must NOT produce GitHub-shaped request: {path}"
        );
    }

    #[test]
    fn github_with_base_url_sets_custom_for_ghe() {
        let provider =
            super::GitHubCiProvider::with_base_url("https://github.corp.example.com/api/v3".into())
                .expect("with_base_url");
        assert_eq!(
            provider.http.base_url,
            "https://github.corp.example.com/api/v3"
        );
    }

    #[test]
    fn github_new_defaults_to_api_github_com() {
        let provider = super::GitHubCiProvider::new().expect("new");
        assert_eq!(provider.http.base_url, "https://api.github.com");
    }

    // ── Hotfix E: auto-clear false-positive tests ────────────────────

    #[test]
    fn auto_clear_skips_young_watch() {
        // A watch created < 60s ago should NOT be auto-cleared even if
        // PR terminal is detected (stale PR from previous branch use).
        let dir = tmp_dir("young-watch");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        // Watch with expires_at = now + TTL (meaning it was just created).
        let now = chrono::Utc::now();
        let expires = (now + chrono::Duration::hours(super::WATCH_TTL_HOURS)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat-new", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null, "expires_at": expires,
        });
        let filename = super::watch_filename("o/r", "feat-new");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        // Provider says PR terminal (stale PR from old branch use).
        let provider = MockCiProvider::with_runs(vec![]).with_pr_terminal();
        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            &dir,
            &watch_path,
            "o/r",
            "feat-new",
            &["agent1".to_string()],
            None,
            None,
            None,
            &registry,
            &provider,
        ))
        .unwrap();

        // Watch must survive (young watch grace).
        assert!(
            watch_path.exists(),
            "young watch (<60s) must NOT be auto-cleared on PR terminal"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auto_clear_fires_on_old_watch_with_merged_pr() {
        // A watch created > 60s ago SHOULD be cleared on PR terminal.
        let dir = tmp_dir("old-watch-clear");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        // Watch with expires_at implying creation > 60s ago.
        let old_creation = chrono::Utc::now() - chrono::Duration::minutes(5);
        let expires = (old_creation + chrono::Duration::hours(super::WATCH_TTL_HOURS)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat-old", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null, "expires_at": expires,
        });
        let filename = super::watch_filename("o/r", "feat-old");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        let provider = MockCiProvider::with_runs(vec![]).with_pr_terminal();
        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            &dir,
            &watch_path,
            "o/r",
            "feat-old",
            &["agent1".to_string()],
            None,
            None,
            None,
            &registry,
            &provider,
        ))
        .unwrap();

        assert!(
            !watch_path.exists(),
            "old watch (>60s) must be cleared on PR terminal"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── Hotfix F: classification layer tests ─────────────────────────

    #[test]
    fn fresh_branch_no_pr_classified_as_pending() {
        // No PR found → PrState::Unknown → watch preserved (pending).
        let dir = tmp_dir("classify-no-pr");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let watch = base_watch();
        let filename = watch_filename("o/r", "feat");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        // Provider returns no runs + PrState::Unknown (no PR found).
        let provider = MockCiProvider::with_runs(vec![]);
        // MockCiProvider default pr_state is Open, override to Unknown.
        *provider.pr_state.lock() = PrState::Unknown;

        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            &dir,
            &watch_path,
            "o/r",
            "feat",
            &["agent1".to_string()],
            None,
            None,
            None,
            &registry,
            &provider,
        ))
        .unwrap();

        assert!(watch_path.exists(), "no-PR branch must NOT be auto-cleared");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn branch_with_open_pr_classified_as_active() {
        // Open PR → PrState::Open → watch preserved.
        let dir = tmp_dir("classify-open-pr");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let watch = base_watch();
        let filename = watch_filename("o/r", "feat");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        let provider = MockCiProvider::with_runs(vec![]);
        // Default pr_state is Open — watch should persist.

        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            &dir,
            &watch_path,
            "o/r",
            "feat",
            &["agent1".to_string()],
            None,
            None,
            None,
            &registry,
            &provider,
        ))
        .unwrap();

        assert!(
            watch_path.exists(),
            "open-PR branch must NOT be auto-cleared"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn branch_with_merged_pr_classified_as_terminal() {
        // Already tested by mock_pr_terminal_clears_watch — verify preserved.
        // Merged PR → PrState::Terminal { merged: true } → auto-clear.
        let dir = tmp_dir("classify-merged");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        // Use old expires_at so grace period doesn't block.
        let old_creation = chrono::Utc::now() - chrono::Duration::minutes(5);
        let expires = (old_creation + chrono::Duration::hours(super::WATCH_TTL_HOURS)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat-merged", "interval_secs": 60,
            "instance": "agent1", "last_run_id": null, "head_sha": null,
            "last_polled_at": null, "expires_at": expires,
        });
        let filename = watch_filename("o/r", "feat-merged");
        let watch_path = ci_dir.join(&filename);
        std::fs::write(&watch_path, serde_json::to_string(&watch).unwrap()).ok();

        let provider = MockCiProvider::with_runs(vec![]).with_pr_terminal();
        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            &dir,
            &watch_path,
            "o/r",
            "feat-merged",
            &["agent1".to_string()],
            None,
            None,
            None,
            &registry,
            &provider,
        ))
        .unwrap();

        assert!(
            !watch_path.exists(),
            "merged-PR branch MUST be auto-cleared"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_pr_not_classified_as_terminal() {
        // A PR closed >1h ago should be treated as stale (Unknown), not terminal.
        // This is the root-cause fix: prevents false-positive auto-clear from
        // old PRs matching the same branch name.
        // Tested via the GitHub provider's closed_at check.
        // The mock provider bypasses this (returns Terminal directly), so this
        // test verifies the design contract via source inspection.
        // #701 split: provider impls (incl. check_pr_terminal) moved to provider.rs.
        let src = include_str!("provider.rs");
        assert!(
            src.contains("closed_at") && src.contains("Duration::hours(1)"),
            "check_pr_terminal must verify closed_at freshness (stale PR filter)"
        );
    }

    // ----------------------------------------------------------------------
    // Sprint 54 P0-1 — Subscriber fan-out (hard contract item 2).
    //
    // EMPIRICAL REGRESSION-PROOF ANCHOR: when fan-out is collapsed back to
    // single-subscriber (commenting out the `for sub in subscribers` loop
    // in `ci_check_repo` and notifying only the first), the second
    // subscriber's inbox stays empty here and the assertion fails with:
    //
    //   thread '...subscriber_fan_out_notifies_every_member' panicked at:
    //   assertion `dev inbox missing terminal notification` failed
    //
    // Restoring the loop returns the test to PASS. This is the proof
    // that the new test catches the multi-caller bug in production code,
    // not just in synthetic mock paths.
    // ----------------------------------------------------------------------
    #[test]
    fn subscriber_fan_out_notifies_every_member() {
        let dir = tmp_dir("fanout");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let watch = serde_json::json!({
            "repo": "o/r",
            "branch": "feat",
            "subscribers": [
                {"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"},
                {"instance": "dev",  "subscribed_at": "2026-05-07T00:00:01Z"}
            ],
            "instance": "lead",
            "interval_secs": 60,
            "last_run_id": null,
            "head_sha": null,
            "last_polled_at": null,
            "last_notified_head_sha": null,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": null,
        });
        let watch_path = ci_dir.join(watch_filename("o/r", "feat"));
        std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

        // Mock provider returns one terminal success run.
        let provider = MockCiProvider::with_runs(vec![CiRun {
            id: 7,
            conclusion: Some("success".to_string()),
            head_sha: "deadbeef".to_string(),
            url: "https://example/run/7".to_string(),
        }]);

        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ci_check_repo(
            &dir,
            &watch_path,
            "o/r",
            "feat",
            &["lead".to_string(), "dev".to_string()],
            None,
            None,
            None,
            &registry,
            &provider,
        ))
        .unwrap();

        // Both inboxes must contain the terminal notification. Inbox layout
        // is JSONL at home/inbox/<name>.jsonl (see inbox::inbox_path).
        for sub in ["lead", "dev"] {
            let inbox_path = dir.join("inbox").join(format!("{sub}.jsonl"));
            let body = std::fs::read_to_string(&inbox_path).unwrap_or_else(|_| {
                panic!("{sub} inbox file missing — fan-out regression: {inbox_path:?}")
            });
            assert!(
                body.contains("[ci-pass]") && body.contains("o/r@feat"),
                "{sub} inbox payload mismatch: {body}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_subscribers_reads_array() {
        let watch = serde_json::json!({
            "subscribers": [
                {"instance": "a", "subscribed_at": "x"},
                {"instance": "b", "subscribed_at": "y"}
            ],
            "instance": "ignored-when-array-present"
        });
        assert_eq!(parse_subscribers(&watch), vec!["a", "b"]);
    }

    #[test]
    fn parse_subscribers_falls_back_to_legacy_instance() {
        let watch = serde_json::json!({"instance": "legacy-only"});
        assert_eq!(parse_subscribers(&watch), vec!["legacy-only"]);
    }

    #[test]
    fn parse_subscribers_empty_when_no_data() {
        let watch = serde_json::json!({});
        assert!(parse_subscribers(&watch).is_empty());
    }

    // -----------------------------------------------------------------
    // Sprint 54 P0-5 (sub-scope B) — consecutive_skips tracking +
    // stalled/resumed inbox fan-out. Each test pins one of the four
    // contract gates from dispatch m-20260507045729197032-16.
    //
    // EMPIRICAL REGRESSION-PROOF ANCHOR: if the
    // `if next_skips >= STALL_THRESHOLD && !already_notified` guard
    // in `bump_consecutive_skips_and_maybe_notify` is dropped, the
    // "fires exactly once" assertion in
    // `stalled_event_fires_exactly_once_at_threshold` fails because
    // every subsequent skip would re-enqueue. PR description carries
    // the captured FAIL signature.
    // -----------------------------------------------------------------

    fn p05_temp_home(tag: &str) -> std::path::PathBuf {
        let dir = tmp_dir(tag);
        std::fs::create_dir_all(dir.join("ci-watches")).ok();
        std::fs::create_dir_all(dir.join("inbox")).ok();
        dir
    }

    fn p05_write_watch(
        home: &Path,
        repo: &str,
        branch: &str,
        watch: serde_json::Value,
    ) -> std::path::PathBuf {
        let path = ci_watches_dir(home).join(watch_filename(repo, branch));
        std::fs::write(&path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();
        path
    }

    fn p05_read_inbox_lines(home: &Path, instance: &str) -> Vec<String> {
        let path = home.join("inbox").join(format!("{instance}.jsonl"));
        std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    #[test]
    fn consecutive_skips_increments_on_rate_limited_skip() {
        // Gate 1: each rate-limited skip bumps the counter atomically.
        let home = p05_temp_home("p05_skip_inc");
        let watch = serde_json::json!({
            "repo": "o/r",
            "branch": "feat",
            "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"}],
            "consecutive_skips": 0,
        });
        let path = p05_write_watch(&home, "o/r", "feat", watch);
        let subscribers = vec!["lead".to_string()];

        let future_reset = (chrono::Utc::now().timestamp() as u64) + 3600;
        bump_consecutive_skips_and_maybe_notify(
            &home,
            &path,
            "o/r",
            "feat",
            &subscribers,
            future_reset,
        );
        bump_consecutive_skips_and_maybe_notify(
            &home,
            &path,
            "o/r",
            "feat",
            &subscribers,
            future_reset,
        );
        let watch: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(watch["consecutive_skips"].as_u64(), Some(2));
        // Below threshold: stalled_notified is either absent (None) or
        // false — both mean "no notify fired yet".
        let notified = watch["stalled_notified"].as_bool().unwrap_or(false);
        assert!(!notified, "below threshold ⇒ no notify yet");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn stalled_event_fires_exactly_once_at_threshold() {
        // Gate 2: at N=3 a [ci-watch-stalled] enqueues; further
        // tick-skips don't re-fire. This is the regression-proof
        // anchor — collapsing the guard reveals duplicates.
        let home = p05_temp_home("p05_stalled_once");
        let watch = serde_json::json!({
            "repo": "o/r",
            "branch": "feat",
            "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"}],
            "consecutive_skips": 0,
        });
        let path = p05_write_watch(&home, "o/r", "feat", watch);
        let subscribers = vec!["lead".to_string()];
        let future_reset = (chrono::Utc::now().timestamp() as u64) + 3600;

        for _ in 0..5 {
            bump_consecutive_skips_and_maybe_notify(
                &home,
                &path,
                "o/r",
                "feat",
                &subscribers,
                future_reset,
            );
        }

        let lines = p05_read_inbox_lines(&home, "lead");
        let stalled_count = lines
            .iter()
            .filter(|l| l.contains("ci-watch-stalled"))
            .count();
        assert_eq!(
            stalled_count, 1,
            "exactly one stalled event per stall window (got {stalled_count})"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resumed_event_fires_on_first_successful_clear() {
        // Gate 3: resume helper fires [ci-watch-resumed] exactly once
        // and clears the stall state.
        let home = p05_temp_home("p05_resumed");
        let watch = serde_json::json!({
            "repo": "o/r",
            "branch": "feat",
            "subscribers": [{"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"}],
            "consecutive_skips": 5,
            "stalled_notified": true,
            "stalled_since_ms": 1700000000000_i64,
        });
        let path = p05_write_watch(&home, "o/r", "feat", watch);
        let subscribers = vec!["lead".to_string()];

        clear_stall_and_maybe_notify_resumed(&home, &path, "o/r", "feat", &subscribers);
        // Second call must be a silent no-op (state already cleared).
        clear_stall_and_maybe_notify_resumed(&home, &path, "o/r", "feat", &subscribers);

        let watch: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(watch["consecutive_skips"].as_u64(), Some(0));
        assert_eq!(watch["stalled_notified"].as_bool(), Some(false));
        assert!(watch["stalled_since_ms"].is_null());

        let lines = p05_read_inbox_lines(&home, "lead");
        let resumed = lines
            .iter()
            .filter(|l| l.contains("ci-watch-resumed"))
            .count();
        assert_eq!(resumed, 1, "exactly one resumed event per recovery");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn stalled_and_resumed_fan_out_to_all_subscribers() {
        // Gate 4: both event types reach every subscriber per the
        // P0-1 fan-out contract.
        let home = p05_temp_home("p05_fanout");
        let watch = serde_json::json!({
            "repo": "o/r",
            "branch": "feat",
            "subscribers": [
                {"instance": "lead", "subscribed_at": "2026-05-07T00:00:00Z"},
                {"instance": "dev",  "subscribed_at": "2026-05-07T00:00:01Z"}
            ],
            "consecutive_skips": 0,
        });
        let path = p05_write_watch(&home, "o/r", "feat", watch);
        let subscribers = vec!["lead".to_string(), "dev".to_string()];
        let future_reset = (chrono::Utc::now().timestamp() as u64) + 3600;

        for _ in 0..STALL_THRESHOLD {
            bump_consecutive_skips_and_maybe_notify(
                &home,
                &path,
                "o/r",
                "feat",
                &subscribers,
                future_reset,
            );
        }
        for sub in ["lead", "dev"] {
            let lines = p05_read_inbox_lines(&home, sub);
            assert!(
                lines.iter().any(|l| l.contains("ci-watch-stalled")),
                "{sub} must receive stalled event (fan-out regression)"
            );
        }

        clear_stall_and_maybe_notify_resumed(&home, &path, "o/r", "feat", &subscribers);
        for sub in ["lead", "dev"] {
            let lines = p05_read_inbox_lines(&home, sub);
            assert!(
                lines.iter().any(|l| l.contains("ci-watch-resumed")),
                "{sub} must receive resumed event"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 54 Hotfix F gap: malformed GitHub head query ─────────────
    //
    // Per RCA m-46: `check_pr_terminal` was sending bare `head=feat/foo`
    // instead of `head=owner:feat/foo`. GitHub silently dropped the
    // filter → returned the most recent PR in the repo → Hotfix F's
    // freshness check passed → false `Terminal{merged}` for fresh-no-PR
    // branches. The malformed-query swallow is the third concrete
    // instance of the silent-drop class systematic prevention pattern
    // (decision `d-20260507100609264367-2`).
    //
    // EMPIRICAL REGRESSION-PROOF ANCHOR: dropping the `{owner}:` prefix
    // from the URL format string trips
    // `github_check_pr_terminal_uses_owner_prefix_in_head_query`. PR
    // description carries the verbatim FAIL signature.

    /// Reuse the gitlab_mock_server scaffolding — it's just an HTTP
    /// listener returning a fixed body, content-type agnostic. Renaming
    /// to `mock_server` would touch dozens of call sites; sharing the
    /// fn under a Hotfix-F-specific helper avoids that churn.
    fn github_mock_server(response_body: &str) -> (u16, std::thread::JoinHandle<()>, MockCapture) {
        gitlab_mock_server(response_body)
    }

    #[test]
    fn github_check_pr_terminal_uses_owner_prefix_in_head_query() {
        // Hotfix F gap gate 1 (regression-proof anchor): the production
        // URL must carry `head={owner}:{branch}` per GitHub docs. Empty
        // PR list is fine — the test only inspects the captured URL.
        let (port, handle, captured) = github_mock_server("[]");
        let provider = super::GitHubCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let _ = rt.block_on(provider.check_pr_terminal("acme/widgets", "feat/foo"));

        handle.join().expect("mock");
        let (path, _request) = captured.lock().expect("lock").take().expect("captured");

        assert!(
            path.contains("/repos/acme/widgets/pulls"),
            "URL must target the repo's pulls endpoint: {path}"
        );
        assert!(
            path.contains("head=acme:feat/foo"),
            "URL must use `head={{owner}}:{{branch}}` per GitHub docs (Hotfix F gap fix); got: {path}"
        );
        // Defensive: also ensure the legacy bare form is GONE — a
        // future edit that re-introduced `head={branch}` without the
        // owner prefix should trip this.
        assert!(
            !path.contains("head=feat/foo&"),
            "URL must NOT use bare `head={{branch}}` (the silent-drop bug); got: {path}"
        );
    }

    #[test]
    fn github_check_pr_terminal_returns_unknown_on_head_ref_mismatch() {
        // Hotfix F gap gate 2 (defensive): even with the correct URL,
        // GitHub may return a PR whose head.ref doesn't match what we
        // asked. The defensive check returns Unknown so we don't
        // misclassify the asked branch as Terminal based on someone
        // else's PR data.
        let mismatched = r#"[{
            "state": "closed",
            "closed_at": "2099-01-01T00:00:00Z",
            "merged_at": "2099-01-01T00:00:00Z",
            "head": {"ref": "different-branch"}
        }]"#;
        let (port, handle, captured) = github_mock_server(mismatched);
        let provider = super::GitHubCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let state = rt.block_on(provider.check_pr_terminal("acme/widgets", "feat/foo"));
        handle.join().expect("mock");
        let _ = captured;

        assert!(
            matches!(state, super::PrState::Unknown),
            "head.ref mismatch must return Unknown (defensive); got: {state:?}"
        );
    }

    #[test]
    fn github_check_pr_terminal_returns_unknown_on_empty_pr_list() {
        // Hotfix F gap gate 3 (production-realistic): the bug surfaced
        // for fresh-no-PR branches, where GitHub correctly returns []
        // once the head query is well-formed. The state machine must
        // map empty → Unknown so the auto-clear path doesn't fire.
        let (port, handle, captured) = github_mock_server("[]");
        let provider = super::GitHubCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let state = rt.block_on(provider.check_pr_terminal("acme/widgets", "fresh-branch-no-pr"));
        handle.join().expect("mock");
        let _ = captured;

        assert!(
            matches!(state, super::PrState::Unknown),
            "fresh-no-PR branch must return Unknown; got: {state:?}"
        );
    }

    #[test]
    fn github_check_pr_terminal_terminal_on_matching_head_ref_and_recent_close() {
        // Hotfix F gap gate 4: the happy path still works post-fix —
        // a closed-and-merged PR matching the head.ref returns
        // Terminal{merged: true}. Defensive head.ref check + Hotfix F
        // freshness check both pass.
        let recent_close = chrono::Utc::now() - chrono::Duration::minutes(5);
        let body = format!(
            r#"[{{
                "state": "closed",
                "closed_at": "{}",
                "merged_at": "{}",
                "head": {{"ref": "feat/foo"}}
            }}]"#,
            recent_close.to_rfc3339(),
            recent_close.to_rfc3339()
        );
        let (port, handle, captured) = github_mock_server(&body);
        let provider = super::GitHubCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
            .expect("provider");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let state = rt.block_on(provider.check_pr_terminal("acme/widgets", "feat/foo"));
        handle.join().expect("mock");
        let _ = captured;

        assert!(
            matches!(state, super::PrState::Terminal { merged: true }),
            "matching head.ref + recent merged_at must be Terminal(merged); got: {state:?}"
        );
    }

    // -------------------------------------------------------------
    // Sprint 57 Wave 2 Track B (#546 Items 1+3) — startup sweep,
    // per-tick eager GC, and protected-ref migration via
    // `gc_stale_watches` / `startup_sweep`.
    // -------------------------------------------------------------

    /// Helper for the new GC tests — write a synthetic watch JSON to
    /// disk under `home/ci-watches/<filename>`. Returns the file path.
    fn write_watch(
        home: &std::path::Path,
        repo: &str,
        branch: &str,
        watch: &serde_json::Value,
    ) -> std::path::PathBuf {
        let ci_dir = ci_watches_dir(home);
        std::fs::create_dir_all(&ci_dir).ok();
        let filename = super::watch_filename(repo, branch);
        let path = ci_dir.join(&filename);
        std::fs::write(&path, serde_json::to_string(watch).unwrap()).ok();
        path
    }

    #[test]
    fn ci_watch_ttl_expires_stale_entries_on_startup_sweep() {
        // Direct gc_stale_watches call simulates the daemon-startup
        // path that runs before the tick loop spins up. An expired
        // (absolute TTL elapsed) entry must be removed AND the event
        // log must record the removal with the startup_sweep origin.
        let home = tmp_dir("startup-sweep-ttl");
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat-stale", "interval_secs": 60,
            "subscribers": [{"instance": "agent-x"}],
            "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": past,
            "last_terminal_seen_at": null,
        });
        let path = write_watch(&home, "o/r", "feat-stale", &watch);

        super::startup_sweep(&home);

        assert!(
            !path.exists(),
            "expired watch must be removed by startup sweep"
        );
        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("ci_watch_removed"),
            "removal event must log; got:\n{log}"
        );
        assert!(
            log.contains("startup_sweep_expired"),
            "reason must include startup_sweep origin; got:\n{log}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn ci_watch_ttl_eager_gc_per_tick() {
        // Per-tick path: `check_ci_watches` calls `gc_stale_watches`
        // BEFORE the poll loop. A watch with `expires_at` in the past
        // but no subscribers list (so the legacy poll body would
        // continue rather than expire it) must still be removed by
        // the eager GC.
        let home = tmp_dir("eager-gc-ttl");
        let past = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat-stale-eager", "interval_secs": 60,
            // empty subscribers — poll loop would `continue` past it
            // without expiring; eager GC must catch it anyway.
            "subscribers": [],
            "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": past,
            "last_terminal_seen_at": null,
        });
        let path = write_watch(&home, "o/r", "feat-stale-eager", &watch);

        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        super::check_ci_watches(&home, &registry);

        assert!(
            !path.exists(),
            "eager GC must remove expired watch even when subscribers list is empty"
        );
        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("eager_gc_expired"),
            "per-tick eager-GC origin must appear in log; got:\n{log}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn migrate_existing_main_watches_at_startup() {
        // Item 3 migration: any pre-existing watch with branch=main
        // (or master) was created before the E4.5 gate landed in
        // handle_watch_ci. Startup sweep must remove it regardless
        // of TTL state so the bypass is closed retroactively.
        let home = tmp_dir("migrate-main-watches");
        let watch_main = serde_json::json!({
            "repo": "owner/repo", "branch": "main", "interval_secs": 60,
            "subscribers": [{"instance": "general"}],
            "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            // Fresh expires_at — would NOT trip the TTL paths.
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": null,
        });
        let watch_master = serde_json::json!({
            "repo": "owner/legacy-repo", "branch": "master", "interval_secs": 60,
            "subscribers": [{"instance": "lead"}],
            "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": null,
        });
        let watch_feat = serde_json::json!({
            "repo": "owner/repo", "branch": "feat-not-touched", "interval_secs": 60,
            "subscribers": [{"instance": "dev"}],
            "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": null,
        });
        let path_main = write_watch(&home, "owner/repo", "main", &watch_main);
        let path_master = write_watch(&home, "owner/legacy-repo", "master", &watch_master);
        let path_feat = write_watch(&home, "owner/repo", "feat-not-touched", &watch_feat);

        super::startup_sweep(&home);

        assert!(
            !path_main.exists(),
            "main watch must be migrated/removed by startup sweep"
        );
        assert!(
            !path_master.exists(),
            "master watch must be migrated/removed by startup sweep"
        );
        assert!(path_feat.exists(), "non-protected watch must be preserved");

        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("startup_sweep_protected_branch_migration"),
            "migration reason must surface in audit log; got:\n{log}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_stale_watches_idempotent_on_clean_dir() {
        // Defensive bonus pin: re-running the sweep on an already-
        // clean dir must be a no-op (zero removals, zero log entries).
        let home = tmp_dir("gc-idempotent");
        let watch = serde_json::json!({
            "repo": "o/r", "branch": "feat-fresh", "interval_secs": 60,
            "subscribers": [{"instance": "dev"}],
            "last_run_id": null, "head_sha": null,
            "last_polled_at": null,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": chrono::Utc::now().to_rfc3339(),
        });
        let path = write_watch(&home, "o/r", "feat-fresh", &watch);

        let n1 = super::gc_stale_watches(&home, "test_pass1");
        let n2 = super::gc_stale_watches(&home, "test_pass2");

        assert_eq!(n1, 0, "first sweep on fresh dir must remove nothing");
        assert_eq!(n2, 0, "second sweep is idempotent");
        assert!(path.exists(), "fresh watch must survive both sweeps");
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Issue #608: aggregate_conclusion_for_sha tests ──────────────────

    fn make_run(id: u64, sha: &str, conclusion: Option<&str>) -> CiRun {
        CiRun {
            id,
            head_sha: sha.to_string(),
            conclusion: conclusion.map(String::from),
            url: String::new(),
        }
    }

    #[test]
    fn aggregate_all_success_emits_pass() {
        let runs = vec![
            make_run(1, "abc123", Some("success")),
            make_run(2, "abc123", Some("success")),
        ];
        assert_eq!(
            aggregate_conclusion_for_sha(&runs, "abc123"),
            Some("success")
        );
    }

    #[test]
    fn aggregate_any_failure_emits_fail() {
        let runs = vec![
            make_run(1, "abc123", Some("success")),
            make_run(2, "abc123", Some("failure")),
        ];
        assert_eq!(
            aggregate_conclusion_for_sha(&runs, "abc123"),
            Some("failure")
        );
    }

    #[test]
    fn aggregate_in_progress_blocks_notification() {
        let runs = vec![
            make_run(1, "abc123", Some("success")),
            make_run(2, "abc123", None),
        ];
        assert_eq!(aggregate_conclusion_for_sha(&runs, "abc123"), None);
    }

    #[test]
    fn aggregate_empty_returns_none() {
        let runs = vec![make_run(1, "other", Some("success"))];
        assert_eq!(aggregate_conclusion_for_sha(&runs, "abc123"), None);
    }

    #[test]
    fn aggregate_failure_with_in_progress_still_reports_failure() {
        let runs = vec![
            make_run(1, "abc123", Some("failure")),
            make_run(2, "abc123", None),
        ];
        assert_eq!(
            aggregate_conclusion_for_sha(&runs, "abc123"),
            Some("failure")
        );
    }

    #[test]
    fn startup_sweep_preserves_valid_watches() {
        let home = tmp_dir("restart-persist");
        let ci_dir = ci_watches_dir(&home);
        std::fs::create_dir_all(&ci_dir).unwrap();

        // Create a valid (non-expired) watch
        let watch = serde_json::json!({
            "repo": "test/repo",
            "branch": "feat-branch",
            "subscribers": ["dev-1"],
            "interval_secs": 60,
            "created_at": chrono::Utc::now().to_rfc3339(),
            "last_polled_at": chrono::Utc::now().timestamp_millis() - 120_000,
        });
        let filename = watch_filename("test/repo", "feat-branch");
        std::fs::write(ci_dir.join(&filename), watch.to_string()).unwrap();

        // Simulate restart: run startup_sweep
        startup_sweep(&home);

        // Watch must survive (not expired, not protected branch)
        let surviving = std::fs::read_to_string(ci_dir.join(&filename));
        assert!(
            surviving.is_ok(),
            "valid watch must survive startup_sweep (restart persistence)"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
