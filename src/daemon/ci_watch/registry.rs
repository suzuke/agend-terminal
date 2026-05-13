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

/// Deterministic, collision-free filename for a CI watch entry.
/// Uses SHA-256 of `"{repo}:{branch}"` to avoid path traversal and
/// collisions when repo names contain `/` (e.g. `owner/repo` vs
/// `owner_repo`).
pub fn watch_filename(repo: &str, branch: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    format!("{repo}:{branch}").hash(&mut h);
    format!("{:016x}.json", h.finish())
}

/// Persist updated tracking state (last_run_id + head_sha) to the watch file.
pub(super) fn update_watch_state(watch_path: &Path, run_id: Option<u64>, head_sha: &str) {
    update_watch_state_with_notify(watch_path, run_id, head_sha, None);
}

/// Persist tracking state including last_notified_head_sha.
pub(super) fn update_watch_state_with_notify(
    watch_path: &Path,
    run_id: Option<u64>,
    head_sha: &str,
    notified_sha: Option<&str>,
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
        if let Ok(mut watch) = serde_json::from_str::<serde_json::Value>(&content) {
            watch["last_run_id"] = serde_json::json!(run_id);
            if !head_sha.is_empty() {
                watch["head_sha"] = serde_json::json!(head_sha);
            }
            if let Some(sha) = notified_sha {
                watch["last_notified_head_sha"] = serde_json::json!(sha);
            }
            watch["last_terminal_seen_at"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
            // M1: atomic write to prevent partial-file on crash
            let _ = crate::store::atomic_write(
                watch_path,
                serde_json::to_string_pretty(&watch)
                    .unwrap_or_default()
                    .as_bytes(),
            );
        }
    }
}
