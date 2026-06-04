//! #1747: periodic GC for stale `/tmp` review/audit worktrees + clones.
//!
//! Review/audit skills (`/structural-review`, `/code-review`) and ad-hoc
//! reviewer flows create temporary git worktrees + clones under `/tmp`
//! (`pr-<N>`, `review-pr-<N>`, `pr<N>-review*`) against a source repo but never
//! clean them up — and they fall OUTSIDE the registry-scoped
//! `worktree_cleanup::sweep_from_registry`, which only scans daemon-managed
//! worktrees under `$AGEND_HOME/worktrees`. 18 such worktrees at ~3GB each
//! (43.9GB) were observed accumulating until disk pressure (#1747).
//!
//! This handler is the daemon-side BACKSTOP. The true root fix — the
//! skills/reviewers releasing their own worktrees on completion — lives at the
//! skill/reviewer layer (tracked separately for the operator).
//!
//! Belt (destructive — deletes `/tmp` dirs and deregisters worktrees in a
//! possibly operator-owned source repo, so it is deliberately strict):
//! - **pattern-scope ONLY** (`pr-<N>` / `*review*` directory names) — NEVER a
//!   bare `/tmp` sweep, so unrelated `/tmp` content is never touched.
//! - **mtime-age > [`MIN_AGE`] (2 days)** — `mtime`, not creation time, so an
//!   in-progress review (recently-touched worktree) is naturally skipped.
//! - **directories only** — stray review *files* (e.g. `*-review.md` audit
//!   artifacts) are left alone.
//!
//! A matched git worktree is removed via the parent repo's
//! `git worktree remove --force` (deregister + delete); a plain dir / 0B leftover
//! via `remove_dir_all`.

use super::{PerTickHandler, TickContext};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Minimum `mtime`-age before a review-pattern `/tmp` dir is GC-eligible.
/// Generous (2 days) so an in-progress or just-finished review is never reaped —
/// a live review worktree is touched far more recently than this, and disk
/// runway is wide (operator confirmed ~246GB free), so this is purely a
/// growth-bounding backstop, not an urgent reclaim.
const MIN_AGE: Duration = Duration::from_secs(2 * 24 * 60 * 60);

pub(crate) struct TmpReviewGcHandler {
    every_n_ticks: u64,
    counter: AtomicU64,
}

impl TmpReviewGcHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            every_n_ticks,
            counter: AtomicU64::new(0),
        }
    }

    /// `fetch_add` returns the prev value, so the first tick (counter=0) fires.
    fn should_fire(&self) -> bool {
        self.counter
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(self.every_n_ticks)
    }
}

impl PerTickHandler for TmpReviewGcHandler {
    fn name(&self) -> &'static str {
        "tmp_review_gc"
    }

    fn run(&self, _ctx: &TickContext<'_>) {
        if !self.should_fire() {
            return;
        }
        let removed =
            gc_stale_tmp_review_worktrees(&std::env::temp_dir(), MIN_AGE, SystemTime::now());
        if removed > 0 {
            tracing::info!(target: "tmp_review_gc", removed,
                "#1747: GC'd stale /tmp review worktrees/clones");
        }
    }
}

/// STRICT name predicate — the only guard against a bare `/tmp` sweep. Matches
/// the observed review-worktree/clone names: `pr-<digit>…` (e.g. `pr-1758`) or
/// any name containing `review` (`review-pr-1746`, `pr1288-review`,
/// `pr1596-review-clone`, `pr903-review-TwW3QF`, `pr1264-review`).
fn is_review_pattern(name: &str) -> bool {
    let pr_dash_num = name
        .strip_prefix("pr-")
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
    pr_dash_num || name.contains("review")
}

/// Sweep `tmp_root` for stale review-pattern directories and remove them.
/// Returns the count removed. `now` is injected so tests drive the age belt
/// deterministically.
fn gc_stale_tmp_review_worktrees(tmp_root: &Path, min_age: Duration, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(tmp_root) else {
        return 0;
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        // Directories only — stray review *files* are left alone.
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_review_pattern(name) {
            continue;
        }
        // mtime-age belt — a recently-touched (in-progress) review is skipped.
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let age = now.duration_since(mtime).unwrap_or_default();
        if age < min_age {
            continue;
        }
        if remove_tmp_worktree(&path) {
            removed += 1;
            tracing::info!(target: "tmp_review_gc", path = %path.display(),
                age_days = age.as_secs() / 86_400,
                "#1747: removed stale /tmp review worktree/clone");
        } else {
            tracing::warn!(target: "tmp_review_gc", path = %path.display(),
                "#1747: failed to remove stale /tmp review worktree");
        }
    }
    removed
}

/// Remove a matched `/tmp` dir. If it is a git worktree (a `.git` gitdir
/// pointer file), deregister it via the parent repo's
/// `git worktree remove --force` so the source repo's worktree list stays clean;
/// otherwise just `remove_dir_all`. Returns true on success.
fn remove_tmp_worktree(path: &Path) -> bool {
    if let Some(parent_repo) = worktree_parent_repo(path) {
        let removed = std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .current_dir(&parent_repo)
            .args(["worktree", "remove", "--force"])
            .arg(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if removed {
            return true;
        }
        // Fallback: remove the dir directly, then prune so the source repo
        // doesn't keep a dangling worktree registration.
        let rm = std::fs::remove_dir_all(path).is_ok();
        let _ = std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .current_dir(&parent_repo)
            .args(["worktree", "prune"])
            .status();
        return rm;
    }
    std::fs::remove_dir_all(path).is_ok()
}

/// If `path` is a git worktree, resolve its parent repo from the `.git` gitdir
/// pointer (`gitdir: <repo>/.git/worktrees/<name>` → `<repo>`). Returns `None`
/// for a plain directory (no `.git` file / unexpected shape).
fn worktree_parent_repo(path: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(path.join(".git")).ok()?;
    let gitdir = content.strip_prefix("gitdir:")?.trim();
    let marker = "/.git/worktrees/";
    let idx = gitdir.find(marker)?;
    Some(PathBuf::from(&gitdir[..idx]))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agend-1747-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn is_review_pattern_matches_observed_names() {
        for ok in [
            "pr-1758",
            "review-pr-1746",
            "pr1288-review",
            "pr1596-review-clone",
            "pr903-review-TwW3QF",
            "pr1264-review",
            "fixup-reviewer-pr1596",
        ] {
            assert!(is_review_pattern(ok), "{ok} should match");
        }
        for no in [
            "pr-abc",    // pr- but not followed by a digit
            "project-x", // unrelated
            "some-build-cache",
            "agend-admin-123",
        ] {
            assert!(!is_review_pattern(no), "{no} should NOT match");
        }
    }

    #[test]
    fn gc_removes_old_pattern_dir() {
        let root = tmp_root("old-del");
        let d = root.join("pr-9999");
        std::fs::create_dir_all(&d).unwrap();
        // now = created + 3d → age 3d > 2d → removed.
        let now = SystemTime::now() + Duration::from_secs(3 * 24 * 60 * 60);
        let removed = gc_stale_tmp_review_worktrees(&root, MIN_AGE, now);
        assert_eq!(removed, 1);
        assert!(!d.exists(), "stale review-pattern dir must be removed");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn gc_skips_recent_pattern_dir() {
        let root = tmp_root("recent-skip");
        let d = root.join("pr1234-review");
        std::fs::create_dir_all(&d).unwrap();
        // now = real now → age ~0 < 2d → skipped (in-progress review).
        let removed = gc_stale_tmp_review_worktrees(&root, MIN_AGE, SystemTime::now());
        assert_eq!(removed, 0);
        assert!(d.exists(), "recently-touched review dir must be kept");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn gc_ignores_non_pattern_and_files() {
        let root = tmp_root("nonpattern");
        let unrelated = root.join("some-build-cache");
        std::fs::create_dir_all(&unrelated).unwrap();
        // A review-named FILE (not a dir) must also be left alone.
        let review_file = root.join("notes-review.md");
        std::fs::write(&review_file, b"x").unwrap();
        // Old enough that ONLY the pattern/dir guards keep them.
        let now = SystemTime::now() + Duration::from_secs(3 * 24 * 60 * 60);
        let removed = gc_stale_tmp_review_worktrees(&root, MIN_AGE, now);
        assert_eq!(removed, 0);
        assert!(unrelated.exists(), "non-pattern dir must be untouched");
        assert!(
            review_file.exists(),
            "review-named file (not a dir) must be untouched"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn worktree_parent_repo_parses_gitdir_pointer() {
        let root = tmp_root("gitdir");
        let wt = root.join("pr-1");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            "gitdir: /Users/op/Documents/Hack/agend-terminal/.git/worktrees/pr-1\n",
        )
        .unwrap();
        assert_eq!(
            worktree_parent_repo(&wt),
            Some(PathBuf::from("/Users/op/Documents/Hack/agend-terminal"))
        );
        // plain dir (no .git) → None
        let plain = root.join("pr-2-review");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(worktree_parent_repo(&plain), None);
        std::fs::remove_dir_all(&root).ok();
    }
}
