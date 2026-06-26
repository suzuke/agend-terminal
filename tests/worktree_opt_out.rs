//! Worktree auto-prune pre-flight integration tests (git-level).
//!
//! fleet.yaml `worktree: false` parsing is covered in-crate by
//! `src/fleet/mod.rs::worktree_opt_out_parsed`, which drives the real
//! `FleetConfig`/`InstanceConfig` structs. The former test-local mirror
//! structs (`TestInstanceConfig`/`TestFleetConfig`) + their parse tests
//! here re-declared the schema instead of exercising production, so they
//! were redundant test-of-test and were removed.

#![allow(clippy::unwrap_used)]

mod common;

// --- Auto-prune pre-flight check tests (worktree.rs functions) ---
// These test the git-level functions directly since they're in the binary.
// The actual has_uncommitted_changes / remove_worktree are tested via
// the fleet.rs unit tests in the binary crate.

/// Verify git status --porcelain detects uncommitted files.
#[test]
fn git_status_porcelain_detects_dirty() {
    let dir = std::env::temp_dir().join(format!("agend-wt-dirty-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    common::git_isolated::git(&dir, &["init", "-b", "main"]);
    common::git_isolated::git(&dir, &["commit", "--allow-empty", "-m", "init"]);
    std::fs::write(dir.join("dirty.txt"), "uncommitted").unwrap();

    let output = common::git_isolated::git(&dir, &["status", "--porcelain"]);
    assert!(
        !output.stdout.is_empty(),
        "git status --porcelain must detect uncommitted file"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Verify clean repo has empty git status --porcelain.
#[test]
fn git_status_porcelain_clean() {
    let dir = std::env::temp_dir().join(format!("agend-wt-clean-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    common::git_isolated::git(&dir, &["init", "-b", "main"]);
    common::git_isolated::git(&dir, &["commit", "--allow-empty", "-m", "init"]);

    let output = common::git_isolated::git(&dir, &["status", "--porcelain"]);
    assert!(
        output.stdout.is_empty(),
        "clean repo must have empty git status --porcelain"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// --- E2E auto-prune: covered in-crate, not reachable here ---
//
// The flip `true→false` auto-prune behavior lives in the BIN crate
// (`src/bootstrap/agent_resolve.rs::resolve_one`, which calls
// `worktree::has_uncommitted_changes` + `worktree::remove_worktree`) and is
// NOT reachable from this integration-test crate. The former
// `e2e_clean_worktree_flip_prunes` / `e2e_dirty_worktree_flip_rejected` tests
// here never flipped config or invoked any prune path — each only asserted
// that a dir it had just created still existed (`wt_dir.exists()`), which is
// trivially true and could never catch a prune regression (the names promised
// behavior the bodies did not exercise). The genuine behavior is covered by
// in-crate unit tests:
//   - agent_resolve::tests::resolve_one_worktree_false_prunes_clean_existing_worktree
//   - agent_resolve::tests::resolve_one_worktree_false_skips_worktree_creation
//   - the dirty-reject path via the `has_uncommitted_changes` guard there.
// The fake E2E tests were removed rather than left as false confidence.
