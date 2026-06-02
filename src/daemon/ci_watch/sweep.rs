use std::path::Path;

use super::registry::{ci_watches_dir, remove_watch};
use super::WATCH_TTL_HOURS;

/// Sprint 54 P0-5 (sub-scope B): consecutive rate-limited skips before a
/// `[ci-watch-stalled]` notification fires. Picked low (3) so a watch
/// stuck behind a multi-minute reset window surfaces quickly without
/// over-paging on a one-tick blip.
pub(crate) const STALL_THRESHOLD: u64 = 3;

/// Sprint 54 P0-5 helper: read existing `consecutive_skips`, increment,
/// persist, and (if we just crossed `STALL_THRESHOLD` and haven't yet
/// notified for this window) fan out a `[ci-watch-stalled]` inbox event
/// to every subscriber. The notify step reuses the P0-1 fan-out
/// contract — one inbox enqueue per subscriber.
///
/// Atomicity: the increment + `stalled_notified` flag move in a single
/// atomic_write so the next tick can't observe a "skips ≥ threshold,
/// flag still false" intermediate state and fire a duplicate event.
pub(super) fn bump_consecutive_skips_and_maybe_notify(
    home: &Path,
    watch_path: &Path,
    repo: &str,
    branch: &str,
    subscribers: &[String],
    reset_epoch: u64,
    display_timezone: Option<&str>,
) {
    let mut watch: super::watch_state::WatchState = match std::fs::read_to_string(watch_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
    {
        Some(v) => v,
        None => return,
    };
    let prev_skips = watch.consecutive_skips.unwrap_or(0);
    let next_skips = prev_skips.saturating_add(1);
    watch.consecutive_skips = Some(next_skips);

    let already_notified = watch.stalled_notified.unwrap_or(false);
    let should_notify = next_skips >= STALL_THRESHOLD && !already_notified;
    if should_notify {
        watch.stalled_notified = Some(true);
        if watch.stalled_since_ms.is_none() {
            watch.stalled_since_ms = Some(chrono::Utc::now().timestamp_millis());
        }
    }
    if let Err(e) = crate::store::atomic_write(
        watch_path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    ) {
        tracing::warn!(path = %watch_path.display(), error = %e, "ci-watch stall-counter write failed");
    }

    if should_notify {
        let stalled_since_ms = watch.stalled_since_ms;
        // next_poll_eta = reset_epoch_ms (skip lifts at reset, then
        // adaptive backoff applies — but reset is the user-visible
        // "stalled until" moment).
        let next_poll_eta = (reset_epoch as i64).saturating_mul(1000);
        let setup_warning = crate::github_token::cached_setup_warning();
        let body = build_stalled_body(
            repo,
            branch,
            stalled_since_ms,
            next_poll_eta,
            setup_warning,
            display_timezone,
        );
        fan_out_health_event(home, repo, branch, subscribers, "ci-watch-stalled", body);
    }
}

/// Sprint 54 P0-5 helper: clear the stall state on the first successful
/// poll after a stall window. Retained for tests; production path uses
/// `clear_stall_state` with in-memory mutation.
#[cfg(test)]
pub(super) fn clear_stall_and_maybe_notify_resumed(
    home: &Path,
    watch_path: &Path,
    repo: &str,
    branch: &str,
    subscribers: &[String],
) {
    let mut watch: super::watch_state::WatchState = match std::fs::read_to_string(watch_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
    {
        Some(v) => v,
        None => return,
    };
    let was_stalled = watch.stalled_notified.unwrap_or(false);
    let had_skips = watch.consecutive_skips.unwrap_or(0) > 0;
    if !was_stalled && !had_skips {
        return;
    }
    watch.consecutive_skips = Some(0);
    watch.stalled_notified = Some(false);
    watch.stalled_since_ms = None;
    if let Err(e) = crate::store::atomic_write(
        watch_path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    ) {
        tracing::warn!(path = %watch_path.display(), error = %e, "ci-watch stall-clear write failed");
    }
    if was_stalled {
        let body =
            format!("[ci-watch-resumed] {repo}@{branch}: poll resumed after rate-limit backoff");
        fan_out_health_event(home, repo, branch, subscribers, "ci-watch-resumed", body);
    }
}

/// In-memory variant of [`clear_stall_and_maybe_notify_resumed`] — clears
/// stall fields on the provided `WatchState` and emits the resume
/// notification if the watch was stalled.  The caller is responsible for
/// flushing the state to disk.
pub(super) fn clear_stall_state(
    state: &mut super::watch_state::WatchState,
    home: &Path,
    repo: &str,
    branch: &str,
    subscribers: &[String],
) {
    let was_stalled = state.stalled_notified.unwrap_or(false);
    let had_skips = state.consecutive_skips.unwrap_or(0) > 0;
    if !was_stalled && !had_skips {
        return;
    }
    state.consecutive_skips = Some(0);
    state.stalled_notified = Some(false);
    state.stalled_since_ms = None;
    if was_stalled {
        let body =
            format!("[ci-watch-resumed] {repo}@{branch}: poll resumed after rate-limit backoff");
        fan_out_health_event(home, repo, branch, subscribers, "ci-watch-resumed", body);
    }
}

fn build_stalled_body(
    repo: &str,
    branch: &str,
    stalled_since_ms: Option<i64>,
    next_poll_eta_ms: i64,
    setup_warning: Option<&'static str>,
    display_timezone: Option<&str>,
) -> String {
    let mut s = format!("[ci-watch-stalled] {repo}@{branch}: rate-limit backoff in effect");
    if let Some(ts) = stalled_since_ms {
        if let Some(dt) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts) {
            // #790: render notification body in operator-configured tz;
            // storage (`stalled_since_ms` json field) stays UTC.
            s.push_str(&format!(
                "\nStalled since: {}",
                crate::display_time::format_local_short(&dt.to_rfc3339(), display_timezone)
            ));
        }
    }
    if let Some(eta) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(next_poll_eta_ms) {
        s.push_str(&format!(
            "\nNext poll ETA: {}",
            crate::display_time::format_local_short(&eta.to_rfc3339(), display_timezone)
        ));
    }
    if let Some(w) = setup_warning {
        s.push_str(&format!("\nSetup hint: {w}"));
    }
    s.push_str(
        "\n\nPolling paused due to rate-limit backoff (\u{2265}3 consecutive skips).\n\
         Will auto-resume when rate-limit window expires.\n\
         Action: no immediate action needed. If stalled >30min, check githubstatus.com and escalate to operator.",
    );
    s
}

/// Sprint 54 P0-5: fan out a CI health event to every subscriber.
/// Mirrors the P0-1 terminal-notify loop — one inbox enqueue per
/// subscriber so multi-caller watches don't get last-write-wins.
fn fan_out_health_event(
    home: &Path,
    repo: &str,
    branch: &str,
    subscribers: &[String],
    kind: &str,
    body: String,
) {
    let repo_branch_key = format!("{repo}@{branch}");
    let supersede_token = format!("{kind}-{}", chrono::Utc::now().timestamp_millis());
    for sub in subscribers {
        crate::inbox::mark_ci_watch_superseded(home, sub, &repo_branch_key, &supersede_token);
        persist_or_log!(
            crate::inbox::enqueue_with_idle_hint(
                home,
                sub,
                crate::inbox::InboxMessage::new_system("system:ci", kind, body.clone())
                    // #946: canonical `{repo}@{branch}` form — stable grep target.
                    .with_correlation_id(repo_branch_key.clone()),
            ),
            "ci_health_event",
            sub
        );
    }
}

/// Sprint 57 Wave 2 Track B (#546 Item 1 + Item 3 migration) —
/// scan ci-watches dir, remove any watch that:
///   1. has `expires_at < now` (absolute TTL elapsed),
///   2. has `last_terminal_seen_at` older than `WATCH_TTL_HOURS`
///      (inactivity TTL elapsed), or
///   3. targets a protected ref per `agent_ops::is_protected_ref`
///      (E4.5 migration — closes the ci_watch-on-main bypass that
///      Sprint 56's `handle_watch_ci` left open until Wave 2 Track B
///      gated it).
///
/// The poll loop (`check_ci_watches_with_provider`) already enforces
/// (1) and (2) lazily on every per-watch tick, but only for watches
/// it actively polls — a watch can persist on disk indefinitely if
/// the upstream branch is gone or no agent is currently polling it.
/// This eager helper closes that gap by walking the entire dir
/// without entering the poll path.
///
/// Returns the number of watches removed. Best-effort: read/parse
/// failures skip the entry rather than aborting the sweep.
pub fn gc_stale_watches(home: &Path, sweep_origin: &str) -> usize {
    let ci_dir = ci_watches_dir(home);
    let Ok(entries) = std::fs::read_dir(&ci_dir) else {
        return 0;
    };
    let now_utc = chrono::Utc::now();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(watch) = serde_json::from_str::<super::watch_state::WatchState>(&content) else {
            continue;
        };
        let repo = if watch.repo.is_empty() {
            "?"
        } else {
            &watch.repo
        };
        let branch = &watch.branch;
        let audit_label = watch.subscriber_names().join(",");

        // (3) E4.5 protected-ref migration — applied first because a
        // protected-ref watch is invalid regardless of TTL state.
        if crate::agent_ops::is_protected_ref(branch) {
            remove_watch(
                home,
                &path,
                &audit_label,
                repo,
                branch,
                &format!("{sweep_origin}_protected_branch_migration"),
            );
            tracing::info!(repo = %repo, branch = %branch, sweep = %sweep_origin,
                "ci_watch removed (E4.5 protected-branch migration)");
            removed += 1;
            continue;
        }

        // (1) absolute TTL.
        if let Some(expires_at) = watch.expires_at.as_deref() {
            if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(expires_at) {
                if now_utc > exp.with_timezone(&chrono::Utc) {
                    remove_watch(
                        home,
                        &path,
                        &audit_label,
                        repo,
                        branch,
                        &format!("{sweep_origin}_expired"),
                    );
                    tracing::info!(repo = %repo, branch = %branch, sweep = %sweep_origin,
                        "ci_watch removed (absolute TTL elapsed)");
                    removed += 1;
                    continue;
                }
            }
        }

        // (2) inactivity TTL.
        if let Some(last_seen) = watch.last_terminal_seen_at.as_deref() {
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(last_seen) {
                let elapsed = now_utc.signed_duration_since(ts.with_timezone(&chrono::Utc));
                if elapsed > chrono::Duration::hours(WATCH_TTL_HOURS) {
                    remove_watch(
                        home,
                        &path,
                        &audit_label,
                        repo,
                        branch,
                        &format!("{sweep_origin}_inactivity_ttl"),
                    );
                    tracing::info!(repo = %repo, branch = %branch, hours = WATCH_TTL_HOURS,
                        sweep = %sweep_origin,
                        "ci_watch removed (inactivity TTL elapsed)");
                    removed += 1;
                    continue;
                }
            }
        }
    }
    removed
}

/// Sprint 57 Wave 2 Track B (#546 Item 1) — daemon-startup eager
/// sweep. Runs once before the tick loop begins so stale entries
/// from a prior daemon process don't outlive the restart. Idempotent;
/// re-runs are no-ops once the dir is clean.
pub fn startup_sweep(home: &Path) {
    let removed = gc_stale_watches(home, "startup_sweep");
    if removed > 0 {
        tracing::info!(removed, "ci_watch startup sweep complete");
    }
    // Log surviving watches so operators can confirm persistence across restart.
    let ci_dir = ci_watches_dir(home);
    if let Ok(entries) = std::fs::read_dir(&ci_dir) {
        let active: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    return None;
                }
                let content = std::fs::read_to_string(&path).ok()?;
                let watch: super::watch_state::WatchState = serde_json::from_str(&content).ok()?;
                let repo = if watch.repo.is_empty() {
                    return None;
                } else {
                    &watch.repo
                };
                let branch = &watch.branch;
                Some(format!("{repo}@{branch}"))
            })
            .collect();
        if !active.is_empty() {
            tracing::info!(
                count = active.len(),
                watches = %active.join(", "),
                "ci_watch: restored watches from disk after restart"
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-946-sweep-{}-{}-{}",
            tag,
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// #946 — sweep.rs `[ci-watch-stalled]` enqueue site (fan_out_health_event
    /// at :161 via bump_consecutive_skips_and_maybe_notify) carries
    /// `correlation_id = {repo}@{branch}`. Pre-fix: None.
    #[test]
    fn ci_stalled_inbox_message_carries_repo_branch_correlation_id() {
        let dir = tmp_dir("946-stalled-corr");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).unwrap();
        // Plant a watch with consecutive_skips == STALL_THRESHOLD-1 so
        // the next bump tips it over and fires the stall notification.
        let watch_path = ci_dir.join("test-watch.json");
        let watch = serde_json::json!({
            "repo": "o/r",
            "branch": "feat",
            "consecutive_skips": STALL_THRESHOLD - 1,
            "stalled_notified": false,
            "subscribers": [{"instance": "agent1", "subscribed_at": "2026-05-19T00:00:00Z"}],
        });
        std::fs::write(&watch_path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

        let subscribers = vec!["agent1".to_string()];
        let reset_epoch = chrono::Utc::now().timestamp() as u64 + 3600;
        bump_consecutive_skips_and_maybe_notify(
            &dir,
            &watch_path,
            "o/r",
            "feat",
            &subscribers,
            reset_epoch,
            None,
        );

        let inbox_path = dir.join("inbox").join("agent1.jsonl");
        let content = std::fs::read_to_string(&inbox_path).unwrap();
        assert!(
            content.contains("[ci-watch-stalled]"),
            "expected [ci-watch-stalled] in inbox: {content}"
        );
        let expected = r#""correlation_id":"o/r@feat""#;
        assert!(
            content.contains(expected),
            "ci-watch-stalled message must carry correlation_id={expected}: {content}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
