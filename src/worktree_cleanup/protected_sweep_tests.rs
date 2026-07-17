#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::tests::{git_commit_dated, git_in, setup_test_repo_with_default, ENV_LOCK};
use super::*;
use std::path::Path;

fn make_old_dated_branch_from(repo: &Path, base: &str, branch: &str, tip_date: &str) {
    git_in(repo, &["checkout", "-b", branch]);
    std::fs::write(repo.join("feat.txt"), branch).ok();
    git_in(repo, &["add", "."]);
    git_commit_dated(repo, "feature work", tip_date);
    git_in(repo, &["checkout", base]);
}

#[test]
fn default_dev_canonical_worktree_is_not_a_phase1_candidate_2830_red() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo_with_default("2830-default-dev-worktree", "dev");
    let canonical_repo = repo.canonicalize().unwrap();
    let entries = list_worktrees(&repo).expect("worktree enumeration");
    assert!(
        !entries.iter().any(|entry| {
            entry.branch == "dev"
                && Path::new(&entry.path)
                    .canonicalize()
                    .ok()
                    .is_some_and(|path| path == canonical_repo)
        }),
        "the canonical default worktree must never enter phase-1 cleanup candidates: {entries:?}"
    );
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn default_dev_is_never_reported_as_merged_2830_red() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo_with_default("2830-default-dev-merged", "dev");
    std::fs::write(repo.join("default-tip.txt"), "dev").unwrap();
    git_in(&repo, &["add", "."]);
    git_commit_dated(&repo, "old default tip", "2024-01-01T00:00:00 +0000");
    assert!(
        !is_branch_merged(&repo, "dev"),
        "the actual default branch must never be considered merged into itself"
    );
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn unoccupied_local_release_main_survives_phase2_with_default_dev_2830_red() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo_with_default("2830-release-main", "dev");
    std::fs::write(repo.join("default-tip.txt"), "dev").unwrap();
    git_in(&repo, &["add", "."]);
    git_commit_dated(&repo, "old default tip", "2024-01-01T00:00:00 +0000");
    git_in(&repo, &["branch", "main"]);

    let pruned = prune_orphaned_branches(&repo, false);
    assert!(
        !pruned.iter().any(|(branch, _)| branch == "main"),
        "the legacy release branch must survive phase 2 even when default=dev: {pruned:?}"
    );
    assert!(
        crate::git_helpers::git_ok(&repo, &["rev-parse", "--verify", "main"]),
        "phase 2 must not delete the unoccupied local release main branch"
    );
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn ordinary_merged_disposable_branch_remains_reapable_2830_red() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo_with_default("2830-disposable-merged", "dev");
    make_old_dated_branch_from(&repo, "dev", "feat/disposable", "2024-01-01T00:00:00 +0000");
    git_in(&repo, &["merge", "--ff-only", "feat/disposable"]);

    let pruned = prune_orphaned_branches(&repo, false);
    assert!(
        pruned
            .iter()
            .any(|(branch, reason)| branch == "feat/disposable" && *reason == "merged"),
        "an ordinary merged disposable branch must remain reapable: {pruned:?}"
    );
    assert!(
        !crate::git_helpers::git_ok(&repo, &["rev-parse", "--verify", "feat/disposable"]),
        "the ordinary merged disposable branch should be removed"
    );
    std::fs::remove_dir_all(&repo).ok();
}
