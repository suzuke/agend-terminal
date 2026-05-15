//! #821 — isolated git subprocess invocation for test fixtures.
//!
//! Test fixtures that shell out to `git` MUST use this module instead
//! of `std::process::Command::new("git")` directly. The helpers pin:
//!
//! - `AGEND_GIT_BYPASS=1` — bypasses the `agend-git` shim's binding-
//!   based routing. Without this, the shim sees the test process's
//!   `AGEND_INSTANCE_NAME` (if set by `cargo test` env inheritance)
//!   and routes git operations to the BOUND worktree's branch instead
//!   of the test's temp dir. The #820 PR's mid-implementation
//!   "feat-b polluted host worktree" incident traced back to a
//!   manual bash debug session running without this env (NOT the
//!   test fixture itself, but the trap class is real).
//!
//! - `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL` / `GIT_COMMITTER_*` —
//!   subprocess `git commit` fails with `unable to auto-detect
//!   email address` on CI runners that lack a global `~/.gitconfig`.
//!   Per the #814 r1 lesson — local dev machines inherit the
//!   developer's gitconfig and mask this trap.
//!
//! - `current_dir(repo_dir)` — git uses cwd for upward `.git`
//!   discovery. Without the dir pin, git walks up from the test
//!   process's cwd (the agend-terminal worktree) and finds the
//!   host worktree's `.git`, leaking operations into the host.
//!
//! ## Pattern to AVOID (bad)
//!
//! ```rust,ignore
//! std::process::Command::new("git")
//!     .args(["checkout", "-b", "feat-b"])
//!     .output()  // ← no cwd pin, no shim bypass, no committer env
//! ```
//!
//! ## Pattern to USE (good)
//!
//! ```rust,ignore
//! use crate::common::git_isolated;
//! let repo = git_isolated::setup_temp_repo("my-tag");
//! git_isolated::git(&repo, &["checkout", "-b", "feat-b"]);
//! ```
//!
//! ## Allowlist
//!
//! Pre-existing test files using raw `Command::new("git")` are
//! grandfathered via the `tests/test_isolation_invariant.rs` allowlist.
//! New tests added after #821 ships MUST use this helper.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run git in `repo_dir` with cwd isolation + shim bypass + pinned
/// author/committer. The canonical entry for test fixtures.
///
/// #821 stub — real implementation lands in C2.
#[allow(dead_code)]
pub fn git(repo_dir: &Path, args: &[&str]) -> Output {
    let _ = (repo_dir, args);
    Command::new("true").output().expect("stub")
}

/// Variant accepting an explicit committer date for back-dating
/// commits (e.g. stale-detection tests per #817 fixture).
///
/// #821 stub — real implementation lands in C2.
#[allow(dead_code)]
pub fn git_dated(repo_dir: &Path, args: &[&str], date_rfc3339: &str) -> Output {
    let _ = (repo_dir, args, date_rfc3339);
    Command::new("true").output().expect("stub")
}

/// Create a temp git repo with `main` branch + initial commit +
/// pinned per-repo gitconfig. Returns the repo PathBuf. Standard
/// entry point for fixtures that need a fresh git repo.
///
/// #821 stub — real implementation lands in C2.
#[allow(dead_code)]
pub fn setup_temp_repo(tag: &str) -> PathBuf {
    let _ = tag;
    std::env::temp_dir()
}
