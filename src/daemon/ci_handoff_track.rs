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
//!
//! ## #1963 concurrency hardening (per-key lock + atomic write)
//! #1888 originally took NO per-file locks, on the theory that whole-file
//! write/delete with no read-modify-write was race-free. The #1960 review found
//! a real (rare, non-blocking, no-data-loss) TOCTOU: `resolve`/`sweep` did
//! `list()` then `remove_file(path)`, so a `record` re-recording the SAME key
//! (a new CI pass) BETWEEN the list and the remove had its fresh track deleted
//! → that episode's re-nudge + escalation were lost. #1963 closes it two ways.
//!
//! `record` is now an ATOMIC write (`write(<key>.json.tmp)` → `rename`); rename
//! is atomic on one filesystem, so the lock-free `list()` (every watchdog tick)
//! and `resolve`'s re-read never observe a half-written track (closes the
//! torn-read skip). And every deleter (`resolve_*`, `sweep_expired`) deletes
//! UNDER a per-key `<key>.lock` after re-reading and confirming the on-disk
//! `sent_at` still matches the episode it listed (delete-if-unchanged). `record`
//! takes the SAME per-key lock around its atomic write, so a `record` cannot
//! interleave between a deleter's re-read and its remove → no TOCTOU, no new race.
//!
//! The lock is a SEPARATE `<key>.lock` sidecar (NOT the track file): the atomic
//! write renames the track's inode, so a flock on the track file itself would be
//! silently lost across the rename. `list()` stays lock-free read-only (the
//! atomic write is what guarantees it never torn-reads). Track count is tiny
//! (in-flight CI handoffs), the lock hold is a single file op, so the per-tick
//! sweep's per-track acquire/release is negligible.

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

/// #1963: per-key lock sidecar — a SEPARATE file from the track. `record`'s
/// atomic write renames the track's inode, so a flock taken on the track file
/// itself would be silently lost across the rename. The `.lock` suffix also
/// keeps it out of `list()` (which filters to the `.json` extension).
fn lock_for(home: &Path, target: &str, correlation: &str) -> PathBuf {
    let track = file_for(home, target, correlation);
    let mut name = track
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".lock");
    track.with_file_name(name)
}

/// #1963: atomic whole-file write (write a sibling `.tmp`, then `rename` over the
/// target). `rename` is atomic on one filesystem, so the lock-free `list()` and
/// `resolve`'s re-read never observe a half-written track. The caller holds the
/// per-key lock, so the fixed `.tmp` name can't collide with a concurrent
/// `record` of the same key. The `.tmp` extension keeps a crash-leftover out of
/// `list()`; the next `record` overwrites it.
fn atomic_write_track(path: &Path, track: &CiHandoffTrack) -> std::io::Result<()> {
    let mut tmp_name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, serde_json::to_vec(track).unwrap_or_default())?;
    std::fs::rename(&tmp, path)
}

/// #1963: delete the `(target, correlation)` track ONLY if its on-disk `sent_at`
/// still equals `expect_sent_at` — i.e. it is the same episode the caller listed,
/// not a newer one a concurrent `record` re-wrote (a new CI pass restarts the age
/// anchor → a new `sent_at`). The re-read AND the remove run UNDER the per-key
/// lock that `record` also takes, so a `record` cannot interleave between them →
/// no TOCTOU. Returns true iff a matching track was removed. Lock-acquire failure
/// → `false` (skip the delete rather than risk an unsynchronized one — a missed
/// resolution just leaves the track for the next signal / 24h backstop).
fn remove_if_unchanged(home: &Path, target: &str, correlation: &str, expect_sent_at: &str) -> bool {
    let Ok(_lock) = crate::store::acquire_file_lock(&lock_for(home, target, correlation)) else {
        return false;
    };
    let path = file_for(home, target, correlation);
    let current = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<CiHandoffTrack>(&b).ok());
    match current {
        Some(t) if t.sent_at == expect_sent_at => std::fs::remove_file(&path).is_ok(),
        // re-recorded (new sent_at) / torn / absent → leave it (the fresh episode
        // must keep its track; a torn read self-heals on the next tick).
        _ => false,
    }
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
    if let Err(e) = std::fs::create_dir_all(dir(home)) {
        tracing::warn!(%target, %correlation, error = %e, "#1888: ci-handoff track dir create failed");
        return;
    }
    // #1963: take the per-key lock so this write is atomic w.r.t. a concurrent
    // resolve/sweep of the same key (delete-if-unchanged) — the resolution can't
    // delete this fresh episode mid-write. Lock-acquire failure → degrade to an
    // unlocked atomic write (rare fs error; still torn-read-safe via rename).
    let _lock = crate::store::acquire_file_lock(&lock_for(home, target, correlation)).ok();
    if let Err(e) = atomic_write_track(&path, &track) {
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
            && remove_if_unchanged(home, &track.target, &track.correlation, &track.sent_at)
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
            && remove_if_unchanged(home, &track.target, &track.correlation, &track.sent_at)
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
        if expired && remove_if_unchanged(home, &track.target, &track.correlation, &track.sent_at) {
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

    /// #1963: the record-vs-remove TOCTOU fix. A deleter that LISTED an old
    /// episode (sent_at S1) must NOT delete a track a concurrent `record`
    /// re-wrote to a NEW episode (S2) — `remove_if_unchanged` re-reads under the
    /// per-key lock and only deletes when the on-disk `sent_at` still matches what
    /// was listed. Models the #1960 race (list sees S1, record writes S2, remove)
    /// at the exact decision point.
    #[test]
    fn remove_if_unchanged_refuses_stale_delete_1963() {
        let home = tmp_home("cas");
        record(&home, "reviewer", "o/r@b", "2026-06-10T00:00:00Z");
        // Same episode → delete succeeds.
        assert!(remove_if_unchanged(
            &home,
            "reviewer",
            "o/r@b",
            "2026-06-10T00:00:00Z"
        ));
        assert!(
            list(&home).is_empty(),
            "a matching-sent_at delete removes the track"
        );

        // Re-record a NEW episode (new sent_at = a new CI pass), then a deleter
        // carrying the STALE listed sent_at must NOT delete it (the race).
        record(&home, "reviewer", "o/r@b", "2026-06-10T00:00:00Z"); // S1
        record(&home, "reviewer", "o/r@b", "2026-06-10T09:00:00Z"); // S2
        assert!(
            !remove_if_unchanged(&home, "reviewer", "o/r@b", "2026-06-10T00:00:00Z"),
            "#1963: a delete carrying the STALE listed sent_at must be refused"
        );
        let tracks = list(&home);
        assert_eq!(tracks.len(), 1, "the fresh episode's track must survive");
        assert_eq!(
            tracks[0].sent_at, "2026-06-10T09:00:00Z",
            "the SURVIVING track is the new episode (S2)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1963: `record` writes atomically (tmp→rename) and the `.lock` / `.tmp`
    /// sidecars are invisible to `list()` — so the lock-free watchdog scan never
    /// sees a partial track or a stray sidecar. The torn-read self-heal is
    /// structural: a reader sees either the old or the new COMPLETE track, never a
    /// half-write (so a torn read can't even occur).
    #[test]
    fn atomic_write_and_sidecars_excluded_from_list_1963() {
        let home = tmp_home("atomic");
        record(&home, "reviewer", "o/r@b", "2026-06-10T00:00:00Z");
        let tracks = list(&home);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].correlation, "o/r@b");
        // The per-key lock sidecar exists (record took the lock) but is NOT a track.
        assert!(
            lock_for(&home, "reviewer", "o/r@b").exists(),
            "per-key lock sidecar present"
        );
        // A stray `.tmp` (simulating a crash mid-write) must be ignored by list().
        let track = file_for(&home, "reviewer", "o/r@b");
        let mut tmp_name = track.file_name().unwrap().to_os_string();
        tmp_name.push(".tmp");
        std::fs::write(track.with_file_name(tmp_name), b"{partial").unwrap();
        assert_eq!(
            list(&home).len(),
            1,
            "#1963: .tmp / .lock sidecars must not appear as tracks"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
