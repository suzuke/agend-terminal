//! Worktree GC retention handler — safe archival with status pre-check + `.trash` window.
//!
//! Phase A1 (#1053) replaced `remove_dir_all` with `git status --porcelain=v1
//! --ignored` + `git worktree remove` — narrowing the deletion set to clean
//! worktrees but leaving a TOCTOU window between the status check and
//! removal: a `.gitignored` file written into the worktree mid-window would
//! be silently deleted by `git worktree remove`.
//!
//! Phase A2 (#1058) closes that window by replacing `git worktree remove`
//! with `fs::rename` to `~/.agend-terminal/.trash/worktrees/<agent>-<unix_ts>/`.
//! `rename(2)` is atomic on POSIX (within filesystem), so any file written in
//! the race window is captured along with the directory — no data loss
//! possible. Operator recovery available within
//! `AGEND_WORKTREE_GC_TRASH_DAYS` (default 7) retention window;
//! `AGEND_WORKTREE_GC_TRASH_DAYS=0` purges same-sweep (replaces a separate
//! disable flag).
//!
//! Cross-filesystem rename returns `EXDEV` → falls back to recursive copy
//! followed by `remove_dir_all`. The fallback is TOCTOU-unsafe by
//! construction (race window opens during recursive copy iteration); a
//! warning is logged. Configure `.trash` placement on the same filesystem as
//! managed worktrees for safety.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub(crate) enum RemovalOutcome {
    Removed,
    Skipped { reason: String },
}

fn owning_repo(worktree: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(worktree.join(".git")).ok()?;
    let gitdir = content.strip_prefix("gitdir: ")?.trim();
    // gitdir = <repo>/.git/worktrees/<name> → 3 parents up = repo root
    Path::new(gitdir)
        .parent()?
        .parent()?
        .parent()
        .map(PathBuf::from)
}

fn trash_root(home: &Path) -> PathBuf {
    home.join(".trash").join("worktrees")
}

fn trash_retention_days() -> u64 {
    crate::env_util::env_parse::<u64>("AGEND_WORKTREE_GC_TRASH_DAYS", 7)
}

fn archive_ts() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    format!("{}-{:09}", d.as_secs(), d.subsec_nanos())
}

/// [H1] Extract the archive time (unix secs) embedded in a `.trash/worktrees/`
/// entry's directory NAME. The name is `{agent}-{secs}-{nanos}` (see
/// [`archive_ts`], which itself contains a `-`), so the last two `-`-segments are
/// the timestamp stamped atomically at archive time.
///
/// This is the only trustworthy archive clock: [`try_archive`] uses `fs::rename`,
/// which PRESERVES the source worktree's original mtime. A freshly-archived
/// force-reclaimed worktree (whose source was untouched for days) therefore has a
/// stale `metadata.modified()`, and keying the retention age on mtime would purge
/// it the SAME sweep it was archived — defeating the recovery window for exactly
/// the irrecoverable case. Returns `None` for names that don't match the expected
/// shape (caller falls back to mtime).
fn archived_at_secs(dir_name: &str) -> Option<u64> {
    let mut parts = dir_name.rsplitn(3, '-');
    let nanos = parts.next()?;
    let secs = parts.next()?;
    parts.next()?; // agent segment (may itself contain '-') must be present
    if nanos.is_empty()
        || secs.is_empty()
        || !nanos.bytes().all(|b| b.is_ascii_digit())
        || !secs.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    secs.parse::<u64>().ok()
}

/// Recursive copy helper used by the `EXDEV` fallback in `try_archive`.
/// Only reachable on Unix where `libc::EXDEV` is the cross-filesystem
/// rename errno; gated to `cfg(unix)` so non-Unix targets don't see it
/// as dead code under `-D dead_code`.
#[cfg(unix)]
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let ftype = entry.file_type()?;
        if ftype.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if ftype.is_symlink() {
            let target = std::fs::read_link(&src_path)?;
            std::os::unix::fs::symlink(target, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Archive `src` to `dst` atomically. Prefers `fs::rename`; falls back to
/// recursive copy + remove on `EXDEV` (cross-filesystem). The fallback is
/// TOCTOU-unsafe — operator should keep `.trash` on the same filesystem.
fn try_archive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            tracing::warn!(
                src = %src.display(),
                dst = %dst.display(),
                "rename crossed filesystem boundary; falling back to copy+remove (TOCTOU-unsafe)"
            );
            copy_dir_recursive(src, dst)?;
            std::fs::remove_dir_all(src)?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

pub(crate) fn maybe_remove_candidate(
    home: &Path,
    candidate: &crate::worktree_pool::GcCandidate,
) -> RemovalOutcome {
    let agent = candidate.agent.as_str();
    let force_reclaim = candidate.kind == crate::worktree_pool::GcKind::ForceReclaim;
    // t-worktree-leak PR-2: a force-reclaim's binding is still present (never
    // released) — capture branch/repo from it BEFORE archive for the post-archive
    // ALERT classification + the unbind.
    let pre_binding = if force_reclaim {
        crate::binding::read(home, agent)
    } else {
        None
    };
    let path = candidate
        .path
        .canonicalize()
        .unwrap_or_else(|_| candidate.path.clone());
    let path = path.as_path();
    let status_output =
        crate::git_helpers::git_bypass(path, &["status", "--porcelain=v1", "--ignored"]);
    match status_output {
        Ok(o) if o.status.success() => {
            let status_text = String::from_utf8_lossy(&o.stdout);
            // Dirty pre-check, parameterized by mode (reviewer-2 efficacy). Porcelain
            // v1 line = `XY <path>`: code at cols 0..2, path from col 3. `!!` = ignored.
            //
            // FORCE-RECLAIM (only fires on dead+age, ALWAYS archive-to-trash =
            // recoverable): no gitignored file should block — `target/`, the
            // `.agend-managed` marker, operator scratch all land in `.trash`
            // recoverably (the archive IS the safety net). Block ONLY on real
            // tracked/untracked WIP. Without this, every BUILT worktree (`!! target/`)
            // no-ops — the same defect the marker caused.
            //
            // CLEAN-RELEASE (can hard-delete via gc_run = irrecoverable): keep #1053's
            // `--ignored` strictness to protect operator gitignored DATA (T13a) — but
            // never let the daemon's OWN marker block. (CleanRelease over-protecting
            // `target/` is a separate pre-existing leak root, tracked as follow-up.)
            let has_blocking_content = status_text.lines().any(|line| {
                let code = line.get(..2).unwrap_or("");
                let path = line.get(3..).map(str::trim).unwrap_or("");
                if path.is_empty() {
                    return false;
                }
                // The daemon's OWN marker NEVER blocks, under ANY status code — it is
                // `!! .agend-managed` where the repo gitignores it (.gitignore:29) and
                // `?? .agend-managed` where it does not. Either way, not operator data.
                if path == crate::worktree_pool::MANAGED_MARKER {
                    return false;
                }
                if code == "!!" {
                    // Other ignored file (e.g. `target/`, operator scratch).
                    return !force_reclaim; // force-reclaim: archive-recoverable → never blocks
                }
                true // tracked/untracked = real WIP → always blocks
            });
            if has_blocking_content {
                tracing::warn!(
                    path = %path.display(),
                    status = %status_text.trim(),
                    "worktree has WIP (tracked/untracked/ignored), skipping GC"
                );
                return RemovalOutcome::Skipped {
                    reason: "wip_status_nonempty".to_string(),
                };
            }
        }
        Ok(o) => {
            tracing::warn!(
                path = %path.display(),
                stderr = %String::from_utf8_lossy(&o.stderr),
                "git status check failed, skipping GC"
            );
            return RemovalOutcome::Skipped {
                reason: "status_check_failed".to_string(),
            };
        }
        Err(e) => {
            tracing::error!(error = %e, "git status invocation failed");
            return RemovalOutcome::Skipped {
                reason: format!("invoke error: {e}"),
            };
        }
    }

    let repo = match owning_repo(path) {
        Some(r) => r,
        None => {
            tracing::warn!(path = %path.display(), "cannot determine owning repo, skipping GC");
            return RemovalOutcome::Skipped {
                reason: "owning_repo_unknown".to_string(),
            };
        }
    };
    // [M3] FORCE-RECLAIM: hold the SAME binding lock `bind_full()` (binding.rs:67)
    // and the clean-release GC path (worktree_pool.rs `gc_remove_one`) take, so
    // the liveness re-validate → archive → unbind below is mutually exclusive
    // with a concurrent re-lease. Without it, a `bind_full` could interleave
    // between the liveness check and the `fs::rename`, archiving a freshly-leased
    // live worktree (which #H1 then purges same-sweep → unrecoverable). Held to
    // function return. Clean-release archives via this fn only from the retention
    // `sweep` (the gc_run clean path holds its own lock), so we scope the lock to
    // the force-reclaim branch to avoid changing clean-release's locking.
    let _binding_lock = if force_reclaim {
        let lock_path = crate::paths::runtime_dir(home)
            .join(agent)
            .join(".binding.json.lock");
        match crate::store::acquire_file_lock(&lock_path) {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::warn!(
                    agent,
                    error = %e,
                    "force-reclaim: binding lock acquisition failed, skipping archive"
                );
                return RemovalOutcome::Skipped {
                    reason: format!("binding_lock_failed: {e}"),
                };
            }
        }
    } else {
        None
    };

    // #1170 + t-worktree-leak PR-2: re-validate just before archive (TOCTOU /
    // fencing). For a CLEAN release the binding must be gone — a rebind means the
    // lease is live again → skip. For a FORCE-RECLAIM the binding is EXPECTED to be
    // present (never released), so re-check LIVENESS instead: if the agent came
    // back to life between enumeration and now, spare it. (Now under the binding
    // lock above for force-reclaim — the liveness check + archive are atomic vs
    // bind_full.)
    if force_reclaim {
        if crate::worktree_pool::is_agent_alive(home, agent) {
            tracing::info!(
                agent,
                path = %path.display(),
                "force-reclaim: agent showed liveness at archive time — sparing (fencing)"
            );
            return RemovalOutcome::Skipped {
                reason: "agent_alive_at_archive".to_string(),
            };
        }
    } else if crate::binding::read(home, agent).is_some() {
        tracing::info!(
            agent,
            path = %path.display(),
            "worktree rebound since GC enumeration, skipping archive"
        );
        return RemovalOutcome::Skipped {
            reason: "rebound_since_enumeration".to_string(),
        };
    }

    let trash_dst = trash_root(home).join(format!("{}-{}", agent, archive_ts()));
    match try_archive(path, &trash_dst) {
        Ok(()) => {
            tracing::info!(
                src = %path.display(),
                dst = %trash_dst.display(),
                "worktree archived to .trash"
            );
            // Prune the dangling worktree reference from git's internal list.
            // Best-effort: prune failure is non-fatal — the rename succeeded,
            // so the active worktree set no longer includes this path; a
            // stale `git worktree list` entry self-corrects on next lookup.
            // #1899: bounded via git_bypass (LOCAL 60s) — best-effort prune.
            let prune = crate::git_helpers::git_bypass(&repo, &["worktree", "prune"]);
            if let Err(e) = prune {
                tracing::warn!(error = %e, "git worktree prune failed (non-fatal)");
            }
            // t-worktree-leak PR-2: a force-reclaim's binding is still present
            // (never released) → clear it now, then LOUD-classify + ALERT.
            if force_reclaim {
                crate::binding::unbind(home, agent);
                let branch = pre_binding
                    .as_ref()
                    .and_then(|b| b.get("branch").and_then(|v| v.as_str()));
                let repo = pre_binding
                    .as_ref()
                    .and_then(|b| b.get("source_repo").and_then(|v| v.as_str()))
                    .and_then(|src| {
                        crate::mcp::handlers::dispatch_hook::derive_repo_from_remote_pub(Path::new(
                            src,
                        ))
                    });
                emit_force_reclaim_alert(home, agent, branch, repo.as_deref(), &candidate.reason);
            }
            RemovalOutcome::Removed
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                dst = %trash_dst.display(),
                error = %e,
                "archive to .trash failed, skipping"
            );
            RemovalOutcome::Skipped {
                reason: format!("archive_failed: {e}"),
            }
        }
    }
}

/// t-worktree-leak PR-2: classify a force-reclaim by the branch's pr_state, to set
/// the ALERT confidence. Never blindly trusts pr_state — `unknown` is explicit.
fn classify_force_reclaim(home: &Path, repo: Option<&str>, branch: Option<&str>) -> &'static str {
    let (Some(repo), Some(branch)) = (repo, branch) else {
        return "unknown";
    };
    use crate::daemon::pr_state::MergeState;
    match crate::daemon::pr_state::load(home, repo, branch) {
        Some(s) => match s.merge_state {
            // A terminal PR whose worktree had to be force-reclaimed means the
            // event-driven release (PR-1) NEVER fired for it = a bug.
            MergeState::Merged { .. } | MergeState::ClosedUnmerged { .. } => "observed_terminal",
            _ if s.pr_number > 0 => "observed_open",
            // #986: a SUCCESSFUL poll (failures==0) is required to claim
            // "queried, none found" — a failed/cold poll also sets last_gh_poll_at.
            _ if s.last_gh_poll_at.is_some() && s.gh_poll_failures == 0 => "queried_none",
            _ => "unknown",
        },
        None => "unknown",
    }
}

/// t-worktree-leak PR-2 safety #4: LOUD operator notification on a force-reclaim,
/// classified by confidence. `observed_terminal` = a terminal PR that never
/// released = an event-release BUG → error-level ALERT (forces the event path to
/// be fixed, never silently swallowed); other confidences are expected
/// abandonment cleanup (warn). Always recoverable from `.trash`.
fn emit_force_reclaim_alert(
    home: &Path,
    agent: &str,
    branch: Option<&str>,
    repo: Option<&str>,
    reason: &str,
) {
    let confidence = classify_force_reclaim(home, repo, branch);
    if confidence == "observed_terminal" {
        tracing::error!(
            agent,
            ?branch,
            confidence,
            reason,
            "PR-2 FORCE-RECLAIM ALERT: archived a worktree whose PR is TERMINAL but was \
             NEVER released — the event-driven release path FAILED (bug). Recoverable in .trash."
        );
    } else {
        tracing::warn!(
            agent, ?branch, confidence, reason,
            "PR-2 force-reclaim: archived an abandoned never-released worktree (recoverable in .trash)"
        );
    }
    crate::event_log::log(
        home,
        if confidence == "observed_terminal" {
            "force_reclaim_alert_event_bug"
        } else {
            "force_reclaim_archived"
        },
        agent,
        &format!("branch={branch:?} confidence={confidence} reason={reason}"),
    );
}

/// Purge `.trash/worktrees/*` entries older than `AGEND_WORKTREE_GC_TRASH_DAYS`
/// days (default 7). `days=0` purges every entry — including those archived
/// earlier in this same sweep tick.
pub(super) fn purge_trash(home: &Path) {
    let root = trash_root(home);
    if !root.exists() {
        return;
    }
    let days = trash_retention_days();
    let cutoff = Duration::from_secs(days.saturating_mul(86400));
    let now = SystemTime::now();
    let entries = match std::fs::read_dir(&root) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read trash root, skipping purge");
            return;
        }
    };
    for entry in entries.flatten() {
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        // [H1] Age from the archive time embedded in the dir NAME (stamped
        // atomically at archive), NOT the inherited mtime — `fs::rename` keeps
        // the source's old mtime, so a fresh archive would look ancient and be
        // purged the same sweep. Fall back to mtime for legacy / unrecognised
        // names.
        let age = match entry.file_name().to_str().and_then(archived_at_secs) {
            Some(secs) => now
                .duration_since(UNIX_EPOCH + Duration::from_secs(secs))
                .unwrap_or(Duration::ZERO),
            None => {
                let mtime = metadata.modified().unwrap_or(now);
                now.duration_since(mtime).unwrap_or(Duration::ZERO)
            }
        };
        if age >= cutoff {
            let p = entry.path();
            if let Err(e) = std::fs::remove_dir_all(&p) {
                tracing::warn!(path = %p.display(), error = %e, "failed to purge trash entry");
            } else {
                tracing::info!(
                    path = %p.display(),
                    age_days = age.as_secs() / 86400,
                    "purged .trash entry"
                );
            }
        }
    }
}

/// Sweep worktree GC candidates. Gated on `AGEND_WORKTREE_GC=1`. Archives
/// clean orphan worktrees to `.trash`, then purges `.trash` entries older
/// than the retention window (N=0 → same-tick purge).
/// Returns the number of worktrees archived this tick.
pub(super) fn sweep(home: &Path) -> usize {
    if std::env::var("AGEND_WORKTREE_GC").as_deref() != Ok("1") {
        return 0;
    }
    let candidates = crate::worktree_pool::gc_candidates(home);
    let mut removed = 0;
    for c in &candidates {
        match maybe_remove_candidate(home, c) {
            RemovalOutcome::Removed => {
                removed += 1;
                tracing::info!(
                    agent = %c.agent,
                    path = %c.path.display(),
                    "retention: worktree archived"
                );
                crate::event_log::log(
                    home,
                    "retention_worktree_archived",
                    &c.agent,
                    &format!("path={}", c.path.display()),
                );
            }
            RemovalOutcome::Skipped { ref reason } => {
                crate::event_log::log(
                    home,
                    "retention_worktree_skipped",
                    &c.agent,
                    &format!("path={} reason={reason}", c.path.display()),
                );
            }
        }
    }
    purge_trash(home);
    removed
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Serializes the tests that mutate the process-global
    /// `AGEND_WORKTREE_GC_TRASH_DAYS` env var, so they cannot race each other (or be
    /// observed mid-mutation) under the parallel test runner.
    static GC_TRASH_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn gc_trash_env_guard() -> std::sync::MutexGuard<'static, ()> {
        GC_TRASH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Build a CleanRelease GcCandidate for the existing maybe_remove_candidate tests.
    fn clean_cand(path: &Path, agent: &str) -> crate::worktree_pool::GcCandidate {
        crate::worktree_pool::GcCandidate {
            path: path.to_path_buf(),
            agent: agent.to_string(),
            reason: "test".to_string(),
            kind: crate::worktree_pool::GcKind::CleanRelease,
        }
    }

    fn tmp_home(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-retention-worktrees-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn setup_git_repo(dir: &Path) -> PathBuf {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();
        repo
    }

    fn add_worktree(repo: &Path, name: &str) -> PathBuf {
        let wt_path = repo.parent().unwrap().join(name);
        std::process::Command::new("git")
            .args(["worktree", "add", "-b", name, wt_path.to_str().unwrap()])
            .current_dir(repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        wt_path
    }

    /// T13a (carried from #1053): worktree with .gitignored file → skip.
    #[test]
    fn worktree_with_gitignored_file_is_skipped() {
        let dir = tmp_home("t13a");
        let repo = setup_git_repo(&dir);
        let wt = add_worktree(&repo, "wt-ignored");

        std::fs::write(wt.join(".gitignore"), "scratch.txt\n").unwrap();
        std::fs::write(wt.join("scratch.txt"), "operator data").unwrap();
        std::process::Command::new("git")
            .args(["add", ".gitignore"])
            .current_dir(&wt)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add gitignore"])
            .current_dir(&wt)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();

        let result = maybe_remove_candidate(&dir, &clean_cand(&wt, "wt-ignored"));
        assert!(matches!(result, RemovalOutcome::Skipped { .. }));
        assert!(wt.exists(), "worktree must NOT be archived");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T13b (carried): worktree with untracked file → skip.
    #[test]
    fn worktree_with_untracked_file_is_skipped() {
        let dir = tmp_home("t13b");
        let repo = setup_git_repo(&dir);
        let wt = add_worktree(&repo, "wt-untracked");

        std::fs::write(wt.join("new-file.txt"), "work in progress").unwrap();

        let result = maybe_remove_candidate(&dir, &clean_cand(&wt, "wt-untracked"));
        assert!(matches!(result, RemovalOutcome::Skipped { .. }));
        assert!(wt.exists(), "worktree must NOT be archived");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T13c (carried): clean worktree → archived to .trash.
    #[test]
    fn clean_worktree_is_archived() {
        let dir = tmp_home("t13c");
        let repo = setup_git_repo(&dir);
        let wt = add_worktree(&repo, "wt-clean");

        let result = maybe_remove_candidate(&dir, &clean_cand(&wt, "wt-clean"));
        assert!(matches!(result, RemovalOutcome::Removed));
        assert!(!wt.exists(), "worktree moved from original path");

        let trash = trash_root(&dir);
        let entries: Vec<_> = std::fs::read_dir(&trash)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "exactly one .trash entry");
        let name = entries[0].file_name();
        assert!(
            name.to_string_lossy().starts_with("wt-clean-"),
            "archive named <agent>-<ts>: {name:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T13d (carried): git status non-zero exit → skip.
    #[test]
    fn git_status_failure_skips() {
        let dir = tmp_home("t13d");
        let bad_path = dir.join("not-a-worktree");
        std::fs::create_dir_all(&bad_path).unwrap();

        let result = maybe_remove_candidate(&dir, &clean_cand(&bad_path, "agent"));
        assert!(matches!(result, RemovalOutcome::Skipped { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T1 (#1058): race-window write preserved in `.trash` via atomic rename.
    ///
    /// `try_archive` uses `fs::rename`, atomic on POSIX. Any file written
    /// into the worktree between the status check and the rename is
    /// captured along with the directory — no data loss.
    #[test]
    fn race_window_write_preserved_in_trash() {
        let dir = tmp_home("t1-race");
        let repo = setup_git_repo(&dir);
        let wt = add_worktree(&repo, "wt-race");

        // Simulate race-window write of a `.gitignored` file. In production
        // this happens between `git status` returning clean and the rename
        // syscall executing; here we write directly because the property
        // under test is that rename captures whatever exists at call time.
        std::fs::write(wt.join(".vscode-settings.json"), b"operator scratch").unwrap();
        let trash_dst = trash_root(&dir).join("wt-race-99999");
        try_archive(&wt, &trash_dst).unwrap();

        assert!(!wt.exists(), "worktree moved from original path");
        assert!(trash_dst.exists(), ".trash entry created");
        assert!(
            trash_dst.join(".vscode-settings.json").exists(),
            "race-window file preserved"
        );
        assert_eq!(
            std::fs::read(trash_dst.join(".vscode-settings.json")).unwrap(),
            b"operator scratch"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T2 (#1058): purge removes entries older than retention window;
    /// recent entries preserved. Unix-only: forcing a directory's mtime
    /// requires `File::set_modified`, but `File::open` on a Windows
    /// directory needs `FILE_FLAG_BACKUP_SEMANTICS` which `std::fs`
    /// doesn't expose. The purge logic itself (`SystemTime` arithmetic)
    /// is platform-agnostic — Unix coverage is sufficient.
    #[cfg(unix)]
    #[test]
    fn trash_purge_removes_old_preserves_recent() {
        let _env = gc_trash_env_guard();
        let dir = tmp_home("t2-purge");
        let trash = trash_root(&dir);
        std::fs::create_dir_all(&trash).unwrap();

        let old = trash.join("old-1700000000");
        let recent = trash.join("recent-now");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&recent).unwrap();
        std::fs::write(old.join("data.txt"), b"old").unwrap();
        std::fs::write(recent.join("data.txt"), b"recent").unwrap();

        let thirty_days_ago = SystemTime::now() - Duration::from_secs(30 * 86400);
        let f = std::fs::File::open(&old).unwrap();
        f.set_modified(thirty_days_ago).unwrap();

        std::env::set_var("AGEND_WORKTREE_GC_TRASH_DAYS", "7");
        purge_trash(&dir);
        std::env::remove_var("AGEND_WORKTREE_GC_TRASH_DAYS");

        assert!(!old.exists(), "30-day-old entry purged");
        assert!(recent.exists(), "fresh entry preserved");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T3 (#1058): `AGEND_WORKTREE_GC_TRASH_DAYS=0` purges entries archived
    /// in the same sweep tick (replaces a separate disable flag).
    #[test]
    fn trash_days_zero_purges_same_tick() {
        let _env = gc_trash_env_guard();
        let dir = tmp_home("t3-zero");
        let trash = trash_root(&dir);
        std::fs::create_dir_all(&trash).unwrap();
        let entry = trash.join("just-archived");
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("data.txt"), b"x").unwrap();

        std::env::set_var("AGEND_WORKTREE_GC_TRASH_DAYS", "0");
        purge_trash(&dir);
        std::env::remove_var("AGEND_WORKTREE_GC_TRASH_DAYS");

        assert!(!entry.exists(), "TRASH_DAYS=0 purges same-tick entries");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// [H1] §3.9: a freshly-archived worktree whose INHERITED mtime is ancient
    /// (the force-reclaim case — `fs::rename` keeps the source's old mtime) must
    /// SURVIVE the retention window. Age is taken from the archive time embedded
    /// in the dir name, not the mtime. Under the bug (mtime-based) this entry's
    /// 30-day-old mtime would purge it the same sweep.
    ///
    /// Unix-only: forcing an old mtime needs `File::open` on a DIRECTORY, which
    /// fails on Windows (`std::fs::File::open` lacks `FILE_FLAG_BACKUP_SEMANTICS`).
    /// Windows coverage of the name-vs-mtime behaviour is provided by
    /// `purge_deletes_when_embedded_archive_time_is_old_h1` (old embedded-time +
    /// fresh mtime → deleted, which only holds under name-based purging). Mirrors
    /// the existing unix-gated `archive_failure_preserves_worktree`.
    #[cfg(unix)]
    #[test]
    fn purge_uses_embedded_archive_time_not_inherited_mtime_h1() {
        let _env = gc_trash_env_guard();
        let dir = tmp_home("h1-archivetime");
        let trash = trash_root(&dir);
        std::fs::create_dir_all(&trash).unwrap();
        // Name embeds NOW (`{agent}-{secs}-{nanos}`); mtime forced 30 days old.
        let entry = trash.join(format!("dev-fr-{}", archive_ts()));
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("data.txt"), b"recoverable").unwrap();
        let thirty_days_ago = SystemTime::now() - Duration::from_secs(30 * 86400);
        std::fs::File::open(&entry)
            .unwrap()
            .set_modified(thirty_days_ago)
            .unwrap();

        std::env::set_var("AGEND_WORKTREE_GC_TRASH_DAYS", "7");
        purge_trash(&dir);
        std::env::remove_var("AGEND_WORKTREE_GC_TRASH_DAYS");

        assert!(
            entry.exists(),
            "[H1] a fresh archive with an OLD inherited mtime must survive the \
             retention window (age from embedded archive-time, not mtime)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// [H1] §3.9 complement: an entry whose EMBEDDED archive time is old is purged
    /// even though its actual mtime is fresh (just created) — proving purge keys on
    /// the dir-name timestamp, not mtime. Under the bug this fresh-mtime entry
    /// would be wrongly preserved.
    #[test]
    fn purge_deletes_when_embedded_archive_time_is_old_h1() {
        let _env = gc_trash_env_guard();
        let dir = tmp_home("h1-oldname");
        let trash = trash_root(&dir);
        std::fs::create_dir_all(&trash).unwrap();
        let old_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 10 * 86400;
        let entry = trash.join(format!("dev-fr-{old_secs}-000000000"));
        std::fs::create_dir_all(&entry).unwrap(); // fresh mtime (just created)

        std::env::set_var("AGEND_WORKTREE_GC_TRASH_DAYS", "7");
        purge_trash(&dir);
        std::env::remove_var("AGEND_WORKTREE_GC_TRASH_DAYS");

        assert!(
            !entry.exists(),
            "[H1] an entry archived 10d ago (per embedded time) is purged despite a fresh mtime"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// [M3] §3.9: a force-reclaim archive must hold the binding lock — while a
    /// concurrent holder (a re-lease's `bind_full`) holds `.binding.json.lock`,
    /// the force-reclaim must NOT archive the worktree; it proceeds only after the
    /// lock is released.
    #[test]
    fn force_reclaim_archive_blocked_while_binding_lock_held_m3() {
        let dir = tmp_home("m3-lock");
        let repo = setup_git_repo(&dir);
        let wt = add_worktree(&repo, "wt-m3");
        write_binding(&dir, "dev-m3", "feat/m3", &wt);
        let cand = crate::worktree_pool::GcCandidate {
            path: wt.clone(),
            agent: "dev-m3".to_string(),
            reason: "m3 lock test".to_string(),
            kind: crate::worktree_pool::GcKind::ForceReclaim,
        };
        // Hold the SAME binding lock a concurrent re-lease's bind_full would hold.
        let lock_path = crate::paths::runtime_dir(&dir)
            .join("dev-m3")
            .join(".binding.json.lock");
        let guard = crate::store::acquire_file_lock(&lock_path).expect("acquire test lock");

        let dir2 = dir.clone();
        let handle = std::thread::spawn(move || maybe_remove_candidate(&dir2, &cand));

        // While the lock is held, the archive cannot proceed.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            wt.exists(),
            "[M3] force-reclaim must NOT archive while the binding lock is held"
        );

        // Release → force-reclaim proceeds and archives.
        drop(guard);
        let outcome = handle.join().expect("join");
        assert!(
            matches!(outcome, RemovalOutcome::Removed),
            "[M3] force-reclaim proceeds after lock release: {outcome:?}"
        );
        assert!(!wt.exists(), "worktree archived after lock released");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// T4 (#1058): cross-filesystem `EXDEV` fallback — recursive copy
    /// preserves files and subdirectories. The full `EXDEV` branch is hard
    /// to exercise without two filesystems, so this asserts the helper that
    /// implements the fallback directly. Unix-only because the helper is
    /// only compiled on Unix (the `EXDEV` errno match arm in `try_archive`
    /// is `cfg(unix)`-gated).
    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_preserves_files_and_subdirs() {
        let dir = tmp_home("t4-copy");
        let src = dir.join("src");
        let dst = dir.join("dst");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("file.txt"), b"hello").unwrap();
        std::fs::write(src.join("nested/inner.txt"), b"world").unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert!(dst.join("file.txt").exists());
        assert_eq!(std::fs::read(dst.join("file.txt")).unwrap(), b"hello");
        assert!(dst.join("nested/inner.txt").exists());
        assert_eq!(
            std::fs::read(dst.join("nested/inner.txt")).unwrap(),
            b"world"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T5 (#1058): archival invokes `git worktree prune` to clean the
    /// dangling worktree reference from git's internal list.
    #[test]
    fn archival_invokes_git_worktree_prune() {
        let dir = tmp_home("t5-prune");
        let repo = setup_git_repo(&dir);
        let wt = add_worktree(&repo, "wt-prune");

        let result = maybe_remove_candidate(&dir, &clean_cand(&wt, "wt-prune"));
        assert!(matches!(result, RemovalOutcome::Removed));

        let output = std::process::Command::new("git")
            .args(["worktree", "list"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&output.stdout);
        assert!(
            !listing.contains("wt-prune"),
            "git worktree prune ran post-archive: {listing}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T6 (#1058): purge running in the same tick as a fresh archive
    /// preserves the new entry under the default retention window.
    #[test]
    fn purge_preserves_fresh_archive_in_same_tick() {
        let _env = gc_trash_env_guard();
        let dir = tmp_home("t6-concurrent");
        let trash = trash_root(&dir);
        std::fs::create_dir_all(&trash).unwrap();
        let entry = trash.join("just-created");
        std::fs::create_dir_all(&entry).unwrap();

        std::env::remove_var("AGEND_WORKTREE_GC_TRASH_DAYS");
        purge_trash(&dir);

        assert!(
            entry.exists(),
            "fresh archive within retention window preserved"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T7 (#1058): archive failure (permission denied) preserves the
    /// worktree and returns `Skipped`.
    #[cfg(unix)]
    #[test]
    fn archive_failure_preserves_worktree() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_home("t7-permission");
        let repo = setup_git_repo(&dir);
        let wt = add_worktree(&repo, "wt-perm");

        // Make the trash root unwritable so the rename fails.
        let trash = trash_root(&dir);
        std::fs::create_dir_all(&trash).unwrap();
        std::fs::set_permissions(&trash, std::fs::Permissions::from_mode(0o500)).unwrap();

        let result = maybe_remove_candidate(&dir, &clean_cand(&wt, "wt-perm"));

        // Restore permissions for cleanup regardless of test outcome.
        let _ = std::fs::set_permissions(&trash, std::fs::Permissions::from_mode(0o755));

        assert!(wt.exists(), "worktree preserved on archive failure");
        assert!(
            matches!(result, RemovalOutcome::Skipped { .. }),
            "result is Skipped: {result:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T8 (#1170): worktree rebound between gc_candidates enumeration and
    /// maybe_remove_candidate execution → skip archive.
    #[test]
    fn rebound_worktree_skipped_by_binding_recheck() {
        let dir = tmp_home("t8-rebound");
        let repo = setup_git_repo(&dir);
        let wt = add_worktree(&repo, "wt-rebound");

        // Simulate a binding written after gc_candidates ran — the worktree
        // was released at enumeration time but an agent rebound before archive.
        let runtime_dir = crate::paths::runtime_dir(&dir).join("wt-rebound");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(
            runtime_dir.join("binding.json"),
            r#"{"worktree":"/tmp/fake","bound_at":"2026-05-25T00:00:00Z"}"#,
        )
        .unwrap();

        let result = maybe_remove_candidate(&dir, &clean_cand(&wt, "wt-rebound"));
        assert!(
            matches!(result, RemovalOutcome::Skipped { ref reason } if reason == "rebound_since_enumeration"),
            "rebound worktree must be skipped: {result:?}"
        );
        assert!(wt.exists(), "worktree must NOT be archived when rebound");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T9 (#1170): two worktrees for the same agent archived in the same
    /// sweep tick get distinct .trash paths (no collision).
    #[test]
    fn same_agent_same_second_no_trash_collision() {
        let dir = tmp_home("t9-collision");
        let repo = setup_git_repo(&dir);
        let wt1 = add_worktree(&repo, "wt-col-a");
        let wt2 = add_worktree(&repo, "wt-col-b");

        let r1 = maybe_remove_candidate(&dir, &clean_cand(&wt1, "agent-x"));
        let r2 = maybe_remove_candidate(&dir, &clean_cand(&wt2, "agent-x"));
        assert!(matches!(r1, RemovalOutcome::Removed), "first: {r1:?}");
        assert!(matches!(r2, RemovalOutcome::Removed), "second: {r2:?}");

        let trash = trash_root(&dir);
        let entries: Vec<_> = std::fs::read_dir(&trash)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("agent-x-"))
            .collect();
        assert_eq!(
            entries.len(),
            2,
            "two distinct .trash entries for same agent: got {}",
            entries.len()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T10 (#1170): archive_ts produces sub-second granularity.
    #[test]
    fn archive_ts_has_nanosecond_precision() {
        let ts1 = archive_ts();
        let ts2 = archive_ts();
        assert!(
            ts1.contains('-'),
            "archive_ts must contain secs-nanos separator"
        );
        let parts: Vec<&str> = ts1.splitn(2, '-').collect();
        assert_eq!(parts.len(), 2, "must have secs-nanos: {ts1}");
        assert_eq!(parts[1].len(), 9, "nanos must be 9 digits: {ts1}");
        // Two sequential calls should differ (or at least not panic)
        let _ = (ts1, ts2);
    }

    fn write_binding(home: &Path, agent: &str, branch: &str, wt: &Path) {
        let bd = crate::paths::runtime_dir(home).join(agent);
        std::fs::create_dir_all(&bd).unwrap();
        std::fs::write(
            bd.join("binding.json"),
            serde_json::json!({
                "version": 1, "agent": agent, "task_id": "t", "branch": branch,
                "worktree": wt.to_str().unwrap(), "source_repo": "/x",
                "issued_at": "2026-06-05T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();
    }

    /// t-worktree-leak PR-2: a ForceReclaim candidate (binding still present, agent
    /// dead) is archived AND unbound — the never-released binding is cleared.
    #[test]
    fn force_reclaim_archives_and_unbinds() {
        let dir = tmp_home("fr-archive");
        let repo = setup_git_repo(&dir);
        let wt = add_worktree(&repo, "wt-fr");
        write_binding(&dir, "dev-fr", "feat/x", &wt);
        let cand = crate::worktree_pool::GcCandidate {
            path: wt.clone(),
            agent: "dev-fr".to_string(),
            reason: "force-reclaim test".to_string(),
            kind: crate::worktree_pool::GcKind::ForceReclaim,
        };
        let outcome = maybe_remove_candidate(&dir, &cand);
        assert!(matches!(outcome, RemovalOutcome::Removed), "{outcome:?}");
        assert!(!wt.exists(), "worktree archived (moved out)");
        assert!(
            crate::binding::read(&dir, "dev-fr").is_none(),
            "force-reclaim must unbind the never-released binding"
        );
        let trash = dir.join(".trash").join("worktrees");
        assert!(
            std::fs::read_dir(&trash)
                .map(|d| d.flatten().count() > 0)
                .unwrap_or(false),
            "archived worktree is recoverable in .trash"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// t-worktree-leak PR-2 safety #4: a force-reclaim of a branch whose PR is
    /// TERMINAL classifies as `observed_terminal` (= event-release bug → ALERT);
    /// no pr_state classifies as `unknown` (never blindly trusts pr_state).
    #[test]
    fn classify_force_reclaim_terminal_is_event_bug() {
        let home = tmp_home("fr-classify");
        assert_eq!(
            classify_force_reclaim(&home, Some("o/r"), Some("feat/x")),
            "unknown",
            "absent pr_state → unknown, not a false bug-alert"
        );
        let mut s = crate::daemon::pr_state::new_for_branch(
            "o/r",
            "feat/x",
            "sha",
            crate::daemon::pr_state::ReviewClass::Single,
        );
        s.merge_state = crate::daemon::pr_state::MergeState::Merged {
            merge_commit: "c".into(),
            merged_at: "2026-06-05T00:00:00Z".into(),
        };
        crate::daemon::pr_state::save(&home, &s).unwrap();
        assert_eq!(
            classify_force_reclaim(&home, Some("o/r"), Some("feat/x")),
            "observed_terminal",
            "merged PR never released → event-release bug"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// setup_git_repo + commit a .gitignore with `.agend-managed`, so a leased
    /// worktree's marker is reported as `!! .agend-managed` under `--ignored` —
    /// the exact production / reviewer-2 repro condition.
    fn setup_git_repo_marker_ignored(dir: &Path) -> PathBuf {
        let repo = setup_git_repo(dir);
        std::fs::write(repo.join(".gitignore"), ".agend-managed\ntarget/\n").unwrap();
        for args in [
            vec!["add", ".gitignore"],
            vec!["commit", "-m", "gitignore marker"],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .env("AGEND_GIT_BYPASS", "1")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
        }
        repo
    }

    /// reviewer-2 efficacy (§3.9 representative fixture): a REAL `lease()` worktree
    /// carries the gitignored `.agend-managed` marker, which `--ignored` reports as
    /// `!! .agend-managed`. It MUST archive — NOT be skipped as WIP by its own
    /// marker. (The old `add_worktree` fixture had no marker → masked this.)
    #[test]
    fn force_reclaim_real_lease_with_marker_is_archived() {
        let dir = tmp_home("fr-real-lease");
        let repo = setup_git_repo_marker_ignored(&dir);
        let lease = crate::worktree_pool::lease(&dir, &repo, "dev-real", "feat/x").expect("lease");
        assert!(
            crate::binding::read(&dir, "dev-real").is_some(),
            "pre: bound"
        );
        let cand = crate::worktree_pool::GcCandidate {
            path: lease.path.clone(),
            agent: "dev-real".to_string(),
            reason: "fr".to_string(),
            kind: crate::worktree_pool::GcKind::ForceReclaim,
        };
        let outcome = maybe_remove_candidate(&dir, &cand);
        assert!(
            matches!(outcome, RemovalOutcome::Removed),
            "marker-bearing real lease must archive, not be skipped by its own marker: {outcome:?}"
        );
        assert!(!lease.path.exists(), "archived");
        assert!(
            crate::binding::read(&dir, "dev-real").is_none(),
            "unbound after force-reclaim"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The 4th leak root: a RELEASED marker-bearing worktree must also archive
    /// (clean-release uses the same dirty-check).
    #[test]
    fn clean_release_real_lease_with_marker_is_archived() {
        let dir = tmp_home("cr-real-lease");
        let repo = setup_git_repo_marker_ignored(&dir);
        let lease = crate::worktree_pool::lease(&dir, &repo, "dev-cr", "feat/y").expect("lease");
        // Released → binding cleared (clean-release path requires the binding gone).
        crate::binding::unbind(&dir, "dev-cr");
        let cand = crate::worktree_pool::GcCandidate {
            path: lease.path.clone(),
            agent: "dev-cr".to_string(),
            reason: "cr".to_string(),
            kind: crate::worktree_pool::GcKind::CleanRelease,
        };
        let outcome = maybe_remove_candidate(&dir, &cand);
        assert!(
            matches!(outcome, RemovalOutcome::Removed),
            "released marker-bearing worktree must archive (clean-release efficacy): {outcome:?}"
        );
        assert!(!lease.path.exists(), "archived");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// reviewer-2 residual: a BUILT worktree carries in-tree `target/`
    /// (`!! target/`). force-reclaim is always archive-to-trash (recoverable), so
    /// gitignored build output must NOT block it — else every built worktree
    /// (i.e. all of them) no-ops, the same defect the marker caused.
    #[test]
    fn force_reclaim_built_worktree_with_target_is_archived() {
        let dir = tmp_home("fr-built");
        let repo = setup_git_repo_marker_ignored(&dir);
        let lease = crate::worktree_pool::lease(&dir, &repo, "dev-built", "feat/x").expect("lease");
        std::fs::create_dir_all(lease.path.join("target")).unwrap();
        std::fs::write(lease.path.join("target").join("artifact.o"), "build").unwrap();
        let cand = crate::worktree_pool::GcCandidate {
            path: lease.path.clone(),
            agent: "dev-built".to_string(),
            reason: "fr".to_string(),
            kind: crate::worktree_pool::GcKind::ForceReclaim,
        };
        let outcome = maybe_remove_candidate(&dir, &cand);
        assert!(
            matches!(outcome, RemovalOutcome::Removed),
            "built worktree (!! target/) force-reclaim must archive (recoverable): {outcome:?}"
        );
        assert!(!lease.path.exists(), "archived");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// force-reclaim still respects REAL WIP: an untracked (non-ignored) file means
    /// uncommitted operator/agent work → skip (don't archive it away).
    #[test]
    fn force_reclaim_with_real_wip_is_skipped() {
        let dir = tmp_home("fr-wip");
        let repo = setup_git_repo_marker_ignored(&dir);
        let lease = crate::worktree_pool::lease(&dir, &repo, "dev-wip", "feat/x").expect("lease");
        // A non-gitignored untracked file = real WIP (`?? notes.txt`).
        std::fs::write(lease.path.join("notes.txt"), "uncommitted work").unwrap();
        let cand = crate::worktree_pool::GcCandidate {
            path: lease.path.clone(),
            agent: "dev-wip".to_string(),
            reason: "fr".to_string(),
            kind: crate::worktree_pool::GcKind::ForceReclaim,
        };
        let outcome = maybe_remove_candidate(&dir, &cand);
        assert!(
            matches!(outcome, RemovalOutcome::Skipped { .. }),
            "force-reclaim must NOT archive a worktree with real (tracked/untracked) WIP: {outcome:?}"
        );
        assert!(lease.path.exists(), "preserved");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
