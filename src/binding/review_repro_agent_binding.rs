//! review-repro (scope: agent-binding) — verification/reproduction test for the
//! `install_hooks` silent-failure finding.
//!
//! RED against current (unfixed) code; GREEN once `install_hooks` surfaces a
//! warning (or returns a Result) when wiring the prepare-commit-msg hook fails.
//! `#[ignore]`d so CI stays green until the fix lands.

use super::install_hooks;

/// Finding: `install_hooks` discards every fallible step — `create_dir_all().ok()`,
/// `let _ = std::fs::write(...)`, and `let _ = git_helpers::git_ok(... "config"
/// "core.hooksPath" ...)`. `git_ok` returns a bool indicating success but it is
/// dropped. If the worktree is not yet a git repo (or the config write fails),
/// the prepare-commit-msg hook is silently NEVER wired up, with NO warning for
/// an operator to notice — the commit-msg/binding enforcement the hook exists
/// to provide is silently absent for that worktree.
///
/// This test drives `install_hooks` against a NON-git directory, so the
/// `git config core.hooksPath ...` step fails (`git_ok` returns false).
///
/// Correct behavior: the failure must be observable — a `warn`-level log
/// mentioning the hook installation failure must fire.
///
/// RED now: no log fires (`install_hooks` swallows the failure). GREEN after
/// fix: a warn about the hook failure is emitted.
#[test]
#[tracing_test::traced_test]
fn install_hooks_warns_when_git_config_fails_agent_binding() {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let uniq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!(
        "agend-review-install-hooks-{}-{}",
        std::process::id(),
        uniq
    ));
    let home = base.join("home");
    // A worktree directory that is NOT a git repo → `git config core.hooksPath`
    // exits non-zero → `git_ok` returns false (the swallowed failure).
    let worktree = base.join("not-a-git-worktree");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&worktree).expect("create worktree");

    install_hooks(&home, &worktree);

    // The fix must surface the failure. A warn about the hook (any of these
    // substrings) is the operator-visible signal the finding asks for.
    let observed = logs_contain("hook")
        || logs_contain("hooksPath")
        || logs_contain("install_hooks")
        || logs_contain("core.hooksPath");

    std::fs::remove_dir_all(&base).ok();
    assert!(
        observed,
        "install_hooks silently swallowed the failed `git config core.hooksPath` \
         on a non-git worktree — a failed hook install must be observable \
         (warn log) so an operator can notice the hook is not wired up"
    );
}
