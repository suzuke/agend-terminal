//! #1888 phase-2: correlation-keyed CI-handoff tracks (track-until-resolution).
//!
//! The #1859/#1860 re-nudge + escalation watchdog used to scan
//! `unread_of_kind(target, "ci-ready-for-action")` — but ANY inbox drain marks
//! the handoff read (storage.rs drain is kind-blind), so the watchdog went
//! blind the moment the reviewer ran a routine inbox check. Production
//! readout (2026-06-10): 14 `#1888-ciready-read` events ALL at
//! `age_at_read 2-7s`, ZERO `#1888-renudge-decision` events — the 2-min
//! re-nudge window never once opened; delivery worked only when the reviewer
//! happened to act on the wake itself.
//!
//! This store decouples the watchdog from inbox read-state: one sidecar file
//! per `(target, correlation)` is RECORDED when the poller enqueues the
//! `[ci-ready-for-action]` handoff, and RESOLVED (deleted) on an explicit
//! resolution signal instead of on read:
//!   - a `kind=report` arriving with that correlation (reviewer verdict
//!     reports carry `repo@branch` — the messaging report-arrival chokepoint,
//!     same spot as `dispatch_idle::mark_resolved`);
//!   - the PR reaching a terminal state (merged / closed-unmerged — the
//!     pr_state scanner);
//!   - the TARGET claiming the branch (a worktree binding via `bind_full`);
//!   - the 24h [`TRACK_MAX_AGE`] backstop swept by the watchdog (a track
//!     whose resolution signal never arrives must not re-nudge forever).
//!
//! Mirrors the `pending-dispatches` file-per-entry sidecar pattern (#1866).
//! No per-file locks: every operation is a whole-file write or a delete (no
//! read-modify-write), and the worst interleaving — a resolution racing a
//! fresh `record` for the same correlation (a new CI pass) — correctly leaves
//! the new episode tracked.

use std::path::{Path, PathBuf};

pub(crate) const SCHEMA_VERSION: u32 = 1;

/// Backstop lifetime: a track never resolved within this window is swept
/// (with a WARN) so the re-nudge cannot run forever.
pub(crate) const TRACK_MAX_AGE: chrono::Duration = chrono::Duration::hours(24);

/// One pending CI handoff awaiting pickup-resolution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CiHandoffTrack {
    pub schema_version: u32,
    /// The `next_after_ci` recipient that owes the review.
    pub target: String,
    /// `owner/repo@branch` — the same key the handoff message carries as its
    /// `correlation_id` and that reviewer reports / pr_state events use.
    pub correlation: String,
    /// RFC3339 — when the handoff was enqueued (the re-nudge age anchor).
    pub sent_at: String,
}

fn dir(home: &Path) -> PathBuf {
    home.join("ci-handoff-tracks")
}

/// Filename-safe key: one file per `(target, correlation)`.
fn file_for(home: &Path, target: &str, correlation: &str) -> PathBuf {
    let sanitize = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    dir(home).join(format!(
        "{}--{}.json",
        sanitize(target),
        sanitize(correlation)
    ))
}

/// Record (or refresh — a NEW CI pass on the same branch restarts the age
/// anchor) the pending handoff for `(target, correlation)`.
pub(crate) fn record(home: &Path, target: &str, correlation: &str, sent_at: &str) {
    let track = CiHandoffTrack {
        schema_version: SCHEMA_VERSION,
        target: target.to_string(),
        correlation: correlation.to_string(),
        sent_at: sent_at.to_string(),
    };
    let path = file_for(home, target, correlation);
    if let Err(e) = std::fs::create_dir_all(dir(home))
        .and_then(|()| std::fs::write(&path, serde_json::to_vec(&track).unwrap_or_default()))
    {
        tracing::warn!(%target, %correlation, error = %e, "#1888: ci-handoff track write failed");
        return;
    }
    tracing::info!(
        tag = "#1888-track-recorded",
        agent = %target,
        %correlation,
        "ci-handoff track recorded (re-nudge until resolution, not until read)"
    );
}

/// All currently-pending tracks. Unparseable files are skipped (and logged) —
/// never panic the watchdog tick.
pub(crate) fn list(home: &Path) -> Vec<CiHandoffTrack> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir(home)).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<CiHandoffTrack>(&b).ok())
        {
            Some(t) => out.push(t),
            None => {
                tracing::warn!(path = %path.display(), "#1888: unparseable ci-handoff track skipped");
            }
        }
    }
    out
}

/// Resolve (delete) every track carrying `correlation`, any target. Returns
/// how many were resolved. `reason` is for the log only.
pub(crate) fn resolve_by_correlation(home: &Path, correlation: &str, reason: &str) -> usize {
    let mut resolved = 0;
    for track in list(home) {
        if track.correlation == correlation
            && std::fs::remove_file(file_for(home, &track.target, &track.correlation)).is_ok()
        {
            resolved += 1;
            tracing::info!(
                tag = "#1888-track-resolved",
                agent = %track.target,
                %correlation,
                reason,
                "ci-handoff track resolved"
            );
        }
    }
    resolved
}

/// Resolve every track whose TARGET is `agent` and whose correlation's branch
/// part matches `branch` — the target claiming the branch (worktree binding)
/// is acting on the handoff.
pub(crate) fn resolve_claimed(home: &Path, agent: &str, branch: &str) -> usize {
    let suffix = format!("@{branch}");
    let mut resolved = 0;
    for track in list(home) {
        if track.target == agent
            && track.correlation.ends_with(&suffix)
            && std::fs::remove_file(file_for(home, &track.target, &track.correlation)).is_ok()
        {
            resolved += 1;
            tracing::info!(
                tag = "#1888-track-resolved",
                agent = %track.target,
                correlation = %track.correlation,
                reason = "target_claimed_branch",
                "ci-handoff track resolved"
            );
        }
    }
    resolved
}

/// Backstop sweep: delete tracks older than [`TRACK_MAX_AGE`] (WARN each) so
/// an unresolved correlation can't re-nudge forever. Returns how many were
/// swept. Called from the watchdog tick.
pub(crate) fn sweep_expired(home: &Path, now: &chrono::DateTime<chrono::Utc>) -> usize {
    let mut swept = 0;
    for track in list(home) {
        let expired = chrono::DateTime::parse_from_rfc3339(&track.sent_at)
            .map(|t| now.signed_duration_since(t.with_timezone(&chrono::Utc)) >= TRACK_MAX_AGE)
            // Unparseable sent_at → treat as expired (a broken track must not
            // re-nudge forever either).
            .unwrap_or(true);
        if expired
            && std::fs::remove_file(file_for(home, &track.target, &track.correlation)).is_ok()
        {
            swept += 1;
            tracing::warn!(
                tag = "#1888-track-expired",
                agent = %track.target,
                correlation = %track.correlation,
                sent_at = %track.sent_at,
                "ci-handoff track hit the 24h backstop without a resolution signal — swept (no more re-nudges); the lead escalation already fired long ago"
            );
        }
    }
    swept
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "agend-1888-track-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn record_list_roundtrip_and_refresh() {
        let home = tmp_home("roundtrip");
        record(&home, "reviewer", "o/r@b1", "2026-06-10T00:00:00Z");
        record(&home, "reviewer", "o/r@b2", "2026-06-10T00:00:00Z");
        assert_eq!(list(&home).len(), 2);
        // Re-record same key = refresh (no duplicate file).
        record(&home, "reviewer", "o/r@b1", "2026-06-10T01:00:00Z");
        let tracks = list(&home);
        assert_eq!(tracks.len(), 2, "refresh must not duplicate");
        assert!(tracks
            .iter()
            .any(|t| t.correlation == "o/r@b1" && t.sent_at == "2026-06-10T01:00:00Z"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_by_correlation_clears_all_targets() {
        let home = tmp_home("resolve-corr");
        record(&home, "reviewer", "o/r@b", "2026-06-10T00:00:00Z");
        record(&home, "reviewer-2", "o/r@b", "2026-06-10T00:00:00Z");
        record(&home, "reviewer", "o/r@other", "2026-06-10T00:00:00Z");
        assert_eq!(resolve_by_correlation(&home, "o/r@b", "test"), 2);
        let left = list(&home);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].correlation, "o/r@other");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_claimed_scopes_to_target_and_branch() {
        let home = tmp_home("resolve-claim");
        record(&home, "reviewer", "o/r@fix/x", "2026-06-10T00:00:00Z");
        record(&home, "other", "o/r@fix/x", "2026-06-10T00:00:00Z");
        assert_eq!(resolve_claimed(&home, "reviewer", "fix/x"), 1);
        let left = list(&home);
        assert_eq!(left.len(), 1, "other target's track untouched");
        assert_eq!(left[0].target, "other");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn sweep_expired_backstop_and_broken_tracks() {
        let home = tmp_home("sweep");
        let now = chrono::Utc::now();
        let old = (now - chrono::Duration::hours(25)).to_rfc3339();
        let fresh = now.to_rfc3339();
        record(&home, "reviewer", "o/r@old", &old);
        record(&home, "reviewer", "o/r@fresh", &fresh);
        record(&home, "reviewer", "o/r@broken", "not-a-timestamp");
        assert_eq!(sweep_expired(&home, &now), 2, "old + broken swept");
        let left = list(&home);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].correlation, "o/r@fresh");
        std::fs::remove_dir_all(&home).ok();
    }
}
