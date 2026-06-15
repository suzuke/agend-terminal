#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Review-repro static invariant (scope: worktree-git), finding #8 (info).
//!
//! `worktree_pool::unsubscribe_all_ci_watches_for_agent` is annotated
//! `#[allow(dead_code)]` and documented as the #931 rollback target, "slated for
//! removal one Sprint after #931 lands assuming no rollback fires." #931 has
//! shipped (release_full no longer touches ci-watches; the persist-across-release
//! tests are green), so this ~55-line function is reachable-but-unreferenced dead
//! code carrying a non-trivial DESTRUCTIVE path (it can delete ci-watch files)
//! that no test exercises against production behavior.
//!
//! This invariant is RED while the function definition remains in
//! `src/worktree_pool.rs`. It goes GREEN once the dead destructive path is
//! deleted (git history is the documented rollback target). Prose/comment
//! mentions of the name do NOT keep it red — only the `fn …` definition does.

use std::path::PathBuf;

#[test]
fn unsubscribe_all_ci_watches_dead_code_removed_worktree_git_8() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/worktree_pool.rs");
    let src = std::fs::read_to_string(&path).expect("read src/worktree_pool.rs");

    // Match the FUNCTION DEFINITION only — `fn unsubscribe_all_ci_watches_for_agent(`
    // on a non-comment line. Comments (prose mentions of the name) start with `//`
    // or `*` and are skipped.
    const DEF: &str = "fn unsubscribe_all_ci_watches_for_agent(";
    let offenders: Vec<(usize, String)> = src
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with('*') {
                return false;
            }
            line.contains(DEF)
        })
        .map(|(i, line)| (i + 1, line.trim().to_string()))
        .collect();

    assert!(
        offenders.is_empty(),
        "worktree-git #8: dead destructive fn `unsubscribe_all_ci_watches_for_agent` \
         is still defined in src/worktree_pool.rs (#931 has shipped — its \
         persist-across-release tests are green). Delete the unreferenced \
         `#[allow(dead_code)]` ci-watch-deleting path; git history is the rollback \
         target. Found definition at:\n{}",
        offenders
            .iter()
            .map(|(ln, txt)| format!("  src/worktree_pool.rs:{ln}: {txt}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
