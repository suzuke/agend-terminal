#![allow(clippy::unwrap_used, clippy::expect_used)]

//! S2 source invariant: the obsolete soft release entry point is gone.
//! Destructive release uses `release_full` or an exact guarded target
//! transaction, so marker mutation cannot occur through an unguarded helper.

use std::path::PathBuf;

#[test]
fn release_marker_rmw_is_lock_guarded_or_record_based_worktree_git_3() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/worktree_pool.rs");
    let src = std::fs::read_to_string(&path).expect("read src/worktree_pool.rs");
    assert!(
        !src.contains("pub fn release("),
        "S2 must remove the obsolete soft release entry point"
    );
    assert!(
        src.contains("release_full"),
        "production release paths must use guarded transaction entry points"
    );
}
