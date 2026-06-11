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
    /// #2008: the branch head (`pr.current_sha`) at record time. `#[serde(default)]`
    /// → a pre-#2008 track reads back as `None`, which [`resolve_head_advanced`]
    /// treats as "head-unknown, do not invalidate" (backward-compatible — the
    /// other resolve exits + 24h backstop still apply). Additive (COMPATIBILITY
    /// tier-b) — no `schema_version` bump.
    #[serde(default)]
    pub head_sha: Option<String>,
}

fn dir(home: &Path) -> PathBuf {
    home.join("ci-handoff-tracks")
}

/// Filename-safe key: one file per `(target, correlation)`. #1969: a trailing
/// `--<8 hex of sha256(target\0correlation)>` disambiguates keys that the lossy
/// sanitize would otherwise collapse to the SAME name (e.g. `a/b` vs `a_b` both
/// sanitize to `a_b`), so distinct keys never share a track / lock / tmp file.
/// The sanitized parts stay for operator readability; the hash guarantees
/// injectivity. (NOTE: `resolve`/`sweep` delete the ACTUAL `list()`-supplied
/// path, not a `file_for` reconstruction — #1969 (X) — so a track written under
/// an OLDER filename encoding is still resolvable after this change; `file_for`
/// only names NEW writes + the per-key lock.)
fn file_for(home: &Path, target: &str, correlation: &str) -> PathBuf {
    dir(home).join(format!(
        "{}--{}--{}.json",
        sanitize_component(target),
        sanitize_component(correlation),
        key_hash(target, correlation)
    ))
}

/// Map an arbitrary key part to a filename-safe, human-readable component.
/// Lossy (collisions possible) — `file_for` adds [`key_hash`] for injectivity.
fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// #1969: 8-hex sha256 of the RAW (un-sanitized) `(target, correlation)`, joined
/// with a NUL byte (which can't appear in an agent name or `owner/repo@branch`)
/// so the digest is an injective per-key disambiguator for [`file_for`]. sha2 is
/// already a dependency (blake3 is not) — 32 bits is ample for the handful of
/// in-flight CI handoffs, and it only has to break ties the sanitize created.
fn key_hash(target: &str, correlation: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(target.as_bytes());
    h.update([0u8]);
    h.update(correlation.as_bytes());
    hex::encode(&h.finalize()[..4])
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
fn remove_if_unchanged(
    home: &Path,
    path: &Path,
    target: &str,
    correlation: &str,
    expect_sent_at: &str,
) -> bool {
    let Ok(_lock) = crate::store::acquire_file_lock(&lock_for(home, target, correlation)) else {
        return false;
    };
    // #1969 (X): re-read + remove the ACTUAL `list()`-supplied `path`, not a
    // `file_for` reconstruction — so a track written under an older filename
    // encoding stays resolvable. The per-key lock (keyed by target/correlation,
    // encoding-independent) still serializes this against `record`'s write.
    let current = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<CiHandoffTrack>(&b).ok());
    match current {
        Some(t) if t.sent_at == expect_sent_at => std::fs::remove_file(path).is_ok(),
        // re-recorded (new sent_at) / torn / absent → leave it (the fresh episode
        // must keep its track; a torn read self-heals on the next tick).
        _ => false,
    }
}

/// Record (or refresh — a NEW CI pass on the same branch restarts the age
/// anchor) the pending handoff for `(target, correlation)`.
pub(crate) fn record(
    home: &Path,
    target: &str,
    correlation: &str,
    sent_at: &str,
    head_sha: Option<&str>,
) {
    let track = CiHandoffTrack {
        schema_version: SCHEMA_VERSION,
        target: target.to_string(),
        correlation: correlation.to_string(),
        sent_at: sent_at.to_string(),
        head_sha: head_sha.map(String::from),
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

/// All currently-pending tracks, each paired with its ACTUAL on-disk path.
/// Unparseable files are skipped (and logged) — never panic the watchdog tick.
/// #1969 (X): callers delete the supplied path directly (no `file_for`
/// reconstruction), so a track under an older filename encoding stays resolvable.
pub(crate) fn list(home: &Path) -> Vec<(PathBuf, CiHandoffTrack)> {
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
            Some(t) => out.push((path, t)),
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
    for (path, track) in list(home) {
        if track.correlation == correlation
            && remove_if_unchanged(
                home,
                &path,
                &track.target,
                &track.correlation,
                &track.sent_at,
            )
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
    for (path, track) in list(home) {
        if track.target == agent
            && track.correlation.ends_with(&suffix)
            && remove_if_unchanged(
                home,
                &path,
                &track.target,
                &track.correlation,
                &track.sent_at,
            )
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

/// #2008: resolve every track for `correlation` whose recorded `head_sha` no
/// longer matches the branch's CURRENT head — the ci-ready obligation was for a
/// head that has since been superseded (a new push / force-push), so without this
/// the handoff watchdog re-nudges a dead head every ~2 min until merge or the 24h
/// backstop (the operator-observed renudge loop, #2008). A track with NO recorded
/// `head_sha` (written before #2008) is LEFT ALONE — we can't tell whether it is
/// stale, so the pre-#2008 behavior (other resolve exits + 24h backstop) is
/// preserved (no mis-kill on upgrade). This ADDS a resolve condition; it
/// introduces no new re-send path. Returns how many were resolved. Called from
/// the ci-watch poll, which already holds the current head.
///
/// KNOWN LIMITATION (codex #2013 review): this is driven from the ci-watch poll,
/// so a branch whose watch is not being polled gets no head-advanced cleanup —
/// notably a watch the poller skips (e.g. no subscribers and no `next_after_ci`).
/// Such a stale track falls back to the OTHER resolve exits (report arrival /
/// branch claim / PR-terminal) and the 24h backstop. The head-advanced path is an
/// optimization for the common (actively-polled) case, not the sole guarantee.
pub(crate) fn resolve_head_advanced(home: &Path, correlation: &str, current_head: &str) -> usize {
    let mut resolved = 0;
    for (path, track) in list(home) {
        let head_advanced = matches!(&track.head_sha, Some(h) if h != current_head);
        if track.correlation == correlation
            && head_advanced
            && remove_if_unchanged(
                home,
                &path,
                &track.target,
                &track.correlation,
                &track.sent_at,
            )
        {
            resolved += 1;
            tracing::info!(
                tag = "#2008-track-head-advanced",
                agent = %track.target,
                correlation = %track.correlation,
                recorded_head = ?track.head_sha,
                current_head = %current_head,
                "ci-handoff track resolved — branch head advanced past the recorded head; stale ci-ready obligation cleared (no more re-nudges for a dead head)"
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
    for (path, track) in list(home) {
        let expired = chrono::DateTime::parse_from_rfc3339(&track.sent_at)
            .map(|t| now.signed_duration_since(t.with_timezone(&chrono::Utc)) >= TRACK_MAX_AGE)
            // Unparseable sent_at → treat as expired (a broken track must not
            // re-nudge forever either).
            .unwrap_or(true);
        if expired
            && remove_if_unchanged(
                home,
                &path,
                &track.target,
                &track.correlation,
                &track.sent_at,
            )
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

/// #1969: passive hygiene GC for the orphan sidecars the per-key-lock design
/// leaves behind. Every key keeps a 0-byte `<key>.json.lock` that is
/// deliberately NEVER unlinked on resolve — ci-handoff keys are REUSED (a new CI
/// pass on the same branch), and unlink-on-resolve would open a
/// flock-on-unlinked-inode double-hold race (the #1968 review's reason NOT to
/// copy `dispatch_idle`'s `delete_sidecar_locked`). A crashed `record` can also
/// leave a `<key>.json.tmp`. Hooked into the hourly retention sweep; returns the
/// count removed.
///
/// SAFETY — never mis-delete a LIVE lock: `acquire_file_lock` opens with
/// `create + truncate(false)` and never writes, so a `.lock`'s mtime is its
/// CREATE time, NOT last-use — a long-lived branch's lock can be old yet live. So
/// a `.lock` is removed ONLY when it is an ORPHAN (no sibling `.json` track → no
/// pending handoff for that key) AND older than [`LOCK_ORPHAN_MIN_AGE`] (a track
/// resolves within the 24h backstop, so a track-less lock past 48h is settled).
/// A `.tmp` is a transient write artifact created under the lock, so any `.tmp`
/// past [`TMP_MIN_AGE`] is a crash leftover (the next `record` overwrites it
/// regardless). `.json` tracks are NEVER touched here — `resolve_*` /
/// `sweep_expired` own their lifecycle.
pub(crate) fn gc_orphan_sidecars(home: &Path, now: std::time::SystemTime) -> usize {
    /// A track-less `.lock` older than this is a settled orphan (tracks resolve
    /// within the 24h backstop, so 48h is a safe margin past any live handoff).
    const LOCK_ORPHAN_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(48 * 3600);
    /// A `.tmp` is only ever a transient under-lock write; older than this = a
    /// crashed `record` leftover.
    const TMP_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

    let mut removed = 0;
    for entry in std::fs::read_dir(dir(home)).into_iter().flatten().flatten() {
        let path = entry.path();
        let min_age = match path.extension().and_then(|e| e.to_str()) {
            Some("lock") => LOCK_ORPHAN_MIN_AGE,
            Some("tmp") => TMP_MIN_AGE,
            // never touch the `.json` tracks (resolve/sweep_expired own them).
            _ => continue,
        };
        let age = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|mt| now.duration_since(mt).ok())
            .unwrap_or(std::time::Duration::ZERO);
        if age < min_age {
            continue;
        }
        // A `.lock` is removed ONLY if its `<key>.json` track is gone (orphan) —
        // a present track means the key is live, and its lock (old-mtime but
        // in-use) must not be unlinked under a concurrent flock.
        if path.extension().and_then(|e| e.to_str()) == Some("lock")
            && path.with_extension("").exists()
        {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
            tracing::debug!(
                tag = "#1969-sidecar-gc",
                path = %path.display(),
                "ci-handoff orphan sidecar swept"
            );
        }
    }
    removed
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
        record(&home, "reviewer", "o/r@b1", "2026-06-10T00:00:00Z", None);
        record(&home, "reviewer", "o/r@b2", "2026-06-10T00:00:00Z", None);
        assert_eq!(list(&home).len(), 2);
        // Re-record same key = refresh (no duplicate file).
        record(&home, "reviewer", "o/r@b1", "2026-06-10T01:00:00Z", None);
        let tracks = list(&home);
        assert_eq!(tracks.len(), 2, "refresh must not duplicate");
        assert!(tracks
            .iter()
            .any(|(_, t)| t.correlation == "o/r@b1" && t.sent_at == "2026-06-10T01:00:00Z"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_by_correlation_clears_all_targets() {
        let home = tmp_home("resolve-corr");
        record(&home, "reviewer", "o/r@b", "2026-06-10T00:00:00Z", None);
        record(&home, "reviewer-2", "o/r@b", "2026-06-10T00:00:00Z", None);
        record(&home, "reviewer", "o/r@other", "2026-06-10T00:00:00Z", None);
        assert_eq!(resolve_by_correlation(&home, "o/r@b", "test"), 2);
        let left = list(&home);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].1.correlation, "o/r@other");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_claimed_scopes_to_target_and_branch() {
        let home = tmp_home("resolve-claim");
        record(&home, "reviewer", "o/r@fix/x", "2026-06-10T00:00:00Z", None);
        record(&home, "other", "o/r@fix/x", "2026-06-10T00:00:00Z", None);
        assert_eq!(resolve_claimed(&home, "reviewer", "fix/x"), 1);
        let left = list(&home);
        assert_eq!(left.len(), 1, "other target's track untouched");
        assert_eq!(left[0].1.target, "other");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn sweep_expired_backstop_and_broken_tracks() {
        let home = tmp_home("sweep");
        let now = chrono::Utc::now();
        let old = (now - chrono::Duration::hours(25)).to_rfc3339();
        let fresh = now.to_rfc3339();
        record(&home, "reviewer", "o/r@old", &old, None);
        record(&home, "reviewer", "o/r@fresh", &fresh, None);
        record(&home, "reviewer", "o/r@broken", "not-a-timestamp", None);
        assert_eq!(sweep_expired(&home, &now), 2, "old + broken swept");
        let left = list(&home);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].1.correlation, "o/r@fresh");
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
        record(&home, "reviewer", "o/r@b", "2026-06-10T00:00:00Z", None);
        let path = file_for(&home, "reviewer", "o/r@b");
        // Same episode → delete succeeds.
        assert!(remove_if_unchanged(
            &home,
            &path,
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
        record(&home, "reviewer", "o/r@b", "2026-06-10T00:00:00Z", None); // S1
        record(&home, "reviewer", "o/r@b", "2026-06-10T09:00:00Z", None); // S2
        assert!(
            !remove_if_unchanged(&home, &path, "reviewer", "o/r@b", "2026-06-10T00:00:00Z"),
            "#1963: a delete carrying the STALE listed sent_at must be refused"
        );
        let tracks = list(&home);
        assert_eq!(tracks.len(), 1, "the fresh episode's track must survive");
        assert_eq!(
            tracks[0].1.sent_at, "2026-06-10T09:00:00Z",
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
        record(&home, "reviewer", "o/r@b", "2026-06-10T00:00:00Z", None);
        let tracks = list(&home);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].1.correlation, "o/r@b");
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

    /// #1969: two keys the lossy sanitize collapses to the same name (`a/b` vs
    /// `a_b` both → `a_b`) must map to DISTINCT files (the `key_hash` suffix), so
    /// one key's track can never clobber the other's.
    #[test]
    fn sanitize_colliding_keys_get_distinct_files_1969() {
        let home = tmp_home("collide");
        record(&home, "reviewer", "o/r@a/b", "2026-06-10T00:00:00Z", None);
        record(&home, "reviewer", "o/r@a_b", "2026-06-10T00:00:00Z", None);
        let tracks = list(&home);
        assert_eq!(
            tracks.len(),
            2,
            "#1969: sanitize-colliding keys must NOT share a file"
        );
        let corrs: Vec<_> = tracks.iter().map(|(_, t)| t.correlation.as_str()).collect();
        assert!(
            corrs.contains(&"o/r@a/b") && corrs.contains(&"o/r@a_b"),
            "both distinct keys present: {corrs:?}"
        );
        assert_ne!(
            file_for(&home, "reviewer", "o/r@a/b"),
            file_for(&home, "reviewer", "o/r@a_b"),
            "the two keys' files differ by the hash suffix"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1969: the orphan-sidecar GC removes a track-less old `.lock` + a crash
    /// `.tmp`, but must NEVER remove a LIVE key's lock — even when its mtime is
    /// old (the lock's mtime is its CREATE time, not last-use). The presence of
    /// the sibling `.json` track is the liveness guard.
    #[test]
    fn gc_orphan_sidecars_keeps_live_lock_removes_orphans_1969() {
        use std::time::{Duration, SystemTime};
        let home = tmp_home("gc");
        // (1) LIVE key → a `.json` track + its `.lock`.
        record(
            &home,
            "reviewer",
            "o/r@active",
            "2026-06-10T00:00:00Z",
            None,
        );
        let active_lock = lock_for(&home, "reviewer", "o/r@active");
        assert!(active_lock.exists());
        // (2) ORPHAN lock — no `.json` track (resolved long ago).
        let orphan_lock = lock_for(&home, "reviewer", "o/r@orphan");
        std::fs::write(&orphan_lock, b"").unwrap();
        // (3) crash `.tmp` leftover.
        let crash = file_for(&home, "reviewer", "o/r@crash");
        let mut tmp_name = crash.file_name().unwrap().to_os_string();
        tmp_name.push(".tmp");
        let crash_tmp = crash.with_file_name(tmp_name);
        std::fs::write(&crash_tmp, b"{partial").unwrap();

        // Sweep with `now` far in the future so every file clears the age window.
        let future = SystemTime::now() + Duration::from_secs(100 * 3600);
        let removed = gc_orphan_sidecars(&home, future);

        assert!(
            active_lock.exists(),
            "#1969: a LIVE key's lock (has a .json track) must NOT be GC'd even when old"
        );
        assert!(
            !orphan_lock.exists(),
            "#1969: an orphan lock past the age window must be GC'd"
        );
        assert!(
            !crash_tmp.exists(),
            "#1969: a crash .tmp past the age window must be GC'd"
        );
        assert_eq!(
            list(&home).len(),
            1,
            "the live .json track is never touched by the sidecar GC"
        );
        assert_eq!(removed, 2, "orphan lock + crash tmp");
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1969: a FRESH orphan `.lock` (within the age window) is kept — only
    /// settled orphans are GC'd, so a lock created moments before a re-record is
    /// not whipped out from under it.
    #[test]
    fn gc_respects_age_window_1969() {
        let home = tmp_home("gc-age");
        std::fs::create_dir_all(dir(&home)).unwrap();
        let fresh_orphan = lock_for(&home, "reviewer", "o/r@fresh-orphan");
        std::fs::write(&fresh_orphan, b"").unwrap();
        assert_eq!(
            gc_orphan_sidecars(&home, std::time::SystemTime::now()),
            0,
            "#1969: a fresh orphan lock is within the age window → kept"
        );
        assert!(fresh_orphan.exists());
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1969 (X) — the migration-safety keystone: a track written under the OLD
    /// filename encoding (no `key_hash` suffix) must still be resolvable, because
    /// `resolve_*` delete the ACTUAL `list()`-supplied path, NOT a `file_for`
    /// reconstruction (which would now compute the new hashed name and miss it).
    #[test]
    fn old_encoding_track_still_resolvable_1969() {
        let home = tmp_home("migrate");
        std::fs::create_dir_all(dir(&home)).unwrap();
        // Pre-#1969 name format: `<target>--<corr>.json`, no hash suffix.
        let old_path = dir(&home).join("reviewer--o_r_b.json");
        let track = CiHandoffTrack {
            schema_version: SCHEMA_VERSION,
            target: "reviewer".into(),
            correlation: "o/r@b".into(),
            sent_at: "2026-06-10T00:00:00Z".into(),
            head_sha: None,
        };
        std::fs::write(&old_path, serde_json::to_vec(&track).unwrap()).unwrap();
        assert_eq!(
            list(&home).len(),
            1,
            "list() reads the old-encoding file by content"
        );
        assert_eq!(
            resolve_by_correlation(&home, "o/r@b", "migration"),
            1,
            "#1969 (X): an old-encoding track must still resolve (delete the listed path)"
        );
        assert!(list(&home).is_empty(), "the old-encoding track is gone");
        assert!(!old_path.exists());
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #2008: head-aware invalidation ──────────────────────────────────

    /// #2008 §3.9: the branch head advanced past what the track recorded → the
    /// stale ci-ready obligation is resolved, so the watchdog stops re-nudging.
    #[test]
    fn head_advanced_resolves_track() {
        let home = tmp_home("head-adv");
        record(
            &home,
            "reviewer",
            "o/r@b",
            "2026-06-10T00:00:00Z",
            Some("HEAD_OLD"),
        );
        assert_eq!(list(&home).len(), 1);
        let resolved = resolve_head_advanced(&home, "o/r@b", "HEAD_NEW");
        assert_eq!(
            resolved, 1,
            "a track for a superseded head must be resolved"
        );
        assert!(
            list(&home).is_empty(),
            "no track remains to re-nudge a dead head"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2008 §3.9: the head has NOT moved → the obligation is still live, keep the
    /// track (the normal re-nudge / resolve exits still apply).
    #[test]
    fn head_unchanged_keeps_track() {
        let home = tmp_home("head-same");
        record(
            &home,
            "reviewer",
            "o/r@b",
            "2026-06-10T00:00:00Z",
            Some("HEAD_X"),
        );
        let resolved = resolve_head_advanced(&home, "o/r@b", "HEAD_X");
        assert_eq!(resolved, 0, "an unchanged head must keep the track");
        assert_eq!(list(&home).len(), 1, "the live track remains");
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2008 §3.9 backward-compat: a pre-#2008 track (no `head_sha` field on disk)
    /// must NOT be invalidated by a head move — we can't tell if it is stale, so
    /// preserve the old behavior (other resolve exits + 24h backstop still apply).
    #[test]
    fn pre_2008_track_without_head_sha_not_invalidated() {
        let home = tmp_home("oldfield");
        std::fs::create_dir_all(dir(&home)).unwrap();
        // A pre-#2008 track on disk: the `head_sha` field is absent entirely.
        std::fs::write(
            dir(&home).join("legacy.json"),
            r#"{"schema_version":1,"target":"reviewer","correlation":"o/r@b","sent_at":"2026-06-10T00:00:00Z"}"#,
        )
        .unwrap();
        let resolved = resolve_head_advanced(&home, "o/r@b", "any-new-head");
        assert_eq!(
            resolved, 0,
            "a pre-#2008 track (no head_sha) must NOT be invalidated by a head move"
        );
        assert_eq!(
            list(&home).len(),
            1,
            "the legacy track survives (no mis-kill on upgrade)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2008: invalidation is scoped to the matching correlation — a head move on
    /// one branch must not resolve another branch's track.
    #[test]
    fn head_advanced_only_affects_matching_correlation() {
        let home = tmp_home("head-corr");
        record(
            &home,
            "reviewer",
            "o/r@b1",
            "2026-06-10T00:00:00Z",
            Some("HEAD_OLD"),
        );
        record(
            &home,
            "reviewer",
            "o/r@b2",
            "2026-06-10T00:00:00Z",
            Some("HEAD_OLD"),
        );
        let resolved = resolve_head_advanced(&home, "o/r@b1", "HEAD_NEW");
        assert_eq!(resolved, 1, "only b1's track is for the moved head");
        let remaining = list(&home);
        assert_eq!(remaining.len(), 1, "b2's track is untouched");
        assert!(remaining.iter().all(|(_, t)| t.correlation == "o/r@b2"));
        std::fs::remove_dir_all(&home).ok();
    }
}
