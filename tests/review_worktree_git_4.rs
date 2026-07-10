#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Review-repro static invariant (scope: worktree-git), finding #4 (low).
//!
//! `worktree_pool::gc_remove_one` must not run `git worktree remove` without a
//! resolved owning-repo cwd. The original bug shape was:
//!
//! ```ignore
//! let mut cmd = std::process::Command::new("git");
//! cmd.args(["worktree", "remove", "--force", &wt_path…]).env("AGEND_GIT_BYPASS", "1");
//! if let Some(ref sr) = source_repo {
//!     cmd.current_dir(sr);
//! }
//! match cmd.output() { … }   // ← ran even when source_repo is None
//! ```
//!
//! When `source_repo` is `None`, git inherits the daemon process's cwd → wrong
//! repo, then `remove_dir_all` can delete the dir without pruning the owning
//! registry (prunable-registry leak).
//!
//! GREEN shapes (either is fine):
//! - early-return / archive fallthrough when `resolve_source_repo` is `None`,
//!   then remove only with a mandatory cwd; OR
//! - route through `git_worktree::remove_force(&source_repo, …)` after a
//!   mandatory `let Some(source_repo) = resolve… else { fallthrough }` so the
//!   empty-cwd raw branch is never taken from this caller.
//!
//! RED: a conditional `cmd.current_dir(sr)` still wrapping the removal spawn.

use std::path::PathBuf;

/// Extract the body text of the first top-level fn whose signature line
/// contains `sig_needle`, via brace balancing.
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

/// True iff some `if let Some(ref sr) = source_repo {` block in `body` sets the
/// removal command's cwd conditionally (`cmd.current_dir(sr)` within the next
/// few non-blank lines) — i.e. the bug shape where `git worktree remove` can run
/// with NO cwd. The line-1200 prune fallback (`git_ok(sr, …)`) does NOT match.
fn has_conditional_removal_cwd(body: &str) -> bool {
    let lines: Vec<&str> = body.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if !line.contains("if let Some(ref sr) = source_repo") {
            continue;
        }
        // Scan the next handful of lines for the conditional removal-cmd cwd.
        let end = (i + 5).min(lines.len());
        for follow in &lines[i + 1..end] {
            if follow.contains("cmd.current_dir(sr)") {
                return true;
            }
        }
    }
    false
}

#[test]
fn gc_remove_one_worktree_remove_has_mandatory_cwd_worktree_git_4() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/worktree_pool/gc.rs");
    let src = std::fs::read_to_string(&path).expect("read src/worktree_pool/gc.rs");
    let body = fn_body(&src, "fn gc_remove_one(");

    // Sanity: still the worktree-remove path (raw argv OR remove_force helper).
    let has_raw_remove = body.contains("\"remove\",") && body.contains("\"--force\",");
    let has_remove_force = body.contains("remove_force");
    assert!(
        has_raw_remove || has_remove_force,
        "gc_remove_one body must remove via raw `worktree remove --force` or \
         `git_worktree::remove_force`"
    );

    // Production fix shape: mandatory `let Some(source_repo) = resolve… else`
    // before remove (not a soft Option that still spawns).
    assert!(
        body.contains("resolve_source_repo")
            && (body.contains("let Some(source_repo)")
                || body.contains("let Some(ref source_repo)")),
        "gc_remove_one must resolve source_repo and require Some before remove \
         (mandatory cwd / no inherit-daemon-cwd spawn)\n\n--- body ---\n{body}"
    );

    assert!(
        !has_conditional_removal_cwd(&body),
        "worktree-git #4: `gc_remove_one` must not set the removal cwd only via \
         `if let Some(ref sr) = source_repo {{ cmd.current_dir(sr) }}` while still \
         spawning when None — that reopens the inherited-cwd registry leak.\n\n\
         --- gc_remove_one() body ---\n{body}"
    );
}
