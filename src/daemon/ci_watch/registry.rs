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
pub(super) fn update_watch_state(watch_path: &Path, run_id: Option<u64>, head_sha: &str) {
    update_watch_state_with_notify(watch_path, run_id, head_sha, None, None, None);
}

/// Persist tracking state including last_notified_head_sha,
/// last_notified_conclusion (#786), and last_stale_emitted_sha
/// (#1026). All optional fields are written only when caller
/// supplies them — preserves the "no state churn when no
/// notification fires" invariant.
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
}
