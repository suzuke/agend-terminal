#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Review-repro static invariant (scope: worktree-git), finding #5 (medium).
//!
//! `is_squash_merged_diff` (branch_sweep.rs) calls `make_scm_provider(...).pr_list(...)`,
//! which for GitHub runs the `gh` CLI via `GitHubScmProvider::run`. That helper
//! uses a bare, UNBOUNDED `cmd.output()` — no timeout — unlike the local git
//! helpers, which route through `git_helpers::spawn_group_bounded`
//! (`LOCAL_GIT_TIMEOUT` / `NETWORK_GIT_TIMEOUT`) with a process-group kill on the
//! deadline. `is_squash_merged` is reached from `worktree_cleanup::
//! prune_orphaned_branches` inside the per-tick auto cleanup sweep, so a slow /
//! hanging `gh` (network stall, auth prompt, rate-limit retry) blocks the
//! daemon's cleanup thread with no upper bound, once per tick, per non-merged
//! branch, per repo.
//!
//! This invariant is RED while `GitHubScmProvider::run` spawns `gh` with an
//! unbounded `.output()`. It goes GREEN when the fix bounds the subprocess with
//! a timeout (e.g. routing the prebuilt `Command` through
//! `git_helpers::spawn_group_bounded`, or a `wait_timeout` / watchdog kill).

use std::path::PathBuf;

/// Extract the body text of the first fn whose signature line contains
/// `sig_needle`, via brace balancing.
fn fn_body(src: &str, sig_needle: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.contains(sig_needle))
        .unwrap_or_else(|| panic!("signature `{sig_needle}` not found in source"));
    let mut depth: i32 = 0;
    let mut seen_open = false;
    let mut out = String::new();
    for line in &lines[start..] {
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
#[ignore = "worktree-git #5 gh-subprocess-timeout: red until fix; remove #[ignore] after fix to confirm"]
fn github_scm_provider_run_is_timeout_bounded_worktree_git_5() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/scm/mod.rs");
    let src = std::fs::read_to_string(&path).expect("read src/scm/mod.rs");

    // The `gh` runner helper: `fn run(args: &[String], cwd: Option<&Path>) -> …`.
    let body = fn_body(&src, "fn run(args: &[String]");

    // Sanity: this really is the `gh` spawner.
    assert!(
        body.contains("Command::new(\"gh\")"),
        "GitHubScmProvider::run body must spawn the `gh` CLI"
    );

    // A bound is present iff the body routes the subprocess through the
    // bounded-spawn machinery (or otherwise enforces a deadline / kill).
    let bounded = body.contains("spawn_group_bounded")
        || body.contains("_timeout")
        || body.contains("wait_timeout")
        || body.contains("kill");

    assert!(
        bounded,
        "worktree-git #5: `GitHubScmProvider::run` spawns `gh` with an UNBOUNDED \
         `cmd.output()` — no timeout — so a slow/hanging `gh` blocks the per-tick \
         worktree cleanup sweep with no upper bound (unlike the local git helpers, \
         which are bounded via `git_helpers::spawn_group_bounded`). Bound the `gh` \
         subprocess with a timeout + process-group kill.\n\n\
         --- GitHubScmProvider::run() body ---\n{body}"
    );
}
