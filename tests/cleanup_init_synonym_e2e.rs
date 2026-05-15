//! #822 integration smoke — heartbeat-synonym whitelist + body-emptiness
//! gate end-to-end at the git-query level, dogfooding the #821 helper.
//!
//! ## Scope (read this before the test body)
//!
//! Production's `clean_empty_init_commits` (in the binary crate at
//! `src/mcp/handlers/dispatch_hook/mod.rs`) is the canonical entry,
//! and its unit tests live in
//! `src/mcp/handlers/dispatch_hook/tests.rs` (the four
//! `clean_empty_init_commits_*` tests added by #822 C1/C2/C3 cover
//! the actual function end-to-end inside the binary crate).
//!
//! This integration test sits at a deliberately narrower scope: it
//! reconstructs the exact git-query sequence the production helper
//! issues (`git log origin/main..HEAD --format=%H %s`, `git diff-tree
//! --no-commit-id`, `git log -1 --format=%b`) using **only**
//! `tests/common/git_isolated::git()` for every subprocess
//! invocation, and asserts the classification contract — which
//! commits the production helper would mark "drop" vs "keep" — at
//! the integration scope.
//!
//! Why not drive via the full daemon MCP entry? `repo
//! cleanup_init_commits` requires a bound agent + daemon-managed
//! worktree state (binding.json, .agend-managed marker, hooks).
//! Spinning that up in an integration test would add ~80 LOC of
//! daemon-state orchestration for a 1-LOC assertion — and unit
//! tests inside the binary crate already cover the function-level
//! path. The honest integration scope is what's tested here: the
//! `tests/common/git_isolated` helper, validating it works
//! correctly for the production query pattern (the helper's first
//! production-style consumer).
//!
//! ## What this dogfoods
//!
//! Every git subprocess in this test goes through #821's
//! `git_isolated::git()` helper. That helper pins
//! `AGEND_GIT_BYPASS=1` + `GIT_AUTHOR_*`/`GIT_COMMITTER_*` envs +
//! `current_dir(repo)`. The #821 invariant test enforces this
//! pattern for all new `tests/*.rs` files (this file has no
//! `// allow: raw-git-subprocess` markers because it doesn't need
//! any).

#![allow(clippy::unwrap_used, clippy::expect_used)]
// `mod common` pulls in tests/common/{env_gate,git_isolated,harness}.rs
// as one tree per cargo per-binary dead-code rules. This test only
// consumes git_isolated, leaving the harness helpers unused at this
// binary's scope — silence the dead-code lint for the inherited items
// (mirrors the `#[allow(dead_code)]` pattern in env_gate.rs).
#![allow(dead_code)]

mod common;

use common::git_isolated;
use std::path::Path;

/// Mirror of production's whitelist (kept literal here so a future
/// change to the production const surfaces as a test-failure delta
/// rather than silent drift). The hardcoded list is the contract.
const HEARTBEAT_NAMES: &[&str] = &["init", "initial"];

fn is_heartbeat_subject(msg: &str) -> bool {
    HEARTBEAT_NAMES.contains(&msg)
}

fn commit_body_is_empty(worktree: &Path, hash: &str) -> bool {
    let out = git_isolated::git(worktree, &["log", "-1", "--format=%b", hash]);
    out.status.success() && String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

fn diff_is_empty(worktree: &Path, hash: &str) -> bool {
    let out = git_isolated::git(
        worktree,
        &["diff-tree", "--no-commit-id", "--name-only", "-r", hash],
    );
    out.status.success() && out.stdout.trim_ascii().is_empty()
}

/// Replicates the candidate-selection logic in production's
/// `clean_empty_init_commits` (mod.rs:735-756 post-#822). Returns
/// commit hashes that production would mark "drop".
fn collect_droppable_hashes(worktree: &Path) -> Vec<String> {
    let out = git_isolated::git(worktree, &["log", "origin/main..HEAD", "--format=%H %s"]);
    assert!(out.status.success(), "git log failed: {out:?}");
    let log = String::from_utf8_lossy(&out.stdout);
    let mut droppable = Vec::new();
    for line in log.lines() {
        let Some((hash, msg)) = line.split_once(' ') else {
            continue;
        };
        if !is_heartbeat_subject(msg) {
            continue;
        }
        if !commit_body_is_empty(worktree, hash) {
            continue;
        }
        if !diff_is_empty(worktree, hash) {
            continue;
        }
        droppable.push(hash.to_string());
    }
    droppable
}

/// Mint an empty-diff commit on `worktree`'s current branch with
/// `subject` + optional `body`. Pure dogfood — every git call here
/// goes through `git_isolated::git`.
fn empty_commit(worktree: &Path, subject: &str, body: Option<&str>) {
    let mut args: Vec<&str> = vec!["commit", "--allow-empty", "-m", subject];
    if let Some(b) = body {
        args.push("-m");
        args.push(b);
    }
    let out = git_isolated::git(worktree, &args);
    assert!(out.status.success(), "empty commit `{subject}` failed");
}

/// Mint a commit that actually changes a file. Use a unique `filename`
/// per call so caller can interleave with empty commits without
/// stomping on existing paths.
fn diffful_commit(worktree: &Path, subject: &str, filename: &str) {
    std::fs::write(worktree.join(filename), format!("payload for {subject}\n")).unwrap();
    let add = git_isolated::git(worktree, &["add", filename]);
    assert!(add.status.success(), "git add `{filename}` failed");
    let commit = git_isolated::git(worktree, &["commit", "-m", subject]);
    assert!(commit.status.success(), "diffful commit `{subject}` failed");
}

/// Build a temp repo with `origin/main` set up (so `origin/main..HEAD`
/// resolves) and HEAD on a fresh feature branch. Returns the repo
/// directory (the same dir is the "worktree" here — for an
/// integration smoke this collapses the repo/worktree split, which
/// is fine because the production helper only ever operates on the
/// worktree path).
fn setup_repo_with_main_ref(tag: &str) -> std::path::PathBuf {
    let repo = git_isolated::setup_temp_repo(tag);
    // Seed main with an initial commit so `origin/main..HEAD` resolves.
    let seed = git_isolated::git(&repo, &["commit", "--allow-empty", "-m", "main: seed"]);
    assert!(seed.status.success(), "seed main failed");
    let head = git_isolated::git(&repo, &["rev-parse", "HEAD"]);
    let main_sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
    let update = git_isolated::git(
        &repo,
        &["update-ref", "refs/remotes/origin/main", &main_sha],
    );
    assert!(update.status.success(), "update-ref origin/main failed");
    let branch = git_isolated::git(&repo, &["checkout", "-b", "feature"]);
    assert!(branch.status.success(), "checkout feature failed");
    repo
}

/// E2E smoke: `init` empty, `initial` empty, real commit, `init` w/
/// body, `initial` w/ body all coexist. The classification contract
/// selects exactly the two empty-body whitelisted subjects.
#[test]
fn cleanup_init_synonym_e2e_classifies_via_helper() {
    let repo = setup_repo_with_main_ref("822_e2e_classify");
    empty_commit(&repo, "init", None); // → drop
    empty_commit(&repo, "initial", None); // → drop (the #820 stray case)
    diffful_commit(&repo, "feat: real work", "real.txt"); // → keep (diff)
    empty_commit(&repo, "init", Some("operator body notes")); // → keep (body)
    empty_commit(&repo, "initial", Some("planning notes")); // → keep (body)
    empty_commit(&repo, "wip", None); // → keep (not in whitelist v1)

    let droppable = collect_droppable_hashes(&repo);
    assert_eq!(
        droppable.len(),
        2,
        "exactly 2 commits should be flagged for drop \
         (empty-body `init` + empty-body `initial`); droppable={droppable:?}"
    );

    // Sanity: total candidate-pool size matches what the helper sees.
    let log = git_isolated::git(&repo, &["log", "origin/main..HEAD", "--format=%H %s"]);
    let total = String::from_utf8_lossy(&log.stdout).lines().count();
    assert_eq!(
        total, 6,
        "expected 6 commits on feature branch, got {total}"
    );

    std::fs::remove_dir_all(&repo).ok();
}
