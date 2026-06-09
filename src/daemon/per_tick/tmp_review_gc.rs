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
//! - **STRUCTURAL scope** (t-worktree-leak Class2): a git worktree is eligible
//!   only when its parent repo (resolved from the `.git` gitdir pointer) is a repo
//!   the daemon MANAGES (`bound_source_repos`). This replaces the fragile name
//!   pattern — which missed `agend-pr*` worktrees → the /tmp leak — and the
//!   managed-repo gate guarantees an operator's OWN unrelated `/tmp` git worktree
//!   is never touched, regardless of its name. A non-worktree dir (a review CLONE
//!   with its own `.git` directory) has no resolvable parent, so it falls back to
//!   the name pattern (`pr-<N>` / `*review*`) to avoid regressing #1747's clone
//!   cleanup.
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

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.should_fire() {
            return;
        }
        let removed = gc_stale_tmp_review_worktrees(
            ctx.home,
            &std::env::temp_dir(),
            MIN_AGE,
            SystemTime::now(),
        );
        if removed > 0 {
            tracing::info!(target: "tmp_review_gc", removed,
                "Class2: GC'd stale /tmp worktrees/clones of managed repos");
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

/// Sweep `tmp_root` for stale `/tmp` worktrees/clones of MANAGED repos and remove
/// them. Returns the count removed. `now` is injected so tests drive the age belt
/// deterministically.
///
/// t-worktree-leak Class2: the eligibility gate is STRUCTURAL for git worktrees —
/// resolve the worktree's parent repo from its `.git` gitdir and reclaim it only
/// if that repo is one the daemon manages (`bound_source_repos`). This replaces
/// the fragile name pattern (which missed `agend-pr*` worktrees → /tmp leak) and
/// the managed-repo gate protects an operator's OWN unrelated `/tmp` git worktree
/// from ever being touched. A non-worktree dir (a review CLONE, with its own `.git`
/// directory rather than a gitdir pointer) has no resolvable parent, so it still
/// falls back to the #1747 name pattern — structural can't gate it, and dropping
/// it would regress #1747's clone cleanup.
fn gc_stale_tmp_review_worktrees(
    home: &Path,
    tmp_root: &Path,
    min_age: Duration,
    now: SystemTime,
) -> usize {
    // The managed-repo set (canonicalized) — the gate against reclaiming a user's
    // unrelated /tmp git worktree. Durable for the common case: the canonical repo
    // is bound by the working fleet. Empty (no current bindings) → no worktree is
    // structurally eligible, which is the conservative outcome.
    let managed: std::collections::HashSet<PathBuf> = crate::binding::bound_source_repos(home)
        .into_iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();
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
        let parent_repo = worktree_parent_repo(&path);
        let eligible = match &parent_repo {
            // A git worktree → STRUCTURAL gate: reclaim only if its parent repo is
            // one we manage (never an operator's own /tmp worktree).
            Some(parent) => match parent.canonicalize() {
                Ok(canon) => managed.contains(&canon),
                Err(_) => false,
            },
            // Not a worktree (clone / plain dir) → fall back to the #1747 name gate
            // so review clones are still reaped (structural can't resolve them).
            None => path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_review_pattern),
        };
        if !eligible {
            if parent_repo.is_some() {
                tracing::debug!(target: "tmp_review_gc", path = %path.display(),
                    "skip: /tmp worktree's parent repo is not daemon-managed (operator-owned)");
            }
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
                parent_repo = ?parent_repo.as_ref().map(|p| p.display().to_string()),
                age_days = age.as_secs() / 86_400,
                "Class2: removed stale /tmp worktree/clone of a managed repo (structural)");
        } else {
            tracing::warn!(target: "tmp_review_gc", path = %path.display(),
                "Class2: failed to remove stale /tmp worktree/clone");
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
        // #1899: bounded via git_bypass (LOCAL 60s) — a stuck `worktree remove`
        // → not-removed (false), then the remove_dir_all fallback below.
        let path_str = path.display().to_string();
        let removed = crate::git_helpers::git_bypass(
            &parent_repo,
            &["worktree", "remove", "--force", &path_str],
        )
        .map(|o| o.status.success())
        .unwrap_or(false);
        if removed {
            return true;
        }
        // Fallback: remove the dir directly, then prune so the source repo
        // doesn't keep a dangling worktree registration.
        let rm = std::fs::remove_dir_all(path).is_ok();
        // #1899: bounded via git_bypass (LOCAL 60s) — best-effort prune.
        let _ = crate::git_helpers::git_bypass(&parent_repo, &["worktree", "prune"]);
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

    fn git(cwd: &Path, args: &[&str]) {
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
    }

    /// A real git repo with one commit.
    fn make_repo(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path).unwrap();
        git(path, &["init", "-b", "main"]);
        git(path, &["commit", "--allow-empty", "-m", "init"]);
        path.to_path_buf()
    }

    /// A REAL git worktree of `repo` at `wt` (`.git` is a gitdir pointer back to
    /// the repo) — not a synthetic path (§3.9).
    fn add_real_worktree(repo: &Path, wt: &Path) {
        git(
            repo,
            &["worktree", "add", "--detach", wt.to_str().unwrap(), "HEAD"],
        );
    }

    /// Write a binding for `agent` whose `source_repo` points at `repo`, so
    /// `bound_source_repos(home)` reports `repo` as managed.
    fn write_binding(home: &Path, agent: &str, repo: &Path) {
        let bd = crate::paths::runtime_dir(home).join(agent);
        std::fs::create_dir_all(&bd).unwrap();
        std::fs::write(
            bd.join("binding.json"),
            serde_json::json!({ "source_repo": repo.to_str().unwrap() }).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn gc_removes_old_clone_dir_via_name_fallback() {
        // A review CLONE (plain dir, no gitdir pointer) → name-fallback path.
        let root = tmp_root("old-del");
        let home = tmp_root("old-del-home");
        let d = root.join("pr-9999");
        std::fs::create_dir_all(&d).unwrap();
        let now = SystemTime::now() + Duration::from_secs(3 * 24 * 60 * 60);
        let removed = gc_stale_tmp_review_worktrees(&home, &root, MIN_AGE, now);
        assert_eq!(removed, 1);
        assert!(!d.exists(), "stale review-name clone dir must be removed");
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_skips_recent_clone_dir() {
        let root = tmp_root("recent-skip");
        let home = tmp_root("recent-skip-home");
        let d = root.join("pr1234-review");
        std::fs::create_dir_all(&d).unwrap();
        let removed = gc_stale_tmp_review_worktrees(&home, &root, MIN_AGE, SystemTime::now());
        assert_eq!(removed, 0);
        assert!(d.exists(), "recently-touched review dir must be kept");
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_ignores_non_pattern_and_files() {
        let root = tmp_root("nonpattern");
        let home = tmp_root("nonpattern-home");
        let unrelated = root.join("some-build-cache");
        std::fs::create_dir_all(&unrelated).unwrap();
        let review_file = root.join("notes-review.md");
        std::fs::write(&review_file, b"x").unwrap();
        let now = SystemTime::now() + Duration::from_secs(3 * 24 * 60 * 60);
        let removed = gc_stale_tmp_review_worktrees(&home, &root, MIN_AGE, now);
        assert_eq!(removed, 0);
        assert!(unrelated.exists(), "non-pattern dir must be untouched");
        assert!(
            review_file.exists(),
            "review-named file (not a dir) must be untouched"
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// §3.9 representative: REAL /tmp git worktrees. A worktree of a MANAGED repo
    /// (named `agend-pr*` — which the OLD name pattern MISSED) is pruned
    /// structurally; an operator's OWN /tmp worktree (unmanaged repo) is NOT
    /// touched, even though it is also old and `agend-pr*`-named.
    #[test]
    fn structural_prunes_managed_worktree_spares_operator_worktree() {
        let root = tmp_root("structural");
        let home = tmp_root("structural-home");
        let managed_repo = make_repo(&home.join("managed-repo"));
        write_binding(&home, "dev", &managed_repo);
        let user_repo = make_repo(&home.join("user-repo")); // no binding → unmanaged

        let managed_wt = root.join("agend-pr123"); // name the OLD pattern missed
        add_real_worktree(&managed_repo, &managed_wt);
        let user_wt = root.join("agend-pr999-user");
        add_real_worktree(&user_repo, &user_wt);
        assert!(worktree_parent_repo(&managed_wt).is_some(), "real worktree");
        assert!(worktree_parent_repo(&user_wt).is_some(), "real worktree");

        let now = SystemTime::now() + Duration::from_secs(3 * 24 * 60 * 60);
        let removed = gc_stale_tmp_review_worktrees(&home, &root, MIN_AGE, now);
        assert_eq!(removed, 1, "only the managed-repo worktree is pruned");
        assert!(
            !managed_wt.exists(),
            "managed-repo /tmp worktree (agend-pr*, name-missed) pruned structurally"
        );
        assert!(
            user_wt.exists(),
            "operator's OWN /tmp worktree (unmanaged repo) must never be reclaimed"
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// A managed-repo worktree that is RECENT (in-progress review) is spared by the
    /// age belt even though it is structurally eligible.
    #[test]
    fn structural_age_belt_spares_recent_managed_worktree() {
        let root = tmp_root("structural-age");
        let home = tmp_root("structural-age-home");
        let managed_repo = make_repo(&home.join("managed-repo"));
        write_binding(&home, "dev", &managed_repo);
        let wt = root.join("agend-pr-fresh");
        add_real_worktree(&managed_repo, &wt);
        // now = real now → age ~0 < 2d → skipped.
        let removed = gc_stale_tmp_review_worktrees(&home, &root, MIN_AGE, SystemTime::now());
        assert_eq!(removed, 0, "recent managed worktree spared by age belt");
        assert!(wt.exists());
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
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
