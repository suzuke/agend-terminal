//! #817 daemon-side stale local branch cleanup.
//!
//! Operator-triggered hygiene sweep that categorizes local branches
//! into 4 buckets (`clean_merged`, `squash_merged`, `stale_idle`,
//! `active_unknown`) and offers a dry-run + confirm-subset workflow
//! to delete the safe ones. Mirrors the `tasks::sweep_impl` pattern
//! from #806 — same `dry-run + confirm_ids + system identity +
//! audit_reason` shape — but operates on local git refs instead of
//! the task board.
//!
//! No GitHub API dependency. Everything is local `git for-each-ref` /
//! `git cherry` / `git branch -D` subprocess. Cache layer (in-memory
//! `HashMap` per sweep) dedups repeated cherry calls when branches
//! share ancestry.
//!
//! Safety stack (mirrors #806 + force-delete-specific layers):
//! - `system:branch_sweep` identity (allow-list at tasks.rs:485)
//! - dry-run default; apply requires explicit `apply=true`
//! - `confirm_ids` MUST be subset of `candidate_ids` from prior dry-run
//! - `audit_reason` required, non-empty
//! - `active_unknown` bucket skipped unless operator explicitly picks
//!   those IDs (the bucket itself surfaces in dry-run for visibility)
//! - `event_log.jsonl` records `branch=<name> source=<sha>` so an
//!   operator can `git branch <name> <sha>` to restore

use std::path::Path;

/// Threshold for `stale_idle` category. Branches whose tip commit
/// committer-date is older than this AND not merged AND not squash-
/// merged land in `stale_idle`. Operator can override via
/// `min_age_days` arg on the MCP call. Dead-code allow lifts at C3
/// when the MCP handler reads the default.
#[allow(dead_code)]
pub(crate) const STALE_IDLE_DEFAULT_DAYS: i64 = 90;

/// Lightweight enumeration of a local branch — what `git for-each-ref`
/// returns. The category is computed separately via per-branch
/// `git cherry` / `git branch --merged` checks.
#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
pub(crate) struct BranchInfo {
    pub name: String,
    pub tip_sha: String,
    /// RFC3339 committer date of the branch tip.
    pub committer_date: String,
}

/// Categorization bucket. Each non-terminal local branch lands in
/// exactly one bucket (first match wins, order: clean_merged →
/// squash_merged → stale_idle → active_unknown).
#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
pub(crate) struct Candidate {
    pub name: String,
    pub tip_sha: String,
    pub reason: String,
}

#[derive(Debug, Default, serde::Serialize)]
#[allow(dead_code)]
pub(crate) struct Categories {
    pub clean_merged: Vec<Candidate>,
    pub squash_merged: Vec<Candidate>,
    pub stale_idle: Vec<Candidate>,
    pub active_unknown: Vec<Candidate>,
}

#[allow(dead_code)]
impl Categories {
    /// Concatenated sorted list of all candidate branch names across
    /// the 3 deletable buckets (clean_merged + squash_merged +
    /// stale_idle). `active_unknown` is NOT in this default list —
    /// the operator must explicitly pick those IDs by their bucket.
    pub fn deletable_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .clean_merged
            .iter()
            .chain(self.squash_merged.iter())
            .chain(self.stale_idle.iter())
            .map(|c| c.name.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Total IDs including the explicit-opt-in `active_unknown`
    /// bucket. Used to validate confirm_ids subset — operator CAN
    /// pick active_unknown IDs, they just don't show up in the
    /// default `deletable_ids`.
    pub fn all_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .clean_merged
            .iter()
            .chain(self.squash_merged.iter())
            .chain(self.stale_idle.iter())
            .chain(self.active_unknown.iter())
            .map(|c| c.name.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn total(&self) -> usize {
        self.all_ids().len()
    }
}

/// #817 stub — real implementation lands in C2. Pre-fix returns
/// empty Categories so the C1 RED tests fail at runtime (asserting
/// non-empty buckets) rather than failing to compile.
#[allow(dead_code)]
pub(crate) fn scan(
    _repo: &Path,
    _base: &str,
    _min_age_days: i64,
    _now: chrono::DateTime<chrono::Utc>,
) -> Result<Categories, String> {
    Ok(Categories::default())
}

/// #817 stub — real implementation lands in C2. Pre-fix returns 0
/// (no deletions) so the C4 GREEN tests fail at runtime.
#[allow(dead_code)]
pub(crate) fn emit_delete_batch(
    _home: &Path,
    _repo: &Path,
    _categories: &Categories,
    _confirm_ids: &std::collections::HashSet<String>,
    _audit_reason: &str,
) -> Result<usize, String> {
    Ok(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, dead_code)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Spawn a temp git repo scoped to `tag`. The repo has an initial
    /// commit on `main` + pinned per-repo gitconfig (`user.name`/
    /// `user.email`) so subsequent git subprocess calls don't fail
    /// with "unable to auto-detect email address" under CI runners
    /// that lack a global ~/.gitconfig. Mirrors #814 r1's CI
    /// portability fix.
    ///
    /// Returns the repo dir path.
    pub(super) fn setup_repo(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("agend-817-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&base).ok();
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).ok();
        git_run(&repo, &["init", "-b", "main"]);
        git_run(&repo, &["config", "user.name", "test"]);
        git_run(&repo, &["config", "user.email", "t@t"]);
        git_run(&repo, &["commit", "--allow-empty", "-m", "main: initial"]);
        repo
    }

    /// Run git with predictable env. `GIT_AUTHOR_DATE` /
    /// `GIT_COMMITTER_DATE` callers use `git_run_dated` instead.
    pub(super) fn git_run(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git ran")
    }

    /// Run git with explicit author + committer date for back-dating
    /// commits. Used by stale_idle tests to plant commits N days in
    /// the past without `chrono::Utc::now() - duration` arithmetic
    /// (flaky near day boundaries).
    pub(super) fn git_run_dated(
        dir: &Path,
        args: &[&str],
        date_rfc3339: &str,
    ) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_AUTHOR_DATE", date_rfc3339)
            .env("GIT_COMMITTER_DATE", date_rfc3339)
            .output()
            .expect("git ran")
    }

    /// Helper: create a branch off main with one commit. Returns the
    /// branch tip SHA.
    pub(super) fn create_branch_with_commit(repo: &Path, branch: &str, commit_msg: &str) -> String {
        git_run(repo, &["checkout", "-b", branch]);
        let file = repo.join(format!("{branch}.txt"));
        std::fs::write(&file, format!("content for {branch}\n")).expect("write");
        git_run(repo, &["add", &format!("{branch}.txt")]);
        git_run(repo, &["commit", "-m", commit_msg]);
        let sha = String::from_utf8_lossy(&git_run(repo, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        git_run(repo, &["checkout", "main"]);
        sha
    }

    #[test]
    fn test_branch_sweep_scan_categorizes_clean_merged() {
        // #817 RED 1: branch "feat-a" merged into main via a merge
        // commit lands in `clean_merged` (git branch --merged main
        // includes it). Stub returns empty Categories → assertion
        // fails. C2 lands the real scan that picks it up.
        let repo = setup_repo("clean_merged");
        create_branch_with_commit(&repo, "feat-a", "feat: a");
        // Merge via a no-fast-forward merge so a merge commit exists.
        git_run(&repo, &["merge", "--no-ff", "-m", "merge feat-a", "feat-a"]);
        // Branch still exists locally after merge.

        let now = chrono::Utc::now();
        let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
        assert!(
            cats.clean_merged.iter().any(|c| c.name == "feat-a"),
            "clean_merged must include feat-a, got: {cats:?}"
        );
        // Not in other buckets.
        assert!(!cats.squash_merged.iter().any(|c| c.name == "feat-a"));
        assert!(!cats.stale_idle.iter().any(|c| c.name == "feat-a"));
        assert!(!cats.active_unknown.iter().any(|c| c.name == "feat-a"));

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn test_branch_sweep_scan_categorizes_squash_merged() {
        // #817 RED 2: branch "feat-b" whose commit was squash-applied
        // to main (cherry-pick or `git commit --amend -m` after merge)
        // — `git cherry main feat-b` returns lines all prefixed `-`.
        // Stub returns empty → assertion fails. C2 lands cherry
        // detection.
        let repo = setup_repo("squash_merged");
        create_branch_with_commit(&repo, "feat-b", "feat: b body");
        // Apply the same patch to main without merge (squash style).
        git_run(&repo, &["cherry-pick", "feat-b"]);
        // Branch still exists; HEAD on main has equivalent patch.

        let now = chrono::Utc::now();
        let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
        assert!(
            cats.squash_merged.iter().any(|c| c.name == "feat-b"),
            "squash_merged must include feat-b, got: {cats:?}"
        );
        assert!(!cats.clean_merged.iter().any(|c| c.name == "feat-b"));

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[test]
    fn test_branch_sweep_scan_categorizes_stale_idle() {
        // #817 RED 3: branch "old-wip" with committer-date 100 days
        // in the past, not merged, not squash-merged → stale_idle.
        // Uses GIT_AUTHOR_DATE/COMMITTER_DATE env to back-date the
        // commit (NOT chrono arithmetic — flaky near day boundary).
        let repo = setup_repo("stale_idle");
        // Back-date by 100 days from a fixed reference point.
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-15T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let old_date = (now - chrono::Duration::days(100)).to_rfc3339();
        git_run(&repo, &["checkout", "-b", "old-wip"]);
        std::fs::write(repo.join("wip.txt"), "wip content\n").expect("write");
        git_run(&repo, &["add", "wip.txt"]);
        git_run_dated(&repo, &["commit", "-m", "WIP: stale work"], &old_date);
        git_run(&repo, &["checkout", "main"]);

        let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
        assert!(
            cats.stale_idle.iter().any(|c| c.name == "old-wip"),
            "stale_idle must include old-wip (100d > 90d threshold), got: {cats:?}"
        );
        // NOT merged + NOT squash-merged.
        assert!(!cats.clean_merged.iter().any(|c| c.name == "old-wip"));
        assert!(!cats.squash_merged.iter().any(|c| c.name == "old-wip"));

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }
}
