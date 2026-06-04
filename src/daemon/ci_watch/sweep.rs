use std::path::Path;

use super::registry::{ci_watches_dir, remove_watch};
use super::{MAX_WATCH_AGE_HOURS, WATCH_TTL_HOURS};

/// Sprint 54 P0-5 (sub-scope B): consecutive rate-limited skips before a
/// `[ci-watch-stalled]` notification fires. Picked low (3) so a watch
/// stuck behind a multi-minute reset window surfaces quickly without
/// over-paging on a one-tick blip.
pub(crate) const STALL_THRESHOLD: u64 = 3;

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

/// #1705: repo-level rate-limit stall tracking. With the repo-level batch poll, a
/// rate-limit is a REPO property (one batch call hit the cap), so the stall state
/// lives PER-REPO in a sidecar — NOT borrowed from a representative watch (a watch
/// can be auto-cleared by #931 PR-terminal mid-stall and must not break the repo's
/// stall anchor). One `[ci-watch-stalled]` / `[ci-watch-resumed]` per repo.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct RepoStallState {
    #[serde(default)]
    consecutive_skips: u64,
    #[serde(default)]
    stalled_notified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stalled_since_ms: Option<i64>,
}

/// Sidecar path: `<ci-watches>/<repo-slug>.stall` (extension `.stall`, not `.json`,
/// so the watch-dir scans which filter on `.json` skip it).
fn repo_stall_path(home: &Path, repo: &str) -> std::path::PathBuf {
    ci_watches_dir(home).join(format!("{}.stall", repo.replace(['/', ':'], "_")))
}

fn read_repo_stall(home: &Path, repo: &str) -> RepoStallState {
    std::fs::read_to_string(repo_stall_path(home, repo))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

/// #1705: repo-level stall bump — called ONCE per repo when a batch poll hits an
/// API/rate-limit error. Emits a single repo-level `[ci-watch-stalled]` when
/// `consecutive_skips` first crosses `STALL_THRESHOLD` (once per stall window).
pub(super) fn bump_repo_stall_and_maybe_notify(
    home: &Path,
    repo: &str,
    subscribers: &[String],
    reset_epoch: Option<u64>,
    display_timezone: Option<&str>,
) {
    let mut st = read_repo_stall(home, repo);
    st.consecutive_skips = st.consecutive_skips.saturating_add(1);
    // `stalled_notified` gates the once-per-window emit; `stalled_since_ms`
    // marks when the window opened (preserved if already set, e.g. seeded by a
    // prior bump) so the body's "Stalled since" anchor is stable.
    let should_notify = st.consecutive_skips >= STALL_THRESHOLD && !st.stalled_notified;
    if should_notify {
        st.stalled_notified = true;
        if st.stalled_since_ms.is_none() {
            st.stalled_since_ms = Some(chrono::Utc::now().timestamp_millis());
        }
    }
    let _ = crate::store::atomic_write(
        &repo_stall_path(home, repo),
        serde_json::to_string_pretty(&st)
            .unwrap_or_default()
            .as_bytes(),
    );
    if should_notify {
        let next_poll_eta = reset_epoch
            .map(|r| (r as i64).saturating_mul(1000))
            .unwrap_or(0);
        let setup_warning = crate::github_token::cached_setup_warning();
        let body = build_stalled_body(
            repo,
            "*",
            st.stalled_since_ms,
            next_poll_eta,
            setup_warning,
            display_timezone,
        );
        fan_out_health_event(home, repo, "*", subscribers, "ci-watch-stalled", body);
    }
}

/// #1705: repo-level resume — called when a batch poll SUCCEEDS. Emits one
/// `[ci-watch-resumed]` if the repo was stalled, then clears the sidecar.
pub(super) fn clear_repo_stall_and_maybe_resume(home: &Path, repo: &str, subscribers: &[String]) {
    let st = read_repo_stall(home, repo);
    if st.consecutive_skips == 0 && !st.stalled_notified && st.stalled_since_ms.is_none() {
        return; // never stalled this window — nothing to clear/announce
    }
    let was_stalled = st.stalled_notified;
    let _ = std::fs::remove_file(repo_stall_path(home, repo));
    if was_stalled {
        let body =
            format!("[ci-watch-resumed] {repo}: batch poll resumed after rate-limit backoff");
        fan_out_health_event(home, repo, "*", subscribers, "ci-watch-resumed", body);
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

        // (2b) #1750 A2: absolute age cap — backstop against a watch kept
        // perpetually young by `refresh_expires_at` on every active poll (so it
        // never trips the refreshed `expires_at` or inactivity TTL above).
        // Anchored on the earliest `subscribed_at`, the one timestamp polling
        // never moves. A watch older than MAX_WATCH_AGE_HOURS never reached
        // terminal (which would have removed it) → stale by definition.
        if let Some(created) = watch.earliest_subscribed_at() {
            if now_utc.signed_duration_since(created) > chrono::Duration::hours(MAX_WATCH_AGE_HOURS)
            {
                remove_watch(
                    home,
                    &path,
                    &audit_label,
                    repo,
                    branch,
                    &format!("{sweep_origin}_max_age"),
                );
                tracing::info!(repo = %repo, branch = %branch, hours = MAX_WATCH_AGE_HOURS,
                    sweep = %sweep_origin,
                    "ci_watch removed (absolute max-age cap — never reached terminal)");
                removed += 1;
                continue;
            }
        }
    }

    // #1740: reap orphaned `.stall` sidecars. The per-repo `<repo-slug>.stall`
    // (see `repo_stall_path`) carries the deliberate `.stall` extension so the
    // `.json` scan above skips it — but that also means once a repo's LAST
    // `.json` watch is gc'd, its `.stall` would leak on disk forever. A `.stall`
    // whose repo STILL has a surviving `.json` watch is a live stall state and
    // MUST be kept; only remove ones with no surviving watch for that repo. Run
    // AFTER the `.json` removal loop above so just-removed watches don't count as
    // surviving. (`.stall` removals are NOT added to `removed`, which counts
    // watches.)
    let surviving_repo_slugs: std::collections::HashSet<String> = std::fs::read_dir(&ci_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                return None;
            }
            let content = std::fs::read_to_string(&p).ok()?;
            let watch: super::watch_state::WatchState = serde_json::from_str(&content).ok()?;
            if watch.repo.is_empty() {
                return None;
            }
            // Mirror `repo_stall_path`'s slug so it matches the `.stall` stem.
            Some(watch.repo.replace(['/', ':'], "_"))
        })
        .collect();
    if let Ok(entries) = std::fs::read_dir(&ci_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("stall") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !surviving_repo_slugs.contains(stem) {
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!(stall = %path.display(), error = %e,
                        "#1740: orphaned .stall sidecar removal failed");
                } else {
                    tracing::info!(stall = %path.display(), sweep = %sweep_origin,
                        "#1740: removed orphaned ci-watch .stall sidecar (no surviving watch for repo)");
                }
            }
        }
    }

    // #1750 A2: reap orphaned `<hash>.lock` files. A lock shares its stem with
    // its `<hash>.json` watch and exists only to guard concurrent writes to that
    // watch; once the watch is gone the lock is dead weight. `remove_watch` now
    // deletes the sibling lock, but historical removals (and the pre-fix lack of
    // any deletion site) left 269 orphans on disk — sweep every `.lock` whose
    // sibling `.json` no longer exists. Run AFTER the `.json` removal loop so a
    // just-removed watch's lock counts as orphaned. (Not added to `removed`,
    // which counts watches, mirroring the `.stall` sweep.)
    let surviving_json_stems: std::collections::HashSet<String> = std::fs::read_dir(&ci_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                return None;
            }
            p.file_stem().and_then(|s| s.to_str()).map(String::from)
        })
        .collect();
    if let Ok(entries) = std::fs::read_dir(&ci_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("lock") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !surviving_json_stems.contains(stem) {
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!(lock = %path.display(), error = %e,
                        "#1750 A2: orphaned ci-watch .lock removal failed");
                } else {
                    tracing::info!(lock = %path.display(), sweep = %sweep_origin,
                        "#1750 A2: removed orphaned ci-watch .lock (no surviving watch)");
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

    /// #946/#1705 — repo-level `[ci-watch-stalled]` enqueue site
    /// (fan_out_health_event via bump_repo_stall_and_maybe_notify) carries
    /// `correlation_id = {repo}@*`. The `*` branch marks a repo-level event
    /// (the batch poll owns stall now, not any single watch). Pre-fix: None.
    #[test]
    fn ci_stalled_inbox_message_carries_repo_correlation_id() {
        let dir = tmp_dir("946-stalled-corr");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).unwrap();
        // Plant the repo stall sidecar at STALL_THRESHOLD-1 so the next bump
        // tips it over and fires the repo-level stall notification.
        let stall_path = ci_dir.join("o_r.stall");
        let sidecar = serde_json::json!({ "consecutive_skips": STALL_THRESHOLD - 1 });
        std::fs::write(&stall_path, serde_json::to_string_pretty(&sidecar).unwrap()).unwrap();

        let subscribers = vec!["agent1".to_string()];
        let reset_epoch = chrono::Utc::now().timestamp() as u64 + 3600;
        bump_repo_stall_and_maybe_notify(&dir, "o/r", &subscribers, Some(reset_epoch), None);

        let inbox_path = dir.join("inbox").join("agent1.jsonl");
        let content = std::fs::read_to_string(&inbox_path).unwrap();
        assert!(
            content.contains("[ci-watch-stalled]"),
            "expected [ci-watch-stalled] in inbox: {content}"
        );
        let expected = r#""correlation_id":"o/r@*""#;
        assert!(
            content.contains(expected),
            "ci-watch-stalled message must carry correlation_id={expected}: {content}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #1740: `gc_stale_watches` reaps an orphaned `.stall` sidecar once a repo's
    /// last `.json` watch is gone, but KEEPS a `.stall` while the repo still has
    /// a surviving watch (it's a live stall state). The `.stall` extension is
    /// skipped by the `.json` scan, so without this it would leak forever.
    #[test]
    fn gc_reaps_orphaned_stall_but_keeps_active() {
        let dir = tmp_dir("1740-stall-gc");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).unwrap();

        let stall = |slug: &str| ci_dir.join(format!("{slug}.stall"));
        let write_stall =
            |slug: &str| std::fs::write(stall(slug), r#"{"consecutive_skips":3}"#).unwrap();
        let write_watch = |name: &str, repo: &str, expires_at: &str| {
            let w = serde_json::json!({ "repo": repo, "branch": "feature-x", "expires_at": expires_at });
            std::fs::write(
                ci_dir.join(format!("{name}.json")),
                serde_json::to_string_pretty(&w).unwrap(),
            )
            .unwrap();
        };

        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let future = (chrono::Utc::now() + chrono::Duration::hours(24)).to_rfc3339();

        // repo "o/r" (slug o_r): only watch is EXPIRED → gc'd → .stall orphaned.
        write_watch("o_r_main", "o/r", &past);
        write_stall("o_r");
        // repo "a/b" (slug a_b): watch ALIVE (future TTL) → survives → keep .stall.
        write_watch("a_b_main", "a/b", &future);
        write_stall("a_b");

        gc_stale_watches(&dir, "test");

        assert!(
            !stall("o_r").exists(),
            "orphaned .stall (no surviving watch for repo) must be gc'd"
        );
        assert!(
            stall("a_b").exists(),
            "active .stall (repo still has a surviving watch) must be KEPT"
        );
        // sanity on the watches themselves
        assert!(
            !ci_dir.join("o_r_main.json").exists(),
            "expired watch should be removed"
        );
        assert!(
            ci_dir.join("a_b_main.json").exists(),
            "live watch should survive"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #1750 A2: an orphaned `<hash>.lock` (no sibling `.json`) is reaped; a
    /// `.lock` whose `.json` watch still lives is kept.
    #[test]
    fn gc_reaps_orphaned_lock_but_keeps_sibling_of_live_watch() {
        let dir = tmp_dir("1750-lock-gc");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).unwrap();

        let future = (chrono::Utc::now() + chrono::Duration::hours(24)).to_rfc3339();
        let w = serde_json::json!({ "repo": "o/r", "branch": "feat", "expires_at": future });
        std::fs::write(
            ci_dir.join("live.json"),
            serde_json::to_string_pretty(&w).unwrap(),
        )
        .unwrap();
        std::fs::write(ci_dir.join("live.lock"), b"").unwrap();
        // orphan: a .lock with no sibling .json
        std::fs::write(ci_dir.join("orphan.lock"), b"").unwrap();

        gc_stale_watches(&dir, "test");

        assert!(
            !ci_dir.join("orphan.lock").exists(),
            "orphaned .lock (no sibling .json) must be gc'd"
        );
        assert!(
            ci_dir.join("live.lock").exists(),
            "sibling .lock of a live watch must be KEPT"
        );
        assert!(ci_dir.join("live.json").exists(), "live watch survives");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #1750 A2: a watch kept perpetually young by `refresh_expires_at` (future
    /// `expires_at`) is still removed once its earliest `subscribed_at` is older
    /// than the absolute max-age cap; a recently-subscribed watch survives.
    #[test]
    fn gc_max_age_cap_removes_watch_kept_young_by_polling() {
        let dir = tmp_dir("1750-maxage-gc");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).unwrap();

        let future = (chrono::Utc::now() + chrono::Duration::hours(24)).to_rfc3339();
        let old_sub =
            (chrono::Utc::now() - chrono::Duration::hours(MAX_WATCH_AGE_HOURS + 1)).to_rfc3339();
        let stale = serde_json::json!({
            "repo": "o/r", "branch": "stale", "expires_at": future,
            "subscribers": [{ "instance": "dev", "subscribed_at": old_sub }],
        });
        std::fs::write(
            ci_dir.join("stale.json"),
            serde_json::to_string_pretty(&stale).unwrap(),
        )
        .unwrap();
        std::fs::write(ci_dir.join("stale.lock"), b"").unwrap();

        let recent_sub = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let young = serde_json::json!({
            "repo": "o/r", "branch": "young", "expires_at": future,
            "subscribers": [{ "instance": "dev", "subscribed_at": recent_sub }],
        });
        std::fs::write(
            ci_dir.join("young.json"),
            serde_json::to_string_pretty(&young).unwrap(),
        )
        .unwrap();

        let removed = gc_stale_watches(&dir, "test");

        assert!(
            !ci_dir.join("stale.json").exists(),
            "watch older than max-age cap must be removed despite a future expires_at"
        );
        assert!(
            !ci_dir.join("stale.lock").exists(),
            "the max-age-removed watch's sibling .lock goes with it (remove_watch)"
        );
        assert!(
            ci_dir.join("young.json").exists(),
            "recently-subscribed watch must survive the age cap"
        );
        assert_eq!(removed, 1, "exactly the over-age watch removed");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #1750 A2: `remove_watch` deletes the sibling `.lock` alongside the `.json`.
    #[test]
    fn remove_watch_deletes_sibling_lock_1750() {
        let dir = tmp_dir("1750-remove-lock");
        let ci_dir = dir.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).unwrap();
        let json = ci_dir.join("w.json");
        let lock = ci_dir.join("w.lock");
        std::fs::write(&json, "{}").unwrap();
        std::fs::write(&lock, b"").unwrap();

        remove_watch(&dir, &json, "dev", "o/r", "feat", "test");

        assert!(!json.exists(), "watch .json removed");
        assert!(!lock.exists(), "sibling .lock removed too");
        std::fs::remove_dir_all(&dir).ok();
    }
}
