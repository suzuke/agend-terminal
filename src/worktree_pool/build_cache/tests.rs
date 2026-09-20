//! #3694 regression tests for the deadline-bounded ignored-cache sweep.

use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

/// Unique temp prefix so this helper cannot collide with another wipe-on-entry
/// fixture (#3245 ratchet). Callers must pass distinct tags.
fn bc_fixture(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-3694-build-cache-{}-{tag}-{id}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create fixture");
    dir
}

fn seed_target_cache(worktree: &Path) {
    let target = worktree.join("target");
    std::fs::create_dir_all(target.join("debug").join("deps")).expect("mkdir nested target");
    for i in 0..64 {
        std::fs::write(target.join("debug").join(format!("obj-{i}.o")), b"x").expect("seed object");
    }
    std::fs::write(target.join("debug").join("deps").join("libfoo.rlib"), b"y").expect("seed rlib");
}

/// A git repository that ignores `target/`, so `check-ignore` classifies the
/// seeded cache as disposable. No commit is required for `check-ignore`.
fn git_repo_ignoring_target(worktree: &Path) {
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .status()
        .expect("git init")
        .success();
    assert!(ok, "git init failed");
    std::fs::write(worktree.join(".gitignore"), "target/\n").expect("write .gitignore");
}

#[test]
fn bounded_removal_removes_everything_within_budget() {
    let wt = bc_fixture("complete");
    seed_target_cache(&wt);
    let target = wt.join("target");
    remove_dir_all_bounded(&target, Instant::now() + Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("ample budget must complete"));
    assert!(!target.exists(), "full sweep must remove the cache");
    std::fs::remove_dir_all(&wt).ok();
}

#[test]
fn bounded_removal_stops_at_deadline_and_leaves_the_tree() {
    let wt = bc_fixture("deadline");
    seed_target_cache(&wt);
    let target = wt.join("target");
    // A zero budget aborts at the first deadline check.
    let out = remove_dir_all_bounded(&target, Instant::now());
    assert!(
        matches!(out, Err(BoundedRemoval::Deadline)),
        "zero budget must report Deadline, got {out:?}"
    );
    assert!(
        target.exists(),
        "a deadline abort must leave the (disposable) tree in place"
    );
    std::fs::remove_dir_all(&wt).ok();
}

#[test]
fn clean_ignored_cache_budget_exhaustion_is_non_fatal() {
    let wt = bc_fixture("skip");
    git_repo_ignoring_target(&wt);
    seed_target_cache(&wt);
    let target = wt.join("target");
    // Zero budget forces the skip path: the sweep must NOT fail the release.
    let out = clean_ignored_build_cache_with_budget(&wt, Duration::ZERO);
    assert!(out.is_ok(), "budget exhaustion is non-fatal: {out:?}");
    assert!(
        target.exists(),
        "a skipped sweep leaves the cache for the bounded git removal"
    );
    std::fs::remove_dir_all(&wt).ok();
}

#[test]
fn clean_ignored_cache_removes_a_small_cache_within_budget() {
    let wt = bc_fixture("remove");
    git_repo_ignoring_target(&wt);
    seed_target_cache(&wt);
    let target = wt.join("target");
    let out = clean_ignored_build_cache_with_budget(&wt, Duration::from_secs(30));
    assert!(out.is_ok(), "ample budget must succeed: {out:?}");
    assert!(!target.exists(), "ample budget must remove the cache");
    std::fs::remove_dir_all(&wt).ok();
}
