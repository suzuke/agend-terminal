//! Shared test-only helpers for the `review_repro_worktree_git` regression
//! guards attached to `worktree_pool.rs` and `worktree_cleanup.rs` (#2550 P3
//! §6) — previously two near-identical copies (`scratch()` + `git()`)
//! differing only in the scratch-dir prefix string baked into each. Behavior
//! is unchanged: each caller still passes its own prefix, so the resulting
//! temp-dir names are byte-identical to before.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Unique scratch dir under the system temp, namespaced by `prefix` (so two
/// callers' temp dirs never collide) and `tag` (per-test identification).
pub(crate) fn scratch(prefix: &str, tag: &str) -> PathBuf {
    static C: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{tag}-{}-{}",
        std::process::id(),
        C.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir scratch");
    dir
}

/// Run `git` with the daemon bypass env (mirrors the in-module test harness).
///
/// Named `review_repro_git`, not the bare `git` the two call sites' own
/// local wrappers used — `tests/anti_pattern_invariant.rs`'s Rule 1
/// (dead-code-helper-pattern) scans `src/` for `pub`/`pub(crate)` fn names
/// that collide with a `tests/`-integration-test's own local helper of the
/// same name (several already define their own `fn git(...)`); a bare `git`
/// here would trip that lint by newly colliding with those pre-existing,
/// unrelated helpers.
pub(crate) fn review_repro_git(dir: &Path, args: &[&str]) {
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
