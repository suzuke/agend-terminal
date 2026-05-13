use std::path::Path;

use super::registry::{ci_watches_dir, parse_subscribers, remove_watch};
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
) {
    let mut watch: serde_json::Value = match std::fs::read_to_string(watch_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
    {
        Some(v) => v,
        None => return,
    };
    let prev_skips = watch["consecutive_skips"].as_u64().unwrap_or(0);
    let next_skips = prev_skips.saturating_add(1);
    watch["consecutive_skips"] = serde_json::json!(next_skips);

    let already_notified = watch["stalled_notified"].as_bool().unwrap_or(false);
    let should_notify = next_skips >= STALL_THRESHOLD && !already_notified;
    if should_notify {
        watch["stalled_notified"] = serde_json::json!(true);
        // Stamp `stalled_since_ms` only on the first stall write — gives
        // operators a stable anchor in the inbox payload.
        if watch["stalled_since_ms"].as_i64().is_none() {
            watch["stalled_since_ms"] = serde_json::json!(chrono::Utc::now().timestamp_millis());
        }
    }
    let _ = crate::store::atomic_write(
        watch_path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    );

    if should_notify {
        let stalled_since_ms = watch["stalled_since_ms"].as_i64();
        // next_poll_eta = reset_epoch_ms (skip lifts at reset, then
        // adaptive backoff applies — but reset is the user-visible
        // "stalled until" moment).
        let next_poll_eta = (reset_epoch as i64).saturating_mul(1000);
        let setup_warning = crate::github_token::cached_setup_warning();
        let body = build_stalled_body(repo, branch, stalled_since_ms, next_poll_eta, setup_warning);
        fan_out_health_event(home, repo, branch, subscribers, "ci-watch-stalled", body);
    }
}

/// Sprint 54 P0-5 helper: clear the stall state on the first successful
/// poll after a stall window. Fans out `[ci-watch-resumed]` exactly
/// once per resume — symmetry with the stalled path.
pub(super) fn clear_stall_and_maybe_notify_resumed(
    home: &Path,
    watch_path: &Path,
    repo: &str,
    branch: &str,
    subscribers: &[String],
) {
    let mut watch: serde_json::Value = match std::fs::read_to_string(watch_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
    {
        Some(v) => v,
        None => return,
    };
    let was_stalled = watch["stalled_notified"].as_bool().unwrap_or(false);
    let had_skips = watch["consecutive_skips"].as_u64().unwrap_or(0) > 0;
    if !was_stalled && !had_skips {
        return; // common case — no stall in flight, nothing to write.
    }
    watch["consecutive_skips"] = serde_json::json!(0);
    watch["stalled_notified"] = serde_json::json!(false);
    watch["stalled_since_ms"] = serde_json::Value::Null;
    let _ = crate::store::atomic_write(
        watch_path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    );
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
) -> String {
    let mut s = format!("[ci-watch-stalled] {repo}@{branch}: rate-limit backoff in effect");
    if let Some(ts) = stalled_since_ms {
        if let Some(dt) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts) {
            s.push_str(&format!("\nStalled since: {}", dt.to_rfc3339()));
        }
    }
    if let Some(eta) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(next_poll_eta_ms) {
        s.push_str(&format!("\nNext poll ETA: {}", eta.to_rfc3339()));
    }
    if let Some(w) = setup_warning {
        s.push_str(&format!("\nSetup hint: {w}"));
    }
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
                kind: Some(kind.to_string()),
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
        let Ok(watch) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let repo = watch["repo"].as_str().unwrap_or("?");
        let branch = watch["branch"].as_str().unwrap_or("?");
        let audit_label = parse_subscribers(&watch).join(",");

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
        if let Some(expires_at) = watch["expires_at"].as_str() {
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
        if let Some(last_seen) = watch["last_terminal_seen_at"].as_str() {
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
                let watch: serde_json::Value = serde_json::from_str(&content).ok()?;
                let repo = watch["repo"].as_str()?;
                let branch = watch["branch"].as_str().unwrap_or("main");
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
