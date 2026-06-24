//! review-repro (scope: agent-binding) — verification/reproduction test for the
//! `cleanup_working_dir` symlink-traversal finding.
//!
//! GREEN now that `cleanup_working_dir` canonicalizes `working_dir` and
//! re-checks it is under the canonicalized workspace root before
//! `remove_dir_all`; runs un-ignored as a live regression guard.

// only used by the #[cfg(unix)] symlink-traversal test below
#[cfg(unix)]
use super::cleanup_working_dir;

/// Finding: `cleanup_working_dir` decides to `remove_dir_all(working_dir)`
/// based on a purely LEXICAL `working_dir.starts_with(workspace)` check (no
/// canonicalization). If the stored `working_directory` is lexically under
/// `$AGEND_HOME/workspace/` but traverses a symlink whose real target is
/// elsewhere, the recursive delete follows the symlink and destroys the
/// symlink's REAL contents — outside the workspace.
///
/// Correct behavior: a path that does not CANONICALLY resolve inside the
/// workspace must not trigger the whole-directory `remove_dir_all`. The
/// out-of-workspace victim contents must survive.
///
/// RED now: the victim file is deleted (symlink followed). GREEN after fix
/// (dunce::canonicalize + re-check): the victim file survives.
#[cfg(unix)]
#[test]
fn cleanup_working_dir_does_not_follow_symlink_out_of_workspace_agent_binding() {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let uniq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!(
        "agend-review-cleanup-symlink-{}-{}",
        std::process::id(),
        uniq
    ));
    let home = base.join("home");
    let victim = base.join("victim");

    // A precious file OUTSIDE the workspace (the symlink's real target).
    std::fs::create_dir_all(victim.join("data")).expect("create victim/data");
    std::fs::write(victim.join("data/precious.txt"), "precious-user-data")
        .expect("write precious file");

    // workspace/<link> is a symlink → victim. `working_dir` traverses it.
    let workspace = crate::paths::workspace_dir(&home);
    std::fs::create_dir_all(&workspace).expect("create workspace");
    std::os::unix::fs::symlink(&victim, workspace.join("link")).expect("symlink");
    let working_dir = workspace.join("link/data");

    // Precondition: the lexical prefix check (the buggy gate) passes, so the
    // current code WILL take the whole-dir `remove_dir_all` branch.
    assert!(
        working_dir.starts_with(&workspace),
        "test invariant: working_dir must lexically start with the workspace root"
    );
    assert!(
        victim.join("data/precious.txt").exists(),
        "test invariant: precious file must exist before cleanup"
    );

    cleanup_working_dir(&home, "victim-agent", &working_dir);

    let survived = victim.join("data/precious.txt").exists();
    std::fs::remove_dir_all(&base).ok();
    assert!(
        survived,
        "cleanup_working_dir followed a symlink OUT of the workspace and deleted \
         real user data — it must canonicalize working_dir and refuse the \
         whole-dir delete when it does not resolve inside the workspace"
    );
}
