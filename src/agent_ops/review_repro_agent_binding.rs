//! review-repro (scope: agent-binding) — verification/reproduction test for the
//! working-dir cleanup symlink-traversal finding.
//!
//! GREEN since the cleanup planner canonicalizes `working_dir` and re-checks it
//! resolves to the victim's exact canonical default before any whole-tree
//! remove; runs un-ignored as a live regression guard. #2764 E: ported from the
//! deleted `cleanup_working_dir` wrapper onto the real destructive entry
//! (`workspace_cleanup::full_delete_destructive_phase`).

/// Finding: the legacy cleanup decided to `remove_dir_all(working_dir)` based
/// on a purely LEXICAL `working_dir.starts_with(workspace)` check (no
/// canonicalization). If the stored `working_directory` is lexically under
/// `$AGEND_HOME/workspace/` but traverses a symlink whose real target is
/// elsewhere, the recursive delete follows the symlink and destroys the
/// symlink's REAL contents — outside the workspace.
///
/// Correct behavior: a path that does not CANONICALLY resolve to the victim's
/// exact default must not trigger the whole-directory `remove_dir_all`. The
/// out-of-workspace victim contents must survive.
#[cfg(unix)]
#[test]
fn cleanup_does_not_follow_symlink_out_of_workspace_agent_binding() {
    use super::workspace_cleanup::{full_delete_destructive_phase, CleanupOutcome};
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

    // Precondition: the lexical prefix check (the historical buggy gate)
    // passes, so a lexical-only implementation WOULD take the whole-dir
    // `remove_dir_all` branch.
    assert!(
        working_dir.starts_with(&workspace),
        "test invariant: working_dir must lexically start with the workspace root"
    );

    let id = crate::types::InstanceId::new();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!(
            "instances:\n  victim-agent:\n    backend: claude\n    id: {}\n    working_directory: {}\n",
            id.full(),
            working_dir.display()
        ),
    )
    .expect("seed fleet.yaml");
    let fleet =
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).expect("load fleet");

    let out = full_delete_destructive_phase(&home, "victim-agent", Some(&fleet));

    let survived = victim.join("data/precious.txt").exists();
    std::fs::remove_dir_all(&base).ok();
    assert!(
        !matches!(out, CleanupOutcome::Clean) || survived,
        "a working_dir canonically resolving elsewhere must never be reported \
         Clean while the symlink target was destroyed"
    );
    assert!(
        survived,
        "cleanup followed a symlink OUT of the workspace and deleted real user \
         data — the planner must canonicalize working_dir and refuse the \
         whole-dir delete when it does not resolve to the victim's exact default"
    );
}
