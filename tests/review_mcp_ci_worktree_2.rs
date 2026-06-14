//! Verification / reproduction test for the mcp-ci-worktree batch, finding 2
//! (HIGH, concurrency):
//!
//!   "handle_watch_ci / handle_unwatch_ci do watch-file RMW without the
//!    per-watch flock used everywhere else"
//!
//! Both MCP handlers (`handle_watch_ci`, `handle_unwatch_ci` in
//! `src/mcp/handlers/ci/mod.rs`) read the watch JSON, mutate it in memory, then
//! `crate::store::atomic_write` it back — WITHOUT acquiring the per-watch flock.
//! Every OTHER writer of the same file holds the sibling `<hash>.lock` via
//! `crate::store::acquire_file_lock` precisely to serialize this read→write RMW
//! (`registry.rs` flush_watch_state / update_watch_state_with_notify /
//! cleanup_watches_for_instance / reassign_next_after_ci, each commented
//! "flock the watch so a concurrent poll/unwatch doesn't race the RMW"). Because
//! the MCP path skips that lock, an MCP `ci watch`/`ci unwatch` can interleave
//! with the daemon poll loop and clobber poll-cursor fields / a just-added or
//! just-removed subscriber. `atomic_write` only makes each write all-or-nothing;
//! it does NOT prevent lost updates across the read→write gap.
//!
//! Method: STATIC_INVARIANT (source-scanning). The data race needs the fix to
//! TAKE the lock before it can be driven without flake, so we assert the
//! structural GUARD is present: each MCP RMW handler body must reference
//! `acquire_file_lock`. RED now (neither handler locks). GREEN once the fix
//! adds `let _lock = crate::store::acquire_file_lock(&watch_path.with_extension(
//! "lock"))?;` to both RMW windows. Mirrors `tests/flock_depth_invariant.rs` /
//! `tests/core_mutex_invariant.rs` harness.

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

/// Extract the body lines of a top-level `fn` (or `pub(...) fn`) whose
/// declaration line CONTAINS `signature_needle`. The body runs from the
/// declaration line until the NEXT line that begins (at column 0) a new
/// top-level item (`fn `, `pub`, `///`, `//!`, `#[`, `enum`, `struct`, `impl`),
/// which is where the previous item ended. Returns the joined body text, or an
/// empty string if the signature was not found.
fn fn_body_after(text: &str, signature_needle: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines.iter().position(|l| l.contains(signature_needle)) else {
        return String::new();
    };
    let mut body = String::new();
    // include the declaration line itself
    body.push_str(lines[start]);
    body.push('\n');
    for line in &lines[start + 1..] {
        // A new top-level item at column 0 ends the current function body.
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
#[ignore = "mcp-ci-worktree-2: mcp-watch-rmw-missing-flock; red until fix; remove #[ignore] after fix to confirm"]
fn mcp_ci_watch_handlers_hold_per_watch_flock_mcp_ci_worktree() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src, &mut files);
    assert!(!files.is_empty(), "no src/*.rs files found");

    // Locate the ci MCP handler module that owns the two RMW handlers.
    let ci_mod = files
        .iter()
        .find(|p| p.ends_with(Path::new("mcp/handlers/ci/mod.rs")))
        .cloned()
        .expect("src/mcp/handlers/ci/mod.rs must exist");
    let text = std::fs::read_to_string(&ci_mod).expect("read ci/mod.rs");

    // The two handlers that perform a read→mutate→atomic_write RMW on the watch
    // file. Each must serialize that window under the SAME flock the daemon-side
    // writers use (`crate::store::acquire_file_lock`).
    let targets = [
        ("fn handle_watch_ci", "handle_watch_ci"),
        ("fn handle_unwatch_ci", "handle_unwatch_ci"),
    ];

    let mut unguarded = Vec::new();
    for (sig, label) in targets {
        let body = fn_body_after(&text, sig);
        assert!(
            !body.is_empty(),
            "could not locate `{sig}` in ci/mod.rs — re-check signature drift"
        );
        // Sanity: this body really does an atomic_write RMW (so the guard is
        // load-bearing, not vacuously satisfied).
        assert!(
            body.contains("atomic_write"),
            "{label}: expected an atomic_write RMW in this handler"
        );
        if !body.contains("acquire_file_lock") {
            unguarded.push(label);
        }
    }

    assert!(
        unguarded.is_empty(),
        "#692/#1882: these ci MCP handlers do a watch-file read→mutate→atomic_write \
         RMW WITHOUT the per-watch flock that every daemon-side writer holds \
         (registry.rs uses `crate::store::acquire_file_lock(&path.with_extension(\"lock\"))`), \
         so a concurrent poll/unwatch loses updates (atomic_write does NOT serialize the \
         read→write gap). Acquire the same flock across each RMW window: {unguarded:?}"
    );
}
