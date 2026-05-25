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
    std::env::var("AGEND_WORKTREE_GC_TRASH_DAYS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(7)
}

fn archive_ts() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    format!("{}-{:09}", d.as_secs(), d.subsec_nanos())
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

pub(crate) fn maybe_remove_candidate(home: &Path, path: &Path, agent: &str) -> RemovalOutcome {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let path = path.as_path();
    let status_output =
        crate::git_helpers::git_bypass(path, &["status", "--porcelain=v1", "--ignored"]);
    match status_output {
        Ok(o) if o.status.success() => {
            if !o.stdout.is_empty() {
                let status_text = String::from_utf8_lossy(&o.stdout);
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
    // #1170 TOCTOU fix: re-validate binding state immediately before archive.
    // gc_candidates() checked this earlier, but the worktree may have been
    // rebound between enumeration and now.
    if crate::binding::read(home, agent).is_some() {
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
            let prune = std::process::Command::new("git")
                .args(["worktree", "prune"])
                .current_dir(&repo)
                .env("AGEND_GIT_BYPASS", "1")
                .output();
            if let Err(e) = prune {
                tracing::warn!(error = %e, "git worktree prune failed (non-fatal)");
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
        let mtime = metadata.modified().unwrap_or(now);
        let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
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
        match maybe_remove_candidate(home, &c.path, &c.agent) {
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

        let result = maybe_remove_candidate(&dir, &wt, "wt-ignored");
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

        let result = maybe_remove_candidate(&dir, &wt, "wt-untracked");
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

        let result = maybe_remove_candidate(&dir, &wt, "wt-clean");
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

        let result = maybe_remove_candidate(&dir, &bad_path, "agent");
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

        let result = maybe_remove_candidate(&dir, &wt, "wt-prune");
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

        let result = maybe_remove_candidate(&dir, &wt, "wt-perm");

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

        let result = maybe_remove_candidate(&dir, &wt, "wt-rebound");
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

        let r1 = maybe_remove_candidate(&dir, &wt1, "agent-x");
        let r2 = maybe_remove_candidate(&dir, &wt2, "agent-x");
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
}
