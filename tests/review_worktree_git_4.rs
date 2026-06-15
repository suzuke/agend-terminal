#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Review-repro static invariant (scope: worktree-git), finding #4 (low).
//!
//! `worktree_pool::gc_remove_one` resolves `source_repo` via
//! `resolve_source_repo(wt_path)` and then spawns `git worktree remove --force
//! <abs path>` with the cwd set ONLY conditionally:
//!
//! ```ignore
//! let mut cmd = std::process::Command::new(git_bin); // git_bin == "git"
//! cmd.args(["worktree", "remove", "--force", &wt_path…]).env("AGEND_GIT_BYPASS", "1");
//! if let Some(ref sr) = source_repo {
//!     cmd.current_dir(sr);
//! }
//! match cmd.output() { … }   // ← runs even when source_repo is None
//! ```
//!
//! When `source_repo` is `None`, git inherits the daemon process's cwd. If that
//! cwd is inside an unrelated repo/worktree, `git worktree remove` resolves the
//! WRONG repo (typically fails), then the `remove_dir_all` fallback physically
//! deletes the dir but CANNOT prune the owning repo's registry (the prune is
//! guarded by `if let Some(ref sr)`), re-introducing the prunable-registry leak.
//!
//! This invariant is RED while the removal command's cwd is set conditionally on
//! `source_repo` being `Some`. It goes GREEN when the fix makes the cwd
//! MANDATORY before the worktree-remove (e.g. early-return / skip when the owning
//! repo cannot be resolved) — so the conditional `cmd.current_dir(sr)` for the
//! removal command no longer exists.

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
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/worktree_pool.rs");
    let src = std::fs::read_to_string(&path).expect("read src/worktree_pool.rs");
    let body = fn_body(&src, "fn gc_remove_one(");

    // Sanity: the body really is the git-worktree-remove path.
    assert!(
        body.contains("\"remove\",") && body.contains("\"--force\","),
        "gc_remove_one body must contain the `git worktree remove --force` call"
    );

    assert!(
        !has_conditional_removal_cwd(&body),
        "worktree-git #4: `gc_remove_one` sets the `git worktree remove` cwd ONLY \
         `if let Some(ref sr) = source_repo`, but spawns `cmd.output()` regardless — \
         so when source_repo is None git runs against the daemon's INHERITED cwd \
         (wrong repo) and the registry prune is skipped (leak). Make the owning-repo \
         cwd MANDATORY before the worktree-remove (resolve it or early-return/skip), \
         so the removal command never runs with an unset cwd.\n\n\
         --- gc_remove_one() body ---\n{body}"
    );
}
