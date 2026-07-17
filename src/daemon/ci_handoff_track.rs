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
/// Grace after a handoff was recorded before a processed inbox row may be used
/// as crash-recovery evidence. This leaves the normal explicit resolver first
/// chance and avoids racing a just-written row/track pair.
pub(crate) const TRACK_RECONCILE_GRACE: chrono::Duration = chrono::Duration::seconds(30);

/// One pending CI handoff awaiting pickup-resolution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CiHandoffTrack {
    pub schema_version: u32,
    /// The `next_after_ci` recipient that owes the review.
    pub target: String,
    /// `owner/repo@branch` — the same key the handoff message carries as its
    /// `correlation_id` and that reviewer reports / pr_state events use.
    pub correlation: String,
    /// Opaque identity shared with the durable ci-ready inbox row. Missing on
    /// legacy tracks; protected settlement must fail closed in that case.
    #[serde(default)]
    pub ci_handoff_episode: Option<String>,
    /// Protected/feature class shared with the inbox row. Missing on legacy
    /// tracks; callers must not infer it from a branch string.
    #[serde(default)]
    pub ci_handoff_class: Option<crate::inbox::CiHandoffClass>,
    /// RFC3339 — when the handoff was enqueued (the re-nudge age anchor).
    pub sent_at: String,
    /// #2008: the branch head (`pr.current_sha`) at record time. `#[serde(default)]`
    /// → a pre-#2008 track reads back as `None`, which [`resolve_head_advanced`]
    /// treats as "head-unknown, do not invalidate" (backward-compatible — the
    /// other resolve exits + 24h backstop still apply). Additive (COMPATIBILITY
    /// tier-b) — no `schema_version` bump.
    #[serde(default)]
    pub head_sha: Option<String>,
    /// #2412-follow-up (ci-handoff correlation convention split): the fleet
    /// dispatch's own `t-...` task id, when the handoff's `next_after_ci`
    /// recipient was reached via a dispatch that carries one (the poller's
    /// `state.task_id`). A standard `kind=report` in this fleet carries
    /// `correlation_id=t-...` (Sprint 58 W4 PR-1), NOT `repo@branch` — so
    /// [`resolve_by_correlation`] matching only `correlation` made that the
    /// common case a permanent no-op (`messaging.rs`'s `track_dispatch`
    /// forwards every report's correlation here uninspected). Matching EITHER
    /// key closes it without touching `resolve_claimed`/`resolve_head_advanced`/
    /// the 24h sweep, whose pinned tests assume `correlation` alone.
    /// `#[serde(default)]` → a pre-fix track reads back as `None` (matches
    /// nothing extra, unchanged behavior).
    #[serde(default)]
    pub task_id: Option<String>,
    /// #35896-11 ⑥: the last time the `handoff_timeout_watchdog` re-nudged /
    /// escalated this `(target, correlation)`, persisted DURABLY on the track so a
    /// daemon RESTART doesn't reset the in-mem throttle map and re-fire a burst for
    /// every live handoff on boot. RFC3339. The watchdog's in-mem map stays the
    /// primary (fast) throttle; these are consulted ONLY as the fallback when the
    /// map lacks the key (post-restart). A fresh [`record`] (new CI pass) leaves
    /// them `None` → the new episode is un-throttled (correct: a genuinely new
    /// handoff should renudge on its own schedule). `#[serde(default)]` → a pre-⑥
    /// track reads back `None` = never-nudged/never-escalated = the pre-⑥ behavior
    /// (the first post-restart tick nudges once, then persists the throttle).
    #[serde(default)]
    pub last_renudged_at: Option<String>,
    #[serde(default)]
    pub last_escalated_at: Option<String>,
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

/// Episode-aware delete guard for protected settlement. Matching the durable
/// episode as well as `sent_at` prevents an old ACK from deleting a re-recorded
/// track when both writes happen within the same timestamp tick.
fn remove_if_episode_unchanged(
    home: &Path,
    path: &Path,
    target: &str,
    correlation: &str,
    expect_sent_at: &str,
    expect_episode: &str,
) -> bool {
    let Ok(_lock) = crate::store::acquire_file_lock(&lock_for(home, target, correlation)) else {
        return false;
    };
    let current = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<CiHandoffTrack>(&b).ok());
    match current {
        Some(t)
            if t.sent_at == expect_sent_at
                && t.ci_handoff_episode.as_deref() == Some(expect_episode) =>
        {
            std::fs::remove_file(path).is_ok()
        }
        _ => false,
    }
}

/// Record (or refresh — a NEW CI pass on the same branch restarts the age
/// anchor) the pending handoff for `(target, correlation)`.
#[allow(dead_code)]
pub(crate) fn record(
    home: &Path,
    target: &str,
    correlation: &str,
    sent_at: &str,
    head_sha: Option<&str>,
    task_id: Option<&str>,
) {
    let _ = record_with_identity(
        home,
        target,
        correlation,
        sent_at,
        head_sha,
        task_id,
        None,
        None,
    );
}

/// Record a track with the durable row identity. Returns `true` only after the
/// JSON track is atomically persisted; callers can keep the CI notification
/// cursor retryable if this write fails.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_with_identity(
    home: &Path,
    target: &str,
    correlation: &str,
    sent_at: &str,
    head_sha: Option<&str>,
    task_id: Option<&str>,
    ci_handoff_episode: Option<&str>,
    ci_handoff_class: Option<crate::inbox::CiHandoffClass>,
) -> bool {
    let track = CiHandoffTrack {
        schema_version: SCHEMA_VERSION,
        target: target.to_string(),
        correlation: correlation.to_string(),
        ci_handoff_episode: ci_handoff_episode.map(String::from),
        ci_handoff_class,
        sent_at: sent_at.to_string(),
        head_sha: head_sha.map(String::from),
        task_id: task_id.map(String::from),
        // #35896-11 ⑥: a fresh record (new CI pass) starts un-throttled — the new
        // handoff episode renudges/escalates on its own schedule.
        last_renudged_at: None,
        last_escalated_at: None,
    };
    let path = file_for(home, target, correlation);
    if let Err(e) = std::fs::create_dir_all(dir(home)) {
        tracing::warn!(%target, %correlation, error = %e, "#1888: ci-handoff track dir create failed");
        return false;
    }
    // #1963: take the per-key lock so this write is atomic w.r.t. a concurrent
    // resolve/sweep of the same key (delete-if-unchanged) — the resolution can't
    // delete this fresh episode mid-write. Lock-acquire failure → degrade to an
    // unlocked atomic write (rare fs error; still torn-read-safe via rename).
    let _lock = crate::store::acquire_file_lock(&lock_for(home, target, correlation)).ok();
    if let Err(e) = atomic_write_track(&path, &track) {
        tracing::warn!(%target, %correlation, error = %e, "#1888: ci-handoff track write failed");
        return false;
    }
    tracing::info!(
        tag = "#1888-track-recorded",
        agent = %target,
        %correlation,
        "ci-handoff track recorded (re-nudge until resolution, not until read)"
    );
    true
}

/// Mint an opaque per-delivery episode token. It is deliberately independent
/// of target/correlation so a re-record of the same branch cannot be settled by
/// an old row or delayed ACK.
pub(crate) fn new_episode() -> String {
    format!("ci-episode-{}", uuid::Uuid::new_v4())
}

/// #35896-11 ⑥: persist the watchdog throttle timestamp(s) on an EXISTING track so
/// a daemon restart doesn't reset the in-mem throttle map and re-fire a burst.
/// Called from the watchdog with the ACTUAL `list()`-supplied `path`
/// (encoding-independent, like the resolve/sweep deleters). Read-modify-write UNDER
/// the per-key lock (the same lock `record`/`remove_if_unchanged` take, so it can't
/// interleave with a concurrent record's write or a resolve's delete), preserving
/// every other field incl. `sent_at` — so a concurrent resolve's delete-if-unchanged
/// still matches its episode. No-op if the track was resolved (deleted) concurrently
/// or on any lock/read/write failure: a lost stamp just means one extra renudge
/// after a restart — the failure mode is "slightly noisier", never "obligation lost".
pub(crate) fn stamp_throttle(
    home: &Path,
    path: &Path,
    target: &str,
    correlation: &str,
    now: &chrono::DateTime<chrono::Utc>,
    renudged: bool,
    escalated: bool,
) {
    let Ok(_lock) = crate::store::acquire_file_lock(&lock_for(home, target, correlation)) else {
        return;
    };
    // Re-read the ACTUAL path under the lock (not a `file_for` reconstruction — an
    // older-encoding track stays stampable, mirroring `remove_if_unchanged`).
    let Some(mut track) = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<CiHandoffTrack>(&b).ok())
    else {
        return; // resolved/absent/torn — nothing to throttle (self-heals next tick)
    };
    let stamp = now.to_rfc3339();
    if renudged {
        track.last_renudged_at = Some(stamp.clone());
    }
    if escalated {
        track.last_escalated_at = Some(stamp);
    }
    if let Err(e) = atomic_write_track(path, &track) {
        tracing::warn!(%target, %correlation, error = %e, "#35896-11 ⑥: throttle stamp write failed");
    }
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

/// Resolve (delete) every track carrying `correlation` — matching EITHER the
/// track's `correlation` (`owner/repo@branch`, what a pr_state/reviewer-verdict
/// report carries) OR its `task_id` (`t-...`, what a standard fleet
/// `kind=report` carries per Sprint 58 W4 PR-1 — see the `task_id` field doc
/// for why both keys are needed). Any target. Returns how many were resolved.
/// `reason` is for the log only.
pub(crate) fn resolve_by_correlation(home: &Path, correlation: &str, reason: &str) -> usize {
    let mut resolved = 0;
    for (path, track) in list(home) {
        if (track.correlation == correlation || track.task_id.as_deref() == Some(correlation))
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

/// #35896-11 ①: the dispatcher `from` DELEGATED this ci-ready obligation by
/// dispatching the work (a `kind=task` review) to someone else — that IS their
/// discharge, so resolve `from`'s OWN track. Target-scoped to `from` so a
/// co-subscriber's handoff for the same branch is left intact.
///
/// Matches by `corr` against the track's `task_id` OR `correlation` (PRIMARY
/// path), and opportunistically by the dispatched `branch` — but the branch
/// fallback resolves ONLY when it uniquely identifies a single track (#2667 F2:
/// a bare `@branch` suffix has no repo dimension, so 2+ matches across repos are
/// ambiguous → resolve none by branch, never a cross-repo false-stop).
///
/// **Convention dependency**: the primary path relies on the fleet's
/// review-dispatch convention REUSING the implementer's original task id (never
/// minting a new one) — `record` stores that id as the track's `task_id`
/// (#2412-follow-up, `poller.rs`), so the delegating dispatch's
/// `correlation_id.or(task_id)` matches it. If that convention ever changes, this
/// resolver goes silent and the ci-ready re-nudge falls back to the explicit
/// `inbox action=discharge` gesture + the escalation watchdog (by design, #35896-11
/// Q4 vet). Branch match is opportunistic because dispatches usually omit `branch=`.
pub(crate) fn resolve_delegated(
    home: &Path,
    from: &str,
    corr: Option<&str>,
    branch: Option<&str>,
) -> usize {
    let branch_suffix = branch.filter(|b| !b.is_empty()).map(|b| format!("@{b}"));
    let corr_hit = |t: &CiHandoffTrack| {
        corr.is_some_and(|c| t.task_id.as_deref() == Some(c) || t.correlation == c)
    };
    let branch_hit = |t: &CiHandoffTrack| {
        branch_suffix
            .as_deref()
            .is_some_and(|s| t.correlation.ends_with(s))
    };

    // Target-scoped: only the dispatcher's OWN tracks are eligible (a
    // co-subscriber's handoff for the same branch must survive).
    let own: Vec<(PathBuf, CiHandoffTrack)> = list(home)
        .into_iter()
        .filter(|(_, t)| t.target == from)
        .collect();

    // #2667 F2 (reviewer5, isolation): the branch fallback compares only
    // `ends_with("@{branch}")` — it has NO repo dimension, so a same-named branch
    // in two repos both match. If the dispatcher holds such handoffs in 2+ repos, a
    // single delegation would false-stop ALL of them = silent cross-repo obligation
    // loss. Mirror #2662's exactly-one fail-safe: a branch match is a valid
    // discharge signal ONLY when it uniquely identifies one track; 2+ branch matches
    // = ambiguity → resolve NONE by branch (prefer a missed stop over a wrong one —
    // the precise task_id/full-correlation path and the explicit `inbox
    // action=discharge` gesture back-stop the residual). The corr path stays precise
    // (a task id / full `owner/repo@branch` names exactly one work item).
    let branch_unique = own.iter().filter(|(_, t)| branch_hit(t)).count() == 1;

    let mut resolved = 0;
    for (path, track) in &own {
        let is_hit = corr_hit(track) || (branch_unique && branch_hit(track));
        if is_hit
            && remove_if_unchanged(
                home,
                path,
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
                reason = "dispatcher_delegated",
                "ci-handoff track resolved"
            );
        }
    }
    resolved
}

/// #t-92758 P2: resolve the track whose TARGET is `agent` AND whose correlation
/// is exactly `correlation` (`owner/repo@branch`) — the dismiss path for
/// `ci unwatch`. Unlike [`resolve_by_correlation`] (which clears every target's
/// track for the branch) and [`resolve_claimed`] (which matches by `@branch`
/// suffix across repos), this is the precise "the caller explicitly dropped THIS
/// handoff" eviction: only the unwatching agent's own ci-ready obligation for
/// this exact repo@branch is cleared, leaving any co-subscriber's track intact.
pub(crate) fn resolve_for_target_correlation(home: &Path, agent: &str, correlation: &str) -> usize {
    resolve_for_target_correlation_reason(home, agent, correlation, "unwatch")
}

/// Target/correlation resolver with an explicit audit reason for non-watch
/// callers (for example a feature-branch channel discharge).
pub(crate) fn resolve_for_target_correlation_reason(
    home: &Path,
    agent: &str,
    correlation: &str,
    reason: &str,
) -> usize {
    let mut resolved = 0;
    for (path, track) in list(home) {
        if track.target == agent
            && track.correlation == correlation
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
                reason,
                "ci-handoff track resolved"
            );
        }
    }
    resolved
}

/// Resolve an explicit-discharge legacy handoff without inferring identity
/// for a newer protected episode that reuses the same target/correlation key.
/// Feature and classless tracks retain the pre-episode discharge behavior;
/// protected tracks must use [`resolve_protected_episode`] instead.
pub(crate) fn resolve_legacy_for_target_correlation_reason(
    home: &Path,
    agent: &str,
    correlation: &str,
    reason: &str,
) -> usize {
    let mut resolved = 0;
    for (path, track) in list(home) {
        if track.target == agent
            && track.correlation == correlation
            && track.ci_handoff_class != Some(crate::inbox::CiHandoffClass::Protected)
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
                reason,
                "legacy ci-handoff track resolved"
            );
        }
    }
    resolved
}

/// Resolve one protected handoff only when every durable identity component
/// matches. Legacy/classless/episode-less tracks are deliberately ignored.
pub(crate) fn resolve_protected_episode(
    home: &Path,
    target: &str,
    correlation: &str,
    episode: &str,
    reason: &str,
) -> usize {
    if episode.is_empty() {
        return 0;
    }
    let mut resolved = 0;
    for (path, track) in list(home) {
        if track.target == target
            && track.correlation == correlation
            && track.ci_handoff_episode.as_deref() == Some(episode)
            && track.ci_handoff_class == Some(crate::inbox::CiHandoffClass::Protected)
            && remove_if_episode_unchanged(
                home,
                &path,
                &track.target,
                &track.correlation,
                &track.sent_at,
                episode,
            )
        {
            resolved += 1;
            tracing::info!(
                tag = "#35896-11-track-resolved",
                agent = %target,
                %correlation,
                %episode,
                reason,
                "protected ci-handoff episode resolved"
            );
        }
    }
    resolved
}

/// Reconcile a crash after the inbox row was durably marked processed but
/// before the sidecar delete completed. The inbox probe is exact and runs
/// under its own lock; this function never deletes a missing or ambiguous row.
pub(crate) fn reconcile_processed(home: &Path, now: &chrono::DateTime<chrono::Utc>) -> usize {
    let mut resolved = 0;
    for (path, track) in list(home) {
        if track.ci_handoff_class != Some(crate::inbox::CiHandoffClass::Protected) {
            continue;
        }
        let Some(episode) = track.ci_handoff_episode.as_deref() else {
            continue;
        };
        let Ok(sent_at) = chrono::DateTime::parse_from_rfc3339(&track.sent_at) else {
            continue;
        };
        if now.signed_duration_since(sent_at.with_timezone(&chrono::Utc)) < TRACK_RECONCILE_GRACE {
            continue;
        }
        if !matches!(
            crate::inbox::storage::protected_handoff_row_state(
                home,
                &track.target,
                &track.correlation,
                episode,
            ),
            crate::inbox::storage::ProtectedHandoffRowState::Processed
        ) {
            continue;
        }
        if remove_if_episode_unchanged(
            home,
            &path,
            &track.target,
            &track.correlation,
            &track.sent_at,
            episode,
        ) {
            resolved += 1;
            tracing::info!(
                tag = "#35896-11-track-reconciled",
                agent = %track.target,
                correlation = %track.correlation,
                %episode,
                reason = "ack_reconciled",
                "processed protected ci-handoff episode reconciled"
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

    fn typed_review_report(task_id: &str) -> crate::inbox::InboxMessage {
        let mut msg = crate::inbox::InboxMessage::new_system(
            "gapfix-dev",
            "report",
            "VERIFIED\n\n### Evidence\nran: cargo test --all-targets → passed",
        )
        .with_correlation_id(task_id.to_string());
        msg.report_purpose = crate::review_receipt::ReportPurpose::CodeReview;
        msg.validated_code_review =
            Some(crate::review_receipt::ValidatedCodeReviewReceipt::for_test(
                crate::review_receipt::ReviewReceiptSummary {
                    receipt_id: "review-receipt:m-ci-handoff-test".into(),
                    source_id: "m-ci-handoff-test".into(),
                    evidence_digest: "a".repeat(64),
                    assignment_id: uuid::Uuid::new_v4(),
                    reviewer_instance_id: crate::types::InstanceId::new(),
                    reviewer_name: "gapfix-dev".into(),
                    repo: "owner/repo".into(),
                    pr_number: 1,
                    branch: "branch".into(),
                    task_id: task_id.into(),
                    reviewed_head: "a".repeat(40),
                    review_class: crate::daemon::pr_state::ReviewClass::Single,
                    slot: crate::review_receipt::ReviewSlot::Primary,
                    verdict: crate::review_receipt::ReviewVerdict::Verified,
                },
            ));
        msg
    }

    #[test]
    fn record_list_roundtrip_and_refresh() {
        let home = tmp_home("roundtrip");
        record(
            &home,
            "reviewer",
            "o/r@b1",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
        record(
            &home,
            "reviewer",
            "o/r@b2",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
        assert_eq!(list(&home).len(), 2);
        // Re-record same key = refresh (no duplicate file).
        record(
            &home,
            "reviewer",
            "o/r@b1",
            "2026-06-10T01:00:00Z",
            None,
            None,
        );
        let tracks = list(&home);
        assert_eq!(tracks.len(), 2, "refresh must not duplicate");
        assert!(tracks
            .iter()
            .any(|(_, t)| t.correlation == "o/r@b1" && t.sent_at == "2026-06-10T01:00:00Z"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_delegated_matches_reused_task_id_and_is_target_scoped() {
        let home = tmp_home("delegated");
        // Lead holds a ci-ready handoff; the track records the implementer's
        // original task id (the id the review dispatch REUSES, #2412-follow-up).
        record(
            &home,
            "lead",
            "o/r@fix/x",
            "2026-07-06T00:00:00Z",
            None,
            Some("t-orig-1"),
        );
        // A co-subscriber holds a track for the SAME branch — must survive.
        record(
            &home,
            "reviewer2",
            "o/r@fix/x",
            "2026-07-06T00:00:00Z",
            None,
            Some("t-orig-1"),
        );
        // Lead delegates by dispatching a task reusing t-orig-1 (corr = task_id).
        assert_eq!(
            resolve_delegated(&home, "lead", Some("t-orig-1"), None),
            1,
            "delegating dispatcher's own track resolves on the reused task id"
        );
        let left = list(&home);
        assert_eq!(left.len(), 1, "co-subscriber's track must be left intact");
        assert_eq!(left[0].1.target, "reviewer2");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_delegated_opportunistic_branch_and_no_false_positive() {
        let home = tmp_home("delegated-branch");
        record(
            &home,
            "lead",
            "o/r@fix/y",
            "2026-07-06T00:00:00Z",
            None,
            Some("t-orig-2"),
        );
        // No corr/task-id match, but the dispatch carried branch=fix/y → resolves.
        assert_eq!(
            resolve_delegated(&home, "lead", Some("t-unrelated"), Some("fix/y")),
            1
        );
        // Fresh track: neither corr nor branch matches → no resolution.
        record(
            &home,
            "lead",
            "o/r@fix/z",
            "2026-07-06T00:00:00Z",
            None,
            Some("t-orig-3"),
        );
        assert_eq!(
            resolve_delegated(&home, "lead", Some("t-nope"), Some("fix/other")),
            0
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2667 F1 (reviewer4): the `resolve_delegated` CALL SITE must fire only for a
    /// kind=TASK delegation — a kind=query carrying the same correlation is NOT a
    /// delegation and must never discharge the dispatcher's ci-ready track
    /// (obligation loss). Real-entry test: drives `track_dispatch` (the gate), not
    /// the resolver directly — the direct-resolver tests above can't see the gate.
    #[test]
    fn track_dispatch_resolves_delegated_only_for_task_kind_not_query_2667() {
        let home = tmp_home("track-dispatch-kind-gate");
        let mk = |kind: &str| crate::inbox::InboxMessage {
            schema_version: 1,
            id: Some(format!("m-{kind}")),
            from: "lead".into(),
            text: format!("[dispatch] {kind}"),
            kind: Some(kind.into()),
            correlation_id: Some("o/r@feat".into()),
            task_id: if kind == "task" {
                Some("t-x".into())
            } else {
                None
            },
            timestamp: "2026-07-06T00:00:00Z".into(),
            ..Default::default()
        };
        let params = serde_json::json!({});
        record(
            &home,
            "lead",
            "o/r@feat",
            "2026-07-06T00:00:00Z",
            None,
            Some("t-x"),
        );

        // kind=query with the SAME correlation → track MUST survive.
        crate::api::handlers::messaging::track_dispatch(
            &home,
            &params,
            "lead",
            "reviewer",
            &mk("query"),
        );
        assert_eq!(
            list(&home).len(),
            1,
            "a kind=query is not a delegation — the dispatcher's ci-ready track must survive"
        );

        // kind=task with its canonical task_id IS the delegation → resolves it;
        // the repo/branch correlation is display-only for task lifecycle.
        crate::api::handlers::messaging::track_dispatch(
            &home,
            &params,
            "lead",
            "reviewer",
            &mk("task"),
        );
        assert_eq!(
            list(&home).len(),
            0,
            "a kind=task dispatch IS the delegation discharge — the dispatcher's own track resolves"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2667 F2 (reviewer5, isolation): the opportunistic branch fallback compares
    /// only `ends_with(\"@{branch}\")` — NO repo dimension. A dispatcher holding a
    /// same-named branch handoff in TWO repos would lose BOTH on one delegation
    /// (silent cross-repo obligation loss). Mirror #2662's exactly-one fail-safe:
    /// the branch signal resolves ONLY when it disambiguates to a single track;
    /// 2+ branch matches = ambiguity → resolve none by branch (the precise task_id
    /// path + explicit discharge back-stop the residual).
    #[test]
    fn resolve_delegated_branch_fallback_exactly_one_cross_repo_isolation_2667() {
        let home = tmp_home("delegated-xrepo");
        // Same branch name `shared` pending in two different repos.
        record(
            &home,
            "lead",
            "o/r1@shared",
            "2026-07-06T00:00:00Z",
            None,
            Some("t-r1"),
        );
        record(
            &home,
            "lead",
            "o/r2@shared",
            "2026-07-06T00:00:00Z",
            None,
            Some("t-r2"),
        );

        // A delegation carrying only the AMBIGUOUS branch (corr matches neither):
        // `@shared` matches both repos → ambiguous → resolve NOTHING (fail-safe).
        assert_eq!(
            resolve_delegated(&home, "lead", Some("t-unrelated"), Some("shared")),
            0,
            "ambiguous cross-repo branch match must resolve nothing (exactly-one fail-safe)"
        );
        assert_eq!(
            list(&home).len(),
            2,
            "both same-branch tracks survive an ambiguous branch-only delegation"
        );

        // The PRECISE reused task_id names exactly one repo's work → resolves only
        // that track, leaving the other repo's same-branch handoff intact.
        assert_eq!(
            resolve_delegated(&home, "lead", Some("t-r1"), Some("shared")),
            1,
            "the reused task_id disambiguates to exactly one repo's track"
        );
        let left = list(&home);
        assert_eq!(
            left.len(),
            1,
            "only r1's track resolved; r2's same-branch handoff survives"
        );
        assert_eq!(
            left[0].1.correlation, "o/r2@shared",
            "the surviving track must be the OTHER repo's (no cross-repo bleed)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_by_correlation_clears_all_targets() {
        let home = tmp_home("resolve-corr");
        record(
            &home,
            "reviewer",
            "o/r@b",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
        record(
            &home,
            "reviewer-2",
            "o/r@b",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
        record(
            &home,
            "reviewer",
            "o/r@other",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
        assert_eq!(resolve_by_correlation(&home, "o/r@b", "test"), 2);
        let left = list(&home);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].1.correlation, "o/r@other");
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2412-follow-up (ci-handoff correlation convention split): a standard
    /// fleet `kind=report` carries `correlation_id=t-...` (Sprint 58 W4 PR-1),
    /// NOT `repo@branch` — so a track must also resolve when the CALLER's
    /// `correlation` arg is the dispatch's task id, not just when it matches
    /// the track's `repo@branch` correlation field.
    #[test]
    fn resolve_by_correlation_also_matches_task_id() {
        let home = tmp_home("resolve-taskid");
        record(
            &home,
            "reviewer",
            "o/r@b",
            "2026-06-10T00:00:00Z",
            None,
            Some("t-20260622133030757281-1"),
        );
        assert_eq!(
            resolve_by_correlation(&home, "t-20260622133030757281-1", "report_arrived"),
            1,
            "a standard report's task-id correlation must resolve the track \
             recorded with that task_id, not just a repo@branch match"
        );
        assert!(list(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    /// Sibling of the above: a track with NO recorded `task_id` (the common
    /// case before this fix, or any dispatch path that never had one) must
    /// still resolve normally by its `repo@branch` correlation — the new OR
    /// clause must not require both keys.
    #[test]
    fn resolve_by_correlation_still_matches_repo_branch_when_task_id_absent() {
        let home = tmp_home("resolve-repobranch-only");
        record(
            &home,
            "reviewer",
            "o/r@b",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
        assert_eq!(
            resolve_by_correlation(&home, "o/r@b", "report_arrived"),
            1,
            "repo@branch matching must be unaffected by the task_id addition"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// task66: a task/analysis report can carry the same task correlation as a
    /// review assignment, but correlation alone is not review authority. The
    /// real `track_dispatch` entry point must leave the CI handoff intact.
    #[test]
    fn ordinary_report_task_id_correlation_does_not_resolve_ci_handoff_2760() {
        let home = tmp_home("2760-untyped-taskid-wiring");
        let task_id = "t-20260622133030757281-1";
        record(
            &home,
            "gapfix-dev",
            "owner/repo@branch",
            "2026-06-10T00:00:00Z",
            None,
            Some(task_id),
        );
        assert_eq!(list(&home).len(), 1);

        let msg = crate::inbox::InboxMessage::new_system("gapfix-dev", "report", "VERIFIED")
            .with_correlation_id(task_id.to_string());
        crate::api::handlers::messaging::track_dispatch(
            &home,
            &serde_json::json!({}),
            "gapfix-dev",
            "lead",
            &msg,
        );

        assert!(
            !list(&home).is_empty(),
            "task66: an ordinary report's task correlation must not resolve a \
             code-review CI handoff without a validated receipt"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// task66 positive sibling: the same real entry point still resolves the
    /// handoff when the API sink has attached a validated typed receipt.
    #[test]
    fn validated_review_receipt_resolves_ci_handoff_2760() {
        let home = tmp_home("2760-typed-taskid-wiring");
        let task_id = "t-20260622133030757281-1";
        record(
            &home,
            "gapfix-dev",
            "owner/repo@branch",
            "2026-06-10T00:00:00Z",
            None,
            Some(task_id),
        );

        crate::api::handlers::messaging::track_dispatch(
            &home,
            &serde_json::json!({}),
            "gapfix-dev",
            "lead",
            &typed_review_report(task_id),
        );

        assert!(
            list(&home).is_empty(),
            "task66: a validated typed receipt resolves the correlated CI handoff"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_for_target_correlation_scopes_to_exact_target_and_correlation() {
        // #t-92758 P2: unwatch dismiss clears ONLY the caller's own track for the
        // exact repo@branch — a co-subscriber's track (same correlation, different
        // target) and the caller's other-branch track both survive.
        let home = tmp_home("resolve-tc");
        record(&home, "lead", "o/r@b", "2026-06-10T00:00:00Z", None, None);
        record(
            &home,
            "reviewer",
            "o/r@b",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
        record(
            &home,
            "lead",
            "o/r@other",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
        assert_eq!(resolve_for_target_correlation(&home, "lead", "o/r@b"), 1);
        let left = list(&home);
        assert_eq!(left.len(), 2, "only lead's o/r@b cleared");
        assert!(left
            .iter()
            .any(|(_, t)| t.target == "reviewer" && t.correlation == "o/r@b"));
        assert!(left
            .iter()
            .any(|(_, t)| t.target == "lead" && t.correlation == "o/r@other"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_claimed_scopes_to_target_and_branch() {
        let home = tmp_home("resolve-claim");
        record(
            &home,
            "reviewer",
            "o/r@fix/x",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
        record(
            &home,
            "other",
            "o/r@fix/x",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
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
        record(&home, "reviewer", "o/r@old", &old, None, None);
        record(&home, "reviewer", "o/r@fresh", &fresh, None, None);
        record(
            &home,
            "reviewer",
            "o/r@broken",
            "not-a-timestamp",
            None,
            None,
        );
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
        record(
            &home,
            "reviewer",
            "o/r@b",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
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
        record(
            &home,
            "reviewer",
            "o/r@b",
            "2026-06-10T00:00:00Z",
            None,
            None,
        ); // S1
        record(
            &home,
            "reviewer",
            "o/r@b",
            "2026-06-10T09:00:00Z",
            None,
            None,
        ); // S2
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
        record(
            &home,
            "reviewer",
            "o/r@b",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
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
        record(
            &home,
            "reviewer",
            "o/r@a/b",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
        record(
            &home,
            "reviewer",
            "o/r@a_b",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
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
            task_id: None,
            ci_handoff_episode: None,
            ci_handoff_class: None,
            last_renudged_at: None,
            last_escalated_at: None,
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
            None,
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
            None,
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
            None,
        );
        record(
            &home,
            "reviewer",
            "o/r@b2",
            "2026-06-10T00:00:00Z",
            Some("HEAD_OLD"),
            None,
        );
        let resolved = resolve_head_advanced(&home, "o/r@b1", "HEAD_NEW");
        assert_eq!(resolved, 1, "only b1's track is for the moved head");
        let remaining = list(&home);
        assert_eq!(remaining.len(), 1, "b2's track is untouched");
        assert!(remaining.iter().all(|(_, t)| t.correlation == "o/r@b2"));
        std::fs::remove_dir_all(&home).ok();
    }

    // ── task167 RED-first R1-R15: protected-main episode settlement ────────
    fn protected_message(
        agent: &str,
        correlation: &str,
        episode: &str,
    ) -> crate::inbox::InboxMessage {
        let mut msg = crate::inbox::InboxMessage::new_system(
            "system:ci",
            "ci-ready-for-action",
            format!("[ci-ready-for-action] {correlation}"),
        )
        .with_correlation_id(correlation.to_string());
        msg.ci_handoff_episode = Some(episode.to_string());
        msg.ci_handoff_class = Some(crate::inbox::CiHandoffClass::Protected);
        msg.text = format!("{agent}:{correlation}");
        msg
    }
    fn seed_protected(home: &Path, agent: &str, correlation: &str, episode: &str) {
        crate::inbox::enqueue(home, agent, protected_message(agent, correlation, episode)).unwrap();
        assert!(record_with_identity(
            home,
            agent,
            correlation,
            "2026-06-10T00:00:00Z",
            Some("HEAD"),
            None,
            Some(episode),
            Some(crate::inbox::CiHandoffClass::Protected)
        ));
    }

    #[test]
    fn r1_episode_and_class_round_trip() {
        let home = tmp_home("r1-identity");
        assert!(record_with_identity(
            &home,
            "reviewer",
            "o/r@main",
            "2026-06-10T00:00:00Z",
            None,
            None,
            Some("ep-1"),
            Some(crate::inbox::CiHandoffClass::Protected)
        ));
        let track = &list(&home)[0].1;
        assert_eq!(track.ci_handoff_episode.as_deref(), Some("ep-1"));
        assert_eq!(
            track.ci_handoff_class,
            Some(crate::inbox::CiHandoffClass::Protected)
        );
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn r2_resolver_requires_exact_episode() {
        let home = tmp_home("r2-exact");
        seed_protected(&home, "reviewer", "o/r@main", "ep-1");
        assert_eq!(
            resolve_protected_episode(&home, "reviewer", "o/r@main", "ep-old", "ack_protected"),
            0
        );
        assert_eq!(
            resolve_protected_episode(&home, "reviewer", "o/r@main", "ep-1", "ack_protected"),
            1
        );
        assert!(list(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn r3_old_ack_after_rerecord_preserves_new_episode() {
        let home = tmp_home("r3-rerecord");
        assert!(record_with_identity(
            &home,
            "reviewer",
            "o/r@main",
            "2026-06-10T00:00:00Z",
            None,
            None,
            Some("old"),
            Some(crate::inbox::CiHandoffClass::Protected)
        ));
        assert!(record_with_identity(
            &home,
            "reviewer",
            "o/r@main",
            "2026-06-10T01:00:00Z",
            None,
            None,
            Some("new"),
            Some(crate::inbox::CiHandoffClass::Protected)
        ));
        assert_eq!(
            resolve_protected_episode(&home, "reviewer", "o/r@main", "old", "ack_protected"),
            0
        );
        assert_eq!(list(&home)[0].1.ci_handoff_episode.as_deref(), Some("new"));
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn r4_target_isolation() {
        let home = tmp_home("r4-target");
        seed_protected(&home, "a", "o/r@main", "ep-a");
        seed_protected(&home, "b", "o/r@main", "ep-b");
        assert_eq!(
            resolve_protected_episode(&home, "a", "o/r@main", "ep-a", "ack_protected"),
            1
        );
        assert_eq!(list(&home)[0].1.target, "b");
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn r5_cross_repo_correlation_isolation() {
        let home = tmp_home("r5-repo");
        seed_protected(&home, "reviewer", "one/r@main", "ep-1");
        seed_protected(&home, "reviewer", "two/r@main", "ep-2");
        assert_eq!(
            resolve_protected_episode(&home, "reviewer", "one/r@main", "ep-1", "ack_protected"),
            1
        );
        assert_eq!(list(&home)[0].1.correlation, "two/r@main");
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn r6_legacy_classless_track_fails_closed() {
        let home = tmp_home("r6-legacy");
        record(
            &home,
            "reviewer",
            "o/r@main",
            "2026-06-10T00:00:00Z",
            None,
            None,
        );
        assert_eq!(
            resolve_protected_episode(&home, "reviewer", "o/r@main", "ep-1", "ack_protected"),
            0
        );
        assert_eq!(list(&home).len(), 1);
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn r7_feature_class_fails_closed_for_protected_settlement() {
        let home = tmp_home("r7-feature");
        assert!(record_with_identity(
            &home,
            "reviewer",
            "o/r@feature",
            "2026-06-10T00:00:00Z",
            None,
            None,
            Some("ep-1"),
            Some(crate::inbox::CiHandoffClass::Feature)
        ));
        assert_eq!(
            resolve_protected_episode(&home, "reviewer", "o/r@feature", "ep-1", "ack_protected"),
            0
        );
        assert_eq!(list(&home).len(), 1);
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn r8_explicit_single_ack_settles_exact_protected_track() {
        let home = tmp_home("r8-single");
        seed_protected(&home, "reviewer", "o/r@main", "ep-1");
        crate::inbox::drain(&home, "reviewer");
        assert_eq!(crate::inbox::ack(&home, "reviewer", None), 1);
        assert!(list(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn r9_explicit_batch_ack_settles_all_exact_rows() {
        let home = tmp_home("r9-batch");
        seed_protected(&home, "reviewer", "o/r@a", "ep-a");
        seed_protected(&home, "reviewer", "o/r@b", "ep-b");
        crate::inbox::drain(&home, "reviewer");
        assert_eq!(crate::inbox::ack(&home, "reviewer", None), 2);
        assert!(list(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn r10_implicit_next_drain_ack_settles_prior_batch() {
        let home = tmp_home("r10-implicit");
        seed_protected(&home, "reviewer", "o/r@main", "ep-1");
        crate::inbox::drain(&home, "reviewer");
        crate::inbox::drain(&home, "reviewer");
        assert!(list(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn r11_new_delivery_does_not_resolve_track() {
        let home = tmp_home("r11-new-delivery");
        seed_protected(&home, "reviewer", "o/r@main", "ep-1");
        assert_eq!(crate::inbox::drain(&home, "reviewer").len(), 1);
        assert_eq!(list(&home).len(), 1);
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn r12_reconciler_resolves_processed_row_after_grace() {
        let home = tmp_home("r12-reconcile");
        seed_protected(&home, "reviewer", "o/r@main", "ep-1");
        crate::inbox::drain(&home, "reviewer");
        crate::inbox::ack(&home, "reviewer", None);
        // Recreate the sidecar to model a crash after row processing but before
        // the resolver's delete completed.
        assert!(record_with_identity(
            &home,
            "reviewer",
            "o/r@main",
            "2026-06-10T00:00:00Z",
            None,
            None,
            Some("ep-1"),
            Some(crate::inbox::CiHandoffClass::Protected)
        ));
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-10T00:01:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(reconcile_processed(&home, &now), 1);
        assert!(list(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn r13_reconciler_missing_row_fails_closed() {
        let home = tmp_home("r13-missing");
        assert!(record_with_identity(
            &home,
            "reviewer",
            "o/r@main",
            "2026-06-10T00:00:00Z",
            None,
            None,
            Some("ep-1"),
            Some(crate::inbox::CiHandoffClass::Protected)
        ));
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-10T00:01:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(reconcile_processed(&home, &now), 0);
        assert_eq!(list(&home).len(), 1);
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn r14_reconciler_ambiguous_rows_fails_closed() {
        let home = tmp_home("r14-ambiguous");
        seed_protected(&home, "reviewer", "o/r@main", "ep-1");
        let mut duplicate = protected_message("reviewer", "o/r@main", "ep-1");
        duplicate.id = Some("duplicate".into());
        crate::inbox::enqueue(&home, "reviewer", duplicate).unwrap();
        crate::inbox::drain(&home, "reviewer");
        crate::inbox::ack(&home, "reviewer", None);
        assert!(record_with_identity(
            &home,
            "reviewer",
            "o/r@main",
            "2026-06-10T00:00:00Z",
            None,
            None,
            Some("ep-1"),
            Some(crate::inbox::CiHandoffClass::Protected)
        ));
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-10T00:01:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(reconcile_processed(&home, &now), 0);
        assert_eq!(list(&home).len(), 1);
        std::fs::remove_dir_all(&home).ok();
    }
    #[test]
    fn r15_repeated_ack_and_resolution_are_idempotent() {
        let home = tmp_home("r15-idempotent");
        seed_protected(&home, "reviewer", "o/r@main", "ep-1");
        crate::inbox::drain(&home, "reviewer");
        assert_eq!(crate::inbox::ack(&home, "reviewer", None), 1);
        assert_eq!(crate::inbox::ack(&home, "reviewer", None), 0);
        assert!(list(&home).is_empty());
        assert_eq!(
            resolve_protected_episode(&home, "reviewer", "o/r@main", "ep-1", "ack_protected"),
            0
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
