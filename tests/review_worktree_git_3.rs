#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Review-repro static invariant (scope: worktree-git), finding #3 (low).
//!
//! `worktree_pool::release()` does a NON-ATOMIC, UNLOCKED read-then-rewrite of
//! the `.agend-managed` marker: `read_to_string(&marker)` → append a
//! `released_at=` line → `atomic_write(&marker, …)`. The read/write are not
//! under the per-agent binding lock that `create()`/`bind_full`/`gc` use, so a
//! concurrent rewrite or a crash between read and write can drop `released_at`
//! from the marker — which `evaluate_candidate` then reclassifies from the
//! clean-release grace path into the force-reclaim backstop, changing the
//! deletion semantics. The `atomic_write` failure is additionally swallowed
//! (`let _ =`).
//!
//! This source-scanning invariant is RED while the unguarded marker RMW remains
//! inside `release()`. It goes GREEN when the fix either:
//!  - guards the marker read-modify-write with the per-agent binding lock
//!    (`acquire_file_lock` appears inside `release`), OR
//!  - stores `released_at` in the binding record instead of appending to the
//!    marker (`atomic_write(&marker` no longer appears inside `release`).

use std::path::PathBuf;

/// Extract the body text of the first top-level fn whose signature line
/// contains `sig_needle`, via brace balancing. Returns the full text from the
/// signature line through the closing brace (inclusive).
fn fn_body(src: &str, sig_needle: &str) -> String {
    let bytes_lines: Vec<&str> = src.lines().collect();
    let start = bytes_lines
        .iter()
        .position(|l| l.contains(sig_needle))
        .unwrap_or_else(|| panic!("signature `{sig_needle}` not found in source"));
    let mut depth: i32 = 0;
    let mut seen_open = false;
    let mut out = String::new();
    for line in &bytes_lines[start..] {
        out.push_str(line);
        out.push('\n');
        for ch in line.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    seen_open = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if seen_open && depth <= 0 {
            break;
        }
    }
    out
}

#[test]
#[ignore = "worktree-git #3 release-marker-rmw: red until fix; remove #[ignore] after fix to confirm"]
fn release_marker_rmw_is_lock_guarded_or_record_based_worktree_git_3() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/worktree_pool.rs");
    let src = std::fs::read_to_string(&path).expect("read src/worktree_pool.rs");

    let body = fn_body(&src, "pub fn release(");

    // The buggy shape: rewriting the marker file in-place inside release().
    let rewrites_marker = body.contains("atomic_write(&marker");
    // The guard the fix must add when it keeps rewriting the marker.
    let lock_guarded = body.contains("acquire_file_lock");

    assert!(
        !rewrites_marker || lock_guarded,
        "worktree-git #3: `release()` rewrites the `.agend-managed` marker \
         (`atomic_write(&marker`) WITHOUT acquiring the per-agent binding lock — a \
         non-atomic, unlocked read-modify-write that can drop `released_at` and \
         reclassify the worktree for GC. Hold the binding lock across the marker \
         RMW (so `acquire_file_lock` appears in `release`) OR store `released_at` \
         in the binding record (so `atomic_write(&marker` no longer appears).\n\n\
         --- release() body ---\n{body}"
    );
}
