//! Review-repro tests (scope: worktree-git) attached to `worktree_cleanup.rs`.
//!
//! Each `#[ignore]`d test encodes the CORRECT expected behavior for a
//! confirmed code-review finding: it is RED against the current (buggy)
//! code and GREEN once the fix lands. Remove the `#[ignore]` after the
//! corresponding fix to lock the behavior in.
//!
//! Placement: in-module submodule so the private `prune_orphaned_branches`
//! and `is_in_use` are reachable via `super::`.

#![allow(clippy::expect_used)]

use super::prune_orphaned_branches;
// `is_in_use` is only exercised by the #[cfg(unix)] symlink-based test below
#[cfg(unix)]
use super::is_in_use;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Unique scratch dir under the system temp.
fn scratch(tag: &str) -> PathBuf {
    static C: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agend-wtclean-reprowg-{}-{}-{}",
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

/// True iff `branch` still exists as a local ref in `repo`.
fn branch_exists(repo: &Path, branch: &str) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ──────────────────────────────────────────────────────────────────────────
// Finding #2 (medium): prune_orphaned_branches deletes a branch on the
// remote-gone signal alone (`is_remote_gone`), with no `is_branch_merged`
// AND-guard and no check for committed-but-unpushed local history. A branch
// pushed once (so `branch.<name>.remote` is set), then deleted on the remote
// while the developer kept making LOCAL commits never re-pushed, is reaped by
// `git branch -D` — those unmerged local commits are unrecoverable. CORRECT:
// a remote-gone branch carrying commits NOT reachable from the default branch
// must be KEPT (the remote-gone signal must be gated by an "every commit is in
// default" check).
// ──────────────────────────────────────────────────────────────────────────

#[test]
fn prune_keeps_remote_gone_branch_with_unpushed_commits_worktree_git() {
    // A bare "remote" + a clone, so `branch.<name>.remote` is real.
    let remote = scratch("remote-gone-bare");
    git(&remote, &["init", "--bare", "-b", "main"]);

    let repo = scratch("remote-gone-clone");
    // Clone INTO `repo` (clone takes src + dst).
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

    // Remote head deleted (e.g. PR closed / branch removed), then prune so the
    // local remote-tracking ref disappears → is_remote_gone() == true.
    git(&remote, &["branch", "-D", "feat/unpushed"]);
    git(&repo, &["fetch", "--prune"]);

    // Precondition sanity: the branch is NOT merged into main and carries
    // local-only commits.
    assert!(
        branch_exists(&repo, "feat/unpushed"),
        "precondition: branch must exist before prune"
    );

    let pruned = prune_orphaned_branches(&repo);

    // CORRECT: a remote-gone branch with committed-but-unpushed local work that
    // is NOT in the default branch must be KEPT (its commits are otherwise
    // unrecoverable after `git branch -D`).
    assert!(
        !pruned.iter().any(|b| b == "feat/unpushed"),
        "remote-gone branch with unpushed local commits must NOT be force-deleted: {pruned:?}"
    );
    assert!(
        branch_exists(&repo, "feat/unpushed"),
        "the branch ref (and its unpushed commits) must survive the prune"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&remote).ok();
}

// ──────────────────────────────────────────────────────────────────────────
// Finding #7 (low): is_in_use canonicalizes both the candidate worktree path
// and each active agent dir, falling back to the un-canonicalized path on
// error (fail-open). When the active agent dir and the worktree path differ
// only by symlink/normalization AND one side transiently fails to canonicalize
// (e.g. the active dir does not currently exist on disk), both the
// canonicalized AND the raw `starts_with` comparisons miss, so is_in_use
// returns false for a worktree an agent is actively using — and the sweep can
// remove it out from under the agent. CORRECT (fail-closed): a canonicalize
// failure must be treated as "cannot prove not-in-use" → in-use.
// ──────────────────────────────────────────────────────────────────────────

#[test]
#[cfg(unix)] // symlink-based repro
#[ignore = "worktree-git #7 is_in_use-canonicalize-fail-open: red until fix; remove #[ignore] after fix to confirm"]
fn is_in_use_fails_closed_on_canonicalize_error_worktree_git() {
    use std::os::unix::fs::symlink;

    let base = scratch("in-use-symlink");
    // `real` is the actual worktree directory; `link` is a symlink alias to it.
    let real = base.join("real");
    std::fs::create_dir_all(&real).expect("mkdir real");
    let link = base.join("link");
    symlink(&real, &link).expect("symlink link -> real");

    // The registered worktree path is the canonical `real`. The active agent's
    // working_dir is recorded THROUGH the symlink alias at `link/agent`, but
    // that subdir does NOT exist on disk yet (transient) → its canonicalize
    // fails and falls back to the raw `link/agent`, whose textual prefix does
    // not start with the canonical `real`.
    let wt_path = real.clone();
    let active = vec![link.join("agent")];

    // The agent IS using a path inside the worktree (`link/agent` resolves
    // under `real`), so is_in_use MUST report in-use. Pre-fix the fail-open
    // canonicalize lets both comparisons miss → false → the worktree would be
    // swept while in use.
    assert!(
        is_in_use(&wt_path, &active),
        "a worktree whose active-agent dir canonicalizes-failed but aliases into \
         it (via symlink) must be treated as IN USE (fail-closed), not swept"
    );

    std::fs::remove_dir_all(&base).ok();
}
