//! #t-…83936-4 incident follow-up: canonical `source_repo` existence heartbeat.
//!
//! On 2026-07-06 the operator's canonical repo was deleted and went undetected
//! for ~40 minutes: the daemon held the deleted dir as its cwd (an orphaned
//! inode still answers cwd-relative lookups), so `binding_state` reported a stale
//! `valid=true` and nothing surfaced the disappearance until an agent's commit
//! hit the dangling gitdir. The root problem was "deleted with nobody noticing".
//!
//! This watchdog closes that gap. Every N ticks it enumerates the registered
//! source_repos (`binding::bound_source_repos` — distinct ABSOLUTE paths from
//! each `binding.json`) and checks, BY ABSOLUTE PATH, that each still (a) exists
//! and (b) is a git repo (`git -C <abs> rev-parse --git-dir`). A vanished or
//! corrupt canonical pages ALL operator escalation channels + writes an
//! event-log `canonical_repo_missing` row.
//!
//! ⚠ The cwd trap (the soul of this fix): the daemon's own cwd may itself BE the
//! deleted canonical, and an orphaned inode still resolves cwd-relative lookups —
//! exactly the incident. Every check here MUST resolve by ABSOLUTE PATH, never
//! `.`/cwd-relative. `bound_source_repos` yields absolute paths, and
//! `std::fs::metadata(abs)` / `git -C abs` do fresh path lookups. This invariant
//! is pinned by a test that runs the check with the process cwd set to a
//! since-deleted directory (see `flags_missing_repo_even_when_cwd_is_deleted`).
//!
//! Complements protection ① (`binding_state`'s `worktree_resolves`): any agent
//! calling `binding_state` (bind / release / introspect) detects a dead canonical
//! INSTANTLY, so during normal activity detection is second-level; this periodic
//! heartbeat is the backstop for a fully-idle fleet.
//!
//! Dedup: a per-repo latch pages ONCE on the present→missing transition; recovery
//! (missing→present) clears the latch so a later re-deletion pages again.

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::Path;

/// Per-tick canonical-existence watchdog. Default cadence 60 ticks (~10 min at
/// the 10 s tick) — a 4× improvement over the incident's 40-min silence; the
/// real-time path is protection ①.
pub(crate) struct CanonicalHeartbeatHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
    /// Repos currently in the alerted-missing state (dedup: one page per outage).
    alerted: Mutex<HashSet<String>>,
}

impl CanonicalHeartbeatHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
            alerted: Mutex::new(HashSet::new()),
        }
    }
}

impl PerTickHandler for CanonicalHeartbeatHandler {
    fn name(&self) -> &'static str {
        "canonical_heartbeat"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        let repos = crate::binding::bound_source_repos(ctx.home);
        let registered: HashSet<String> = repos
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect();
        let mut alerted = self.alerted.lock();
        for repo in &repos {
            let key = repo.to_string_lossy().into_owned();
            let Some(reason) = canonical_missing_reason(repo) else {
                alerted.remove(&key); // healthy → reset latch so a future loss re-pages
                continue;
            };
            if !alerted.insert(key.clone()) {
                continue; // already paged this outage — don't re-page every cadence
            }
            let msg = format!(
                "[canonical-missing] registered source_repo '{}' is GONE ({reason}). \
                 Every worktree bound to it is now unusable (dangling gitdir) and \
                 agents may silently commit into dead worktrees until it is \
                 restored — re-clone/restore the canonical, then repair worktrees.",
                repo.display(),
            );
            let dispatched = crate::channel::notify_all_escalation_channels(
                &key,
                crate::channel::NotifySeverity::Error,
                &msg,
                false,
            );
            crate::event_log::log(ctx.home, "canonical_repo_missing", &key, &msg);
            tracing::error!(
                repo = %repo.display(),
                reason,
                channels = dispatched,
                "canonical_repo_missing: registered source_repo vanished"
            );
        }
        // De-registered repos (no live binding references them) drop out of the
        // latch so it can't grow unbounded.
        alerted.retain(|k| registered.contains(k));
    }
}

/// `None` = the canonical is healthy; `Some(reason)` = it's gone or corrupt.
/// Resolves STRICTLY by absolute path (the caller passes absolute source_repo
/// paths) so the daemon's own — possibly orphaned — cwd can never mask a
/// deletion. `git_ok` runs only after the path exists, so it never touches a
/// missing dir.
fn canonical_missing_reason(repo: &Path) -> Option<&'static str> {
    if std::fs::metadata(repo).is_err() {
        return Some("path missing");
    }
    if !crate::git_helpers::git_ok(repo, &["rev-parse", "--git-dir"]) {
        return Some("not a git repo (corrupt/removed .git)");
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp(tag: &str) -> std::path::PathBuf {
        static C: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "agend-canon-hb-{tag}-{}-{}",
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn git_init(dir: &Path) {
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git init");
    }

    #[test]
    fn healthy_repo_is_not_missing() {
        let d = tmp("healthy");
        git_init(&d);
        assert_eq!(canonical_missing_reason(&d), None);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn deleted_dir_is_path_missing() {
        let d = tmp("deleted");
        git_init(&d);
        std::fs::remove_dir_all(&d).unwrap();
        assert_eq!(canonical_missing_reason(&d), Some("path missing"));
    }

    #[test]
    fn existing_non_git_dir_is_corrupt() {
        let d = tmp("nongit"); // exists but never `git init`ed
        assert_eq!(
            canonical_missing_reason(&d),
            Some("not a git repo (corrupt/removed .git)")
        );
        std::fs::remove_dir_all(&d).ok();
    }

    /// THE SOUL of this fix (#t-…83936-4): the check must resolve by ABSOLUTE PATH
    /// and stay correct even when the process cwd is itself a deleted directory —
    /// exactly the incident, where the daemon's cwd was the orphaned canonical.
    /// A future refactor to a `.`/cwd-relative check would silently reintroduce
    /// the 40-min blind spot; this pins against that.
    #[test]
    #[serial_test::serial]
    fn flags_missing_repo_even_when_cwd_is_deleted() {
        let healthy = tmp("cwd-healthy");
        git_init(&healthy);
        let gone = tmp("cwd-gone");
        git_init(&gone);
        std::fs::remove_dir_all(&gone).unwrap(); // an absolute repo that is now gone

        // Put the process into an orphaned-inode cwd (delete our own cwd).
        let prev_cwd = std::env::current_dir().ok();
        let orphan_cwd = tmp("cwd-orphan");
        std::env::set_current_dir(&orphan_cwd).unwrap();
        std::fs::remove_dir_all(&orphan_cwd).unwrap(); // cwd now points at a deleted inode

        // Despite the deleted cwd, absolute-path resolution must be correct:
        let gone_verdict = canonical_missing_reason(&gone);
        let healthy_verdict = canonical_missing_reason(&healthy);

        // Restore cwd BEFORE asserting so a failure can't strand the test process.
        if let Some(p) = prev_cwd {
            std::env::set_current_dir(p).ok();
        }
        assert_eq!(
            gone_verdict,
            Some("path missing"),
            "a deleted absolute repo must be flagged even from a deleted cwd"
        );
        assert_eq!(
            healthy_verdict, None,
            "a healthy absolute repo must resolve even from a deleted cwd"
        );
        std::fs::remove_dir_all(&healthy).ok();
    }
}
