//! Verification / reproduction test for the mcp-ci-worktree batch, finding 3
//! (LOW, resource-leak):
//!
//!   "handle_release_repo leaks .git/worktrees metadata when worktree .git is
//!    not a readable file"
//!
//! In `handle_release_repo` (`src/mcp/handlers/ci/mod.rs`) `source_repo` is
//! derived ONLY when `canonical.join(".git").is_file()` AND the file is readable
//! AND its `gitdir:` line parses with three resolvable parents. If any of those
//! fail, `source_repo` is `None`. On the two fallback arms (git worktree remove
//! returned non-zero, or spawn failed) the code force-removes the working-tree
//! dir via `remove_dir_all` but SKIPS `worktree::prune(src)` because src is
//! `None` — leaving stale `<source>/.git/worktrees/<meta>/` metadata in the
//! source repo, with NO warning surfaced to the caller. This is exactly the
//! half-cleanup state `force_release`/`gc.rs` exists to repair, leaked silently.
//!
//! Why static_invariant (not behavioral): the fallback arms only run when
//! `git worktree remove` returns NON-ZERO or spawn-fails. In this repo `git` is
//! intercepted by the fleet `agend-git` shim and (depending on env/PATH/git
//! version) a remove on a non-worktree path commonly exits 0 — so the fallback
//! arm cannot be driven deterministically through the real entry point across
//! environments. Per the user's "all code is testable — pick the method"
//! principle, we assert the structural FIX instead: the suggestion is to emit a
//! `tracing::warn!` (and/or a response `warning` field) when `source_repo` is
//! None so the leak is diagnosable. We scan `handle_release_repo`'s body for
//! that diagnostic. RED now (the body has NEITHER a `tracing::warn` NOR a
//! `"warning"` key — only silent `remove_dir_all` + conditional `prune`).
//! GREEN once the fix surfaces the un-prunable-metadata case. Mirrors
//! `tests/core_mutex_invariant.rs` / `tests/flock_depth_invariant.rs`.

use std::path::{Path, PathBuf};

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir src") {
        let p = entry.expect("dir entry").path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

/// Extract the body text of the top-level item whose declaration line contains
/// `signature_needle`, up to (but not including) the next top-level item. See
/// the sibling finding-2 invariant for the boundary heuristic rationale.
fn fn_body_after(text: &str, signature_needle: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines.iter().position(|l| l.contains(signature_needle)) else {
        return String::new();
    };
    let mut body = String::new();
    body.push_str(lines[start]);
    body.push('\n');
    for line in &lines[start + 1..] {
        let starts_new_item = line.starts_with("fn ")
            || line.starts_with("pub ")
            || line.starts_with("pub(")
            || line.starts_with("///")
            || line.starts_with("//!")
            || line.starts_with("#[")
            || line.starts_with("enum ")
            || line.starts_with("struct ")
            || line.starts_with("impl ")
            || line.starts_with("mod ");
        if starts_new_item {
            break;
        }
        body.push_str(line);
        body.push('\n');
    }
    body
}

#[test]
#[ignore = "mcp-ci-worktree-3: release-repo-silent-metadata-leak; red until fix; remove #[ignore] after fix to confirm"]
fn release_repo_surfaces_unprunable_metadata_leak_mcp_ci_worktree() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src, &mut files);
    assert!(!files.is_empty(), "no src/*.rs files found");

    let ci_mod = files
        .iter()
        .find(|p| p.ends_with(Path::new("mcp/handlers/ci/mod.rs")))
        .cloned()
        .expect("src/mcp/handlers/ci/mod.rs must exist");
    let text = std::fs::read_to_string(&ci_mod).expect("read ci/mod.rs");

    let body = fn_body_after(&text, "fn handle_release_repo");
    assert!(
        !body.is_empty(),
        "could not locate `handle_release_repo` in ci/mod.rs — re-check signature drift"
    );

    // Sanity: confirm we located the RIGHT function — it must contain the
    // conditional prune whose `None` branch is the silent leak.
    assert!(
        body.contains("crate::worktree::prune") && body.contains("remove_dir_all"),
        "located body does not look like handle_release_repo's cleanup path \
         (missing prune/remove_dir_all)"
    );

    // The FIX surfaces the un-prunable-metadata case rather than swallowing it:
    // a `tracing::warn!` and/or a `warning` field in the fallback response.
    let surfaces_leak = body.contains("tracing::warn") || body.contains("\"warning\"");
    assert!(
        surfaces_leak,
        "#release-leak: handle_release_repo force-removes the working tree but SKIPS \
         `worktree::prune` whenever `source_repo` is None (unreadable `.git` pointer / \
         failed path arithmetic), leaving stale `<source>/.git/worktrees/<meta>/` metadata \
         with NO diagnostic. Emit a `tracing::warn!` and/or a response `warning` field on \
         the source_repo==None fallback arms (or recover the source repo before giving up)."
    );
}
