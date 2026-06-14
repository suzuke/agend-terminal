//! xcut-concurrency F4 (security, defense-in-depth): `worktree_path` joins the
//! agent/instance name into a filesystem path without validating it, and
//! `worktree::create` only validates the BRANCH segment (`validate_branch`),
//! never the AGENT segment. A caller passing an unvalidated instance name plus
//! a VALID custom branch lets the agent segment traverse OUT of the worktrees
//! pool (e.g. `../escape`), because the only path-traversal guard at this layer
//! is on the branch.
//!
//! Correct behavior: `create` must call `agent::validate_name(instance_name)`
//! (which rejects `.`/`/` and therefore `..`) at the top — mirroring the
//! `validate_branch` guard already a few lines later — and return `None` for a
//! traversal name BEFORE any `git worktree add` runs. This test drives the
//! CURRENT entry point with a traversal agent name and a clean custom branch,
//! and asserts `create` returns `None` and creates nothing outside the pool.
//!
//! Red now: the branch is valid so `validate_branch` passes, the agent segment
//! is never validated, `git worktree add` (AGEND_GIT_BYPASS=1) materializes the
//! worktree OUTSIDE `<home>/worktrees/`, and `create` returns `Some` → the
//! `is_none()` assertion fails (and an escape dir is created). Green after the
//! validate_name guard lands.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::create;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A fresh git repo with one commit. Mirrors the parent module's `tmp_repo`
/// fixture (AGEND_GIT_BYPASS=1 so the agend-git shim does not deny the ops).
fn tmp_repo(name: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-wt-xcut-repo-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["init", "-b", "main"])
        .current_dir(&dir)
        .output()
        .ok();
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@test",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(&dir)
        .output()
        .ok();
    dir
}

/// A test home dir distinct from the repo dir, so the external worktree layout
/// `<home>/worktrees/<agent>/<branch>/` is verifiable in isolation.
fn tmp_home(name: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-wt-xcut-home-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

#[test]
#[ignore = "xcut-concurrency F4: red until fix; remove #[ignore] after fix to confirm"]
fn create_rejects_path_traversal_in_agent_name_xcut_concurrency() {
    let home = tmp_home("traversal");
    let repo = tmp_repo("traversal");

    // Malicious agent/instance segment with a path-traversal component, paired
    // with a VALID custom branch (so validate_branch passes and the ONLY thing
    // standing between the caller and a pool escape would be agent-name
    // validation — which create() does not currently perform).
    let malicious_agent = "../escape-xcut-concurrency";
    let valid_branch = "valid-branch-xcut";

    let result = create(&home, &repo, malicious_agent, Some(valid_branch));

    // Where an unvalidated `..` agent segment would escape to.
    let pool = home.join("worktrees");
    let escape_dir = home.join("escape-xcut-concurrency");

    // Capture facts before cleanup so the panic message is informative.
    let escaped_path = result.as_ref().map(|i| i.path.clone());
    let escape_dir_created = escape_dir.exists();
    let escaped_outside_pool = escaped_path
        .as_ref()
        .map(|p| !path_is_within(p, &pool))
        .unwrap_or(false);

    // Cleanup regardless of outcome (the worktree may have materialized outside
    // the pool in the buggy state).
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&escape_dir).ok();
    std::fs::remove_dir_all(&repo).ok();

    assert!(
        result.is_none(),
        "xcut-concurrency F4: worktree::create accepted a path-traversal instance name \
         ('{malicious_agent}') with a valid custom branch and returned {escaped_path:?} \
         (escaped_outside_pool={escaped_outside_pool}, escape_dir_created={escape_dir_created}). \
         create() must validate the agent/instance segment (agent::validate_name) at the top — \
         mirroring the validate_branch guard — and return None for a traversal name, so the \
         path-construction layer is self-defending and a worktree can never be materialized \
         outside <home>/worktrees/."
    );
}

/// True if `child` is `base` or lives under it (lexical, no symlink resolution —
/// the fixture paths contain no symlinks).
fn path_is_within(child: &Path, base: &Path) -> bool {
    let cc: Vec<_> = child.components().collect();
    let bc: Vec<_> = base.components().collect();
    cc.len() >= bc.len() && cc[..bc.len()] == bc[..]
}
