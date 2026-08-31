//! Review-repro tests (scope: worktree-git) attached to `worktree_pool.rs`.
//!
//! Each test encodes the CORRECT expected behavior for a confirmed code-review
//! finding and is GREEN on current code (the fix has landed); they run
//! un-ignored as live regression guards locking the behavior in.
//!
//! Placement: in-module submodule so the private `cleanup_merged_branch`
//! and `evaluate_candidate` are reachable via `super::`.

#![allow(clippy::expect_used)]

use super::{cleanup_merged_branch, evaluate_candidate, MANAGED_MARKER};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Unique scratch dir under the system temp.
fn scratch(tag: &str) -> PathBuf {
    crate::review_repro_test_util::scratch("agend-wtpool-reprowg", tag)
}

/// Run `git` with the daemon bypass env (mirrors the in-module test harness).
fn git(dir: &Path, args: &[&str]) {
    crate::review_repro_test_util::review_repro_git(dir, args)
}

// ──────────────────────────────────────────────────────────────────────────
// Finding #1 (medium): cleanup_merged_branch keys its merge decision on
// default_branch(), which SILENTLY falls back to the hard-coded "main" when
// it cannot read refs/remotes/<remote>/HEAD. In a repo whose REAL default is
// `develop` (no origin/HEAD), a feature branch fully merged into `develop`
// is NOT an ancestor of the (wrong) `main`, so `is_merged` is false and the
// dry-run reports "branch not merged into main" instead of "would delete".
// CORRECT behavior: the merged feature branch is recognized against the
// repo's true default branch.
// ──────────────────────────────────────────────────────────────────────────

#[test]
fn cleanup_merged_branch_uses_true_default_not_main_worktree_git() {
    let repo = scratch("default-branch-develop");
    // A repo whose TRUE default branch is `develop`, with NO remote (so
    // refs/remotes/origin/HEAD does not exist → default_branch() falls back
    // to "main"). `feat/merged` is fully merged into `develop`.
    git(&repo, &["init", "-b", "develop"]);
    git(&repo, &["commit", "--allow-empty", "-m", "init"]);
    git(&repo, &["branch", "feat/merged"]);
    git(&repo, &["checkout", "feat/merged"]);
    git(&repo, &["commit", "--allow-empty", "-m", "work"]);
    git(&repo, &["checkout", "develop"]);
    git(&repo, &["merge", "--no-edit", "feat/merged"]);

    // Observation-only dry-run: never mutates refs, so it is safe to assert
    // the decision text without deleting anything.
    let (deleted, reason) = cleanup_merged_branch(&repo, "feat/merged", true, None);
    assert!(
        !deleted,
        "dry-run must never delete (deleted should be false): {reason:?}"
    );
    let reason = reason.unwrap_or_default();
    // A branch genuinely merged into the repo's TRUE default must be detected
    // as merged → the dry-run preview must say it WOULD delete. Pre-fix the
    // misdetected "main" makes is_merged=false → "branch not merged into main".
    assert!(
        reason.contains("would delete"),
        "branch merged into the TRUE default (`develop`) must be recognized as \
         merged; default_branch() must not silently fall back to `main`. Got: {reason:?}"
    );
    assert!(
        !reason.contains("not merged into main"),
        "the hard-coded `main` fallback must not drive the merge decision when \
         the real default is `develop`. Got: {reason:?}"
    );

    std::fs::remove_dir_all(&repo).ok();
}

// ──────────────────────────────────────────────────────────────────────────
// Finding #6 (low): evaluate_candidate must not infer ownership from the
// worktree path when the `.agend-managed` marker lacks an `agent=` line. For a
// SLASH branch (the common case), path-derived ownership could be ambiguous or
// point at the wrong instance. CORRECT: destructive GC fails closed without the
// marker's authoritative owner.
// ──────────────────────────────────────────────────────────────────────────

#[test]
fn evaluate_candidate_without_authoritative_owner_fails_closed_worktree_git() {
    let home = scratch("gc-agent-slash");
    let real_agent = "agent-real";
    // New-layout slash-branch worktree: <home>/worktrees/<agent>/feat/track-x
    let wt = super::daemon_managed_worktree_root(&home)
        .join(real_agent)
        .join("feat")
        .join("track-x");
    std::fs::create_dir_all(&wt).expect("mkdir worktree");

    // CORRUPT marker: NO `agent=` line. Even though the worktree is old enough
    // for force-reclaim, the missing authoritative owner must suppress it.
    let old = (chrono::Utc::now() - chrono::Duration::days(60)).to_rfc3339();
    std::fs::write(
        wt.join(MANAGED_MARKER),
        format!("branch=feat/track-x\nleased_at={old}\n"),
    )
    .expect("write marker");

    // No live agents, no daemon run dir (so not in boot grace), and no binding;
    // the age/liveness gates would otherwise make this force-reclaim eligible.
    let live: HashSet<String> = HashSet::new();
    assert!(
        evaluate_candidate(&home, &wt, &live).is_none(),
        "GC must fail closed when the marker has no authoritative owner"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ──────────────────────────────────────────────────────────────────────────
// t-...50899-10 (CR-2026-06-14 parity, data-loss): cleanup_merged_branch's
// delete gate was `!is_merged && !is_gone` — a remote-tracking ref being gone
// was an INDEPENDENT delete trigger, with no squash verification (unlike
// worktree_cleanup.rs's prune_orphaned_branches, already fixed by
// CR-2026-06-14 to require `merged || is_squash_gc_eligible`). A branch
// pushed once, then its remote deleted (e.g. squash-merge on GitHub), that
// keeps accruing LOCAL commits never re-pushed was reaped by `git branch -D`
// — those unmerged local commits are unrecoverable. CORRECT: a remote-gone
// branch carrying commits NOT reachable from default must be KEPT.
// ──────────────────────────────────────────────────────────────────────────

#[test]
fn cleanup_merged_branch_keeps_remote_gone_branch_with_unpushed_commits_worktree_git() {
    // A bare "remote" + a clone, so `branch.<name>.remote` is real.
    let remote = scratch("cmb-remote-gone-bare");
    git(&remote, &["init", "--bare", "-b", "main"]);

    let repo = scratch("cmb-remote-gone-clone");
    std::process::Command::new("git")
        .args([
            "clone",
            &remote.display().to_string(),
            &repo.display().to_string(),
        ])
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git clone");

    git(&repo, &["commit", "--allow-empty", "-m", "init"]);
    git(&repo, &["push", "-u", "origin", "main"]);

    // Feature branch: push it (configures upstream), then add MORE local
    // commits that are never re-pushed.
    git(&repo, &["checkout", "-b", "feat/unpushed"]);
    git(&repo, &["commit", "--allow-empty", "-m", "first (pushed)"]);
    git(&repo, &["push", "-u", "origin", "feat/unpushed"]);
    git(
        &repo,
        &["commit", "--allow-empty", "-m", "unpushed local work"],
    );
    git(&repo, &["checkout", "main"]);

    // Remote head deleted (e.g. PR closed / branch removed) — cleanup_merged_branch
    // itself runs `fetch --prune` on the non-dry-run path, so is_gone will be true.
    git(&remote, &["branch", "-D", "feat/unpushed"]);

    let (deleted, reason) = cleanup_merged_branch(&repo, "feat/unpushed", false, None);
    assert!(
        !deleted,
        "must NOT delete a remote-gone branch carrying unpushed local commits \
         (data-loss risk t-...50899-10): {reason:?}"
    );
    let ref_exists = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "refs/heads/feat/unpushed"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ref_exists, "branch ref must still exist locally after skip");

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&remote).ok();
}

/// Closes the loop on the fix above: a branch that IS genuinely squash-merged
/// (every commit's patch already applied to default) AND old enough to clear
/// `SQUASH_GC_MIN_TIP_AGE`, with its remote gone, must still be deletable —
/// proving the new gate reuses `is_squash_gc_eligible` rather than silently
/// disabling remote-gone cleanup altogether.
#[test]
fn cleanup_merged_branch_still_deletes_aged_squash_merged_remote_gone_branch_worktree_git() {
    let remote = scratch("cmb-squash-gone-bare");
    git(&remote, &["init", "--bare", "-b", "main"]);

    let repo = scratch("cmb-squash-gone-clone");
    std::process::Command::new("git")
        .args([
            "clone",
            &remote.display().to_string(),
            &repo.display().to_string(),
        ])
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git clone");

    git(&repo, &["commit", "--allow-empty", "-m", "init"]);
    git(&repo, &["push", "-u", "origin", "main"]);

    // Branch tip is backdated past SQUASH_GC_MIN_TIP_AGE (24h) so the
    // heuristic squash detection clears its age floor.
    let old_date = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
    git(&repo, &["checkout", "-b", "feat/squashed"]);
    std::fs::write(repo.join("feat.txt"), "feat content\n").expect("write");
    git(&repo, &["add", "feat.txt"]);
    std::process::Command::new("git")
        .args(["commit", "-m", "feat: squashed body"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", &old_date)
        .env("GIT_COMMITTER_DATE", &old_date)
        .output()
        .expect("git commit dated");
    git(&repo, &["push", "-u", "origin", "feat/squashed"]);

    // Squash-apply feat/squashed's diff onto main as a separate commit (mirrors
    // GitHub's "Squash and merge"), then simulate the remote branch deletion
    // that follows a squash-merge.
    git(&repo, &["checkout", "main"]);
    git(&repo, &["cherry-pick", "--no-commit", "feat/squashed"]);
    git(&repo, &["commit", "-m", "squash: feat/squashed body"]);
    git(&repo, &["push", "origin", "main"]);
    git(&remote, &["branch", "-D", "feat/squashed"]);

    let (deleted, reason) = cleanup_merged_branch(&repo, "feat/squashed", false, None);
    assert!(
        deleted,
        "a genuinely squash-merged, aged, remote-gone branch must still be \
         reaped via is_squash_gc_eligible: {reason:?}"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&remote).ok();
}
