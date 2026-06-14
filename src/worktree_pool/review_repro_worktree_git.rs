//! Review-repro tests (scope: worktree-git) attached to `worktree_pool.rs`.
//!
//! Each `#[ignore]`d test encodes the CORRECT expected behavior for a
//! confirmed code-review finding: it is RED against the current (buggy)
//! code and GREEN once the fix lands. Remove the `#[ignore]` after the
//! corresponding fix to lock the behavior in.
//!
//! Placement: in-module submodule so the private `cleanup_merged_branch`
//! and `evaluate_candidate` are reachable via `super::`.

#![allow(clippy::expect_used)]

use super::{cleanup_merged_branch, evaluate_candidate, GcKind, MANAGED_MARKER};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Unique scratch dir under the system temp.
fn scratch(tag: &str) -> PathBuf {
    static C: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agend-wtpool-reprowg-{}-{}-{}",
        tag,
        std::process::id(),
        C.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir scratch");
    dir
}

/// Run `git` with the daemon bypass env (mirrors the in-module test harness).
fn git(dir: &Path, args: &[&str]) {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git invocation");
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
#[ignore = "worktree-git #1 default_branch-fallback: red until fix; remove #[ignore] after fix to confirm"]
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
    let (deleted, reason) = cleanup_merged_branch(&repo, "feat/merged", true);
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
// Finding #6 (low): evaluate_candidate's agent-name fallback (when the
// `.agend-managed` marker is corrupt and lacks an `agent=` line) reads
// `wt_path.parent().file_name()`. For a SLASH branch (the common case,
// `feat/<x>`), the worktree path is `<home>/worktrees/<agent>/feat/<x>`, so
// the parent's file_name is `feat`, NOT the real agent. The force-reclaim /
// liveness / binding decision is then evaluated against agent `feat` rather
// than the real agent. CORRECT: derive the agent from the FIRST path
// component under the worktree root.
// ──────────────────────────────────────────────────────────────────────────

#[test]
#[ignore = "worktree-git #6 gc-agent-slash-branch-fallback: red until fix; remove #[ignore] after fix to confirm"]
fn evaluate_candidate_derives_real_agent_for_slash_branch_worktree_git() {
    let home = scratch("gc-agent-slash");
    let real_agent = "agent-real";
    // New-layout slash-branch worktree: <home>/worktrees/<agent>/feat/track-x
    let wt = super::daemon_managed_worktree_root(&home)
        .join(real_agent)
        .join("feat")
        .join("track-x");
    std::fs::create_dir_all(&wt).expect("mkdir worktree");

    // CORRUPT marker: NO `agent=` line (the only case the parent-dir fallback
    // fires). `leased_at` is well past the force-reclaim age cap and there is
    // NO `released_at`, so this routes through the force-reclaim backstop and
    // yields a candidate whose `.agent` we can inspect.
    let old = (chrono::Utc::now() - chrono::Duration::days(60)).to_rfc3339();
    std::fs::write(
        wt.join(MANAGED_MARKER),
        format!("branch=feat/track-x\nleased_at={old}\n"),
    )
    .expect("write marker");

    // No live agents, no daemon run dir (so not in boot grace), no binding for
    // either `agent-real` or `feat` → the force-reclaim arm is reached.
    let live: HashSet<String> = HashSet::new();
    let cand = evaluate_candidate(&home, &wt, &live)
        .expect("an abandoned corrupt-marker slash-branch worktree must be a candidate");
    assert_eq!(
        cand.kind,
        GcKind::ForceReclaim,
        "no released_at + past-cap leased_at → force-reclaim path"
    );
    assert_eq!(
        cand.agent, real_agent,
        "agent must be derived from the FIRST component under the worktrees root \
         (`{real_agent}`), not the immediate parent dir of a slash branch (`feat`). \
         Got: {:?}",
        cand.agent
    );

    std::fs::remove_dir_all(&home).ok();
}
