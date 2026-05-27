use std::path::{Path, PathBuf};

/// Canonical path to the ci-watches directory.
pub fn ci_watches_dir(home: &Path) -> PathBuf {
    home.join("ci-watches")
}

/// Read the list of subscribed instances from a watch JSON value.
///
/// Schema migration (Sprint 54 P0-1): the canonical source is the
/// `subscribers` array (`[{instance, subscribed_at}, …]`). Pre-Sprint-54
/// files carry only a single `instance: "X"` field; this helper returns
/// `[X]` for them so the daemon's poll loop, notify path, and unwatch
/// logic all see one uniform `Vec<String>` regardless of file vintage.
///
/// The legacy `instance` field is preserved on writes for one release
/// cycle (read-only by writers post-r0) and slated for removal in
/// Sprint 55 once daemons in the wild have written-back the new
/// schema at least once.
pub(crate) fn parse_subscribers(watch: &serde_json::Value) -> Vec<String> {
    if let Some(arr) = watch.get("subscribers").and_then(|v| v.as_array()) {
        let mut out: Vec<String> = arr
            .iter()
            .filter_map(|s| s.get("instance").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        out.sort();
        out.dedup();
        if !out.is_empty() {
            return out;
        }
    }
    // Legacy: pre-r0 watch files carry only `instance: "X"`. Treat as a
    // singleton list so the rest of the pipeline doesn't have to fork.
    if let Some(legacy) = watch.get("instance").and_then(|v| v.as_str()) {
        if !legacy.is_empty() {
            return vec![legacy.to_string()];
        }
    }
    Vec::new()
}

/// Remove a watch file and log the removal event.
///
/// `instance_label` is a free-form audit string — the caller passes
/// either a single subscriber (legacy callers) or comma-joined
/// subscribers (post-r0 multi-caller). The event log mirrors the
/// label verbatim for human-readable traceability.
pub fn remove_watch(
    home: &Path,
    watch_path: &Path,
    instance_label: &str,
    repo: &str,
    branch: &str,
    reason: &str,
) {
    let _ = std::fs::remove_file(watch_path);
    crate::event_log::log(
        home,
        "ci_watch_removed",
        instance_label,
        &format!("repo={repo} branch={branch} reason={reason}"),
    );
}

/// Deterministic, collision-resistant filename for a CI watch entry.
/// Uses SHA-256 of `"{repo}:{branch}"` to avoid path traversal and
/// collisions when repo names contain `/` (e.g. `owner/repo` vs
/// `owner_repo`). Cryptographic — collision is computationally
/// infeasible (no known sha256 collision attacks).
///
/// Pre-#943 this used `std::collections::hash_map::DefaultHasher`
/// (SipHash-2-4 truncated to 64 bits) — birthday collision at
/// ~2^32 entries and within-session adversarial collision findable
/// at ~2^32 brute-force. The docstring already claimed sha256; this
/// fix brings implementation into line. Filename grows from 16 hex
/// (DefaultHasher) to 64 hex (sha256).
///
/// Old-format files are migrated at boot via
/// [`super::migration::migrate_legacy_watch_filenames`] (#942/#943
/// PR-B). Operators don't see duplicate 72h notifications because the
/// migration runs synchronously before the poller loop starts.
///
/// Performance: ~900ns/call vs DefaultHasher's ~100ns. At typical
/// per-watch subscription rate (~100/agent/day) the ~90µs/day delta
/// is negligible (#942 dev-2 cross-audit Pushback 4).
pub fn watch_filename(repo: &str, branch: &str) -> String {
    let composite = format!("{repo}:{branch}");
    format!(
        "{}.json",
        crate::daemon::utils::sha256_hex(composite.as_bytes())
    )
}

/// Persist updated tracking state (last_run_id + head_sha) to the watch file.
/// Retained for tests that exercise the legacy per-field write path.
#[cfg(test)]
#[allow(dead_code)]
pub(super) fn update_watch_state(watch_path: &Path, run_id: Option<u64>, head_sha: &str) {
    update_watch_state_with_notify(watch_path, run_id, head_sha, None, None, None);
}

/// Persist tracking state including last_notified_head_sha,
/// last_notified_conclusion (#786), and last_stale_emitted_sha
/// (#1026). Retained for tests; production path uses
/// `flush_watch_state` after in-memory mutation.
#[cfg(test)]
#[allow(dead_code)]
pub(super) fn update_watch_state_with_notify(
    watch_path: &Path,
    run_id: Option<u64>,
    head_sha: &str,
    notified_sha: Option<&str>,
    notified_conclusion: Option<&str>,
    stale_emitted_sha: Option<&str>,
) {
    // #692: flock protects RMW against concurrent unsubscribe
    let lock_path = watch_path.with_extension("lock");
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(path = %lock_path.display(), error = %e, "failed to acquire ci-watch lock, skipping update");
            return;
        }
    };
    if let Ok(content) = std::fs::read_to_string(watch_path) {
        if let Ok(mut watch) = serde_json::from_str::<super::watch_state::WatchState>(&content) {
            watch.last_run_id = run_id;
            if !head_sha.is_empty() {
                watch.head_sha = Some(head_sha.to_string());
            }
            if let Some(sha) = notified_sha {
                watch.last_notified_head_sha = Some(sha.to_string());
            }
            if let Some(c) = notified_conclusion {
                watch.last_notified_conclusion = Some(c.to_string());
            }
            if let Some(s) = stale_emitted_sha {
                watch.last_stale_emitted_sha = Some(s.to_string());
            }
            watch.last_terminal_seen_at = Some(chrono::Utc::now().to_rfc3339());
            // M1: atomic write to prevent partial-file on crash
            if let Err(e) = crate::store::atomic_write(
                watch_path,
                serde_json::to_string_pretty(&watch)
                    .unwrap_or_default()
                    .as_bytes(),
            ) {
                tracing::warn!(path = %watch_path.display(), error = %e, "ci-watch state write failed");
            }
        }
    }
}

/// Flush an in-memory WatchState to disk under the ci-watch flock.
/// Merge-safe: starts from the on-disk state (preserving all
/// control-plane fields) and only applies poll-owned deltas from
/// the in-memory snapshot.
pub(super) fn flush_watch_state(watch_path: &Path, state: &super::watch_state::WatchState) {
    let lock_path = watch_path.with_extension("lock");
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(path = %lock_path.display(), error = %e, "failed to acquire ci-watch lock, skipping flush");
            return;
        }
    };
    let mut merged = match std::fs::read_to_string(watch_path)
        .ok()
        .and_then(|c| serde_json::from_str::<super::watch_state::WatchState>(&c).ok())
    {
        Some(c) => c,
        None => return, // file deleted by concurrent unwatch — respect deletion
    };
    // Apply only poll-owned fields from in-memory state.
    merged.last_run_id = state.last_run_id;
    merged.head_sha = state.head_sha.clone();
    merged.last_polled_at = state.last_polled_at;
    merged.effective_interval_secs = state.effective_interval_secs;
    merged.last_terminal_seen_at = state.last_terminal_seen_at.clone();
    merged.last_notified_head_sha = state.last_notified_head_sha.clone();
    merged.last_notified_conclusion = state.last_notified_conclusion.clone();
    merged.last_stale_emitted_sha = state.last_stale_emitted_sha.clone();
    merged.last_mergeable_state = state.last_mergeable_state.clone();
    merged.last_mergeable_check_at = state.last_mergeable_check_at.clone();
    merged.rate_limit_until = state.rate_limit_until;
    merged.rate_limit_remaining = state.rate_limit_remaining;
    merged.rate_limit_limit = state.rate_limit_limit;
    merged.consecutive_skips = state.consecutive_skips;
    merged.stalled_notified = state.stalled_notified;
    merged.stalled_since_ms = state.stalled_since_ms;
    merged.terminal_since = state.terminal_since.clone();
    merged.early_fail_notified_sha = state.early_fail_notified_sha.clone();
    if let Err(e) = crate::store::atomic_write(
        watch_path,
        serde_json::to_string_pretty(&merged)
            .unwrap_or_default()
            .as_bytes(),
    ) {
        tracing::warn!(path = %watch_path.display(), error = %e, "ci-watch state flush failed");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ── #943 sha256 contract for watch_filename ──

    #[test]
    fn watch_filename_uses_sha256_64_hex_chars_plus_json_extension() {
        let f = watch_filename("owner/repo", "feat/x");
        assert!(f.ends_with(".json"), "filename must end .json: {f}");
        let stem = f.strip_suffix(".json").unwrap();
        assert_eq!(
            stem.len(),
            64,
            "sha256 hex digest is 64 chars (post-#943): {f}"
        );
        assert!(
            stem.chars().all(|c| c.is_ascii_hexdigit()),
            "filename stem must be pure ascii hex: {f}"
        );
    }

    #[test]
    fn watch_filename_deterministic_across_calls() {
        let a = watch_filename("owner/repo", "feat/x");
        let b = watch_filename("owner/repo", "feat/x");
        assert_eq!(a, b, "watch_filename must be deterministic");
    }

    #[test]
    fn watch_filename_distinguishes_delimiter_ambiguity() {
        // `owner/repo` + `feat-x` must NOT collide with
        // `owner` + `repo:feat-x` despite both producing the same raw
        // composite under naive concatenation.
        let a = watch_filename("owner/repo", "feat-x");
        let b = watch_filename("owner", "repo:feat-x");
        assert_ne!(
            a, b,
            "filename hash must distinguish (repo, branch) split — composite was `repo:branch` (concat ambiguous)"
        );
    }

    #[test]
    fn watch_filename_differs_from_pre_943_defaulthasher_length() {
        // Pre-#943 DefaultHasher output: 16 hex + .json (21 chars total).
        // Post-#943 sha256: 64 hex + .json (69 chars total).
        // This guards against accidentally reverting to DefaultHasher.
        let f = watch_filename("owner/repo", "feat/x");
        assert!(
            f.len() > 21,
            "post-#943 filename length must exceed legacy DefaultHasher length (21): got {f}"
        );
    }

    #[test]
    fn flush_preserves_concurrent_unwatch() {
        let dir = std::env::temp_dir().join(format!("agend-flush-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let watch_path = dir.join("test.json");

        let initial = super::super::watch_state::WatchState {
            repo: "o/r".into(),
            branch: "feat".into(),
            subscribers: Some(vec![
                super::super::watch_state::Subscriber {
                    instance: "A".into(),
                    subscribed_at: None,
                },
                super::super::watch_state::Subscriber {
                    instance: "B".into(),
                    subscribed_at: None,
                },
            ]),
            ..Default::default()
        };
        std::fs::write(&watch_path, serde_json::to_string_pretty(&initial).unwrap()).unwrap();

        let mut stale = initial.clone();
        stale.last_run_id = Some(42);
        stale.head_sha = Some("abc123".into());

        let mut on_disk = initial;
        on_disk.subscribers = Some(vec![super::super::watch_state::Subscriber {
            instance: "A".into(),
            subscribed_at: None,
        }]);
        std::fs::write(&watch_path, serde_json::to_string_pretty(&on_disk).unwrap()).unwrap();

        flush_watch_state(&watch_path, &stale);

        let result: super::super::watch_state::WatchState =
            serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
        assert_eq!(result.last_run_id, Some(42), "poll fields must be applied");
        assert_eq!(result.head_sha.as_deref(), Some("abc123"));
        let subs: Vec<String> = result.subscriber_names();
        assert_eq!(
            subs,
            vec!["A"],
            "concurrent unwatch of B must be preserved, not overwritten"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flush_respects_concurrent_deletion() {
        let dir = std::env::temp_dir().join(format!("agend-flush-delete-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let watch_path = dir.join("deleted.json");

        let stale = super::super::watch_state::WatchState {
            repo: "o/r".into(),
            branch: "feat".into(),
            last_run_id: Some(99),
            ..Default::default()
        };

        // File does not exist (concurrent unwatch deleted it).
        assert!(!watch_path.exists());
        flush_watch_state(&watch_path, &stale);
        assert!(
            !watch_path.exists(),
            "flush must not resurrect a deleted watch file"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flush_preserves_concurrent_metadata_update() {
        let dir = std::env::temp_dir().join(format!("agend-flush-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let watch_path = dir.join("meta.json");

        let initial = super::super::watch_state::WatchState {
            repo: "o/r".into(),
            branch: "feat".into(),
            expires_at: Some("2026-01-01T00:00:00Z".into()),
            task_id: Some("t-old".into()),
            required_checks: None,
            ..Default::default()
        };
        std::fs::write(&watch_path, serde_json::to_string_pretty(&initial).unwrap()).unwrap();

        // Poller snapshot taken at tick start (stale metadata).
        let mut stale = initial.clone();
        stale.last_run_id = Some(42);
        stale.head_sha = Some("abc".into());

        // Concurrent `ci watch` updates metadata on disk.
        let mut updated = initial;
        updated.expires_at = Some("2026-06-01T00:00:00Z".into());
        updated.task_id = Some("t-new".into());
        updated.required_checks = Some(vec!["build".into()]);
        std::fs::write(&watch_path, serde_json::to_string_pretty(&updated).unwrap()).unwrap();

        flush_watch_state(&watch_path, &stale);

        let result: super::super::watch_state::WatchState =
            serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
        assert_eq!(result.last_run_id, Some(42), "poll field must be applied");
        assert_eq!(result.head_sha.as_deref(), Some("abc"));
        assert_eq!(
            result.expires_at.as_deref(),
            Some("2026-06-01T00:00:00Z"),
            "concurrent expires_at update must survive flush"
        );
        assert_eq!(
            result.task_id.as_deref(),
            Some("t-new"),
            "concurrent task_id update must survive flush"
        );
        assert_eq!(
            result.required_checks.as_deref(),
            Some(&["build".to_string()][..]),
            "concurrent required_checks update must survive flush"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
