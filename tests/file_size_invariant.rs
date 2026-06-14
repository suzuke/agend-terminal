//! File size invariant — prevents MCP handler monolith regrowth.
//!
//! Sprint 26 PR-C: after splitting src/mcp/handlers.rs (3223 LOC) into
//! sub-modules, this test enforces that no handler implementation file
//! exceeds `MAX_LOC`. Prevents the split-then-regrow pattern observed in
//! prior commit 386b98d.
//!
//! The walk is RECURSIVE: handler implementations also live in nested
//! sub-modules (`ci/`, `dispatch_hook/`, `comms_gates/`, `force_release/`,
//! `instance_state/`), so a top-level-only scan would let the largest
//! handlers regrow undetected. (It did: `ci/mod.rs` and
//! `dispatch_hook/mod.rs` had already grown past the limit while a
//! non-recursive check stayed green.)
//!
//! **Skip rules**
//! - Any file whose name contains `test` — test modules are allowed to be
//!   large.
//! - `dispatch.rs` — the routing-table registry introduced in #694 BLOCK 2;
//!   a single file by design (centralized name→handler mapping + per-tool
//!   action sub-routing tables), not a handler implementation.
//! - `KNOWN_OVERSIZED` — pre-existing oversized handler implementations,
//!   recorded explicitly so the guard still actively prevents *new* files
//!   and the rest of the tree from regrowing. These are technical debt that
//!   should be split; remove each from the list once it is back under
//!   `MAX_LOC`. They are NOT silently skipped: the test asserts each one
//!   still exists and is still over the limit, so a stale entry fails loud.

use std::path::{Path, PathBuf};

const HANDLERS_DIR: &str = "src/mcp/handlers";
const MAX_LOC: usize = 750;

/// Files skipped by exact file name (not handler implementations).
const SKIP_FILES: &[&str] = &["dispatch.rs"];

/// Pre-existing oversized handler implementations, matched by path suffix
/// relative to the repo root. Technical debt — split and remove from here.
const KNOWN_OVERSIZED: &[&str] = &[
    "src/mcp/handlers/ci/mod.rs",
    "src/mcp/handlers/dispatch_hook/mod.rs",
];

/// Recursively collect every `*.rs` file under `dir`.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read handlers dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
}

/// True if `path` ends with the given repo-relative suffix (`/`-normalized).
fn path_ends_with(path: &Path, suffix: &str) -> bool {
    path.to_string_lossy().replace('\\', "/").ends_with(suffix)
}

fn loc_of(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .expect("read file")
        .lines()
        .count()
}

#[test]
fn mcp_handler_files_under_max_loc() {
    let dir = Path::new(HANDLERS_DIR);
    assert!(
        dir.is_dir(),
        "src/mcp/handlers must be a directory (not a single file)"
    );

    let mut files = Vec::new();
    collect_rs_files(dir, &mut files);

    let mut violations = Vec::new();
    for path in &files {
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if file_name.contains("test") || SKIP_FILES.contains(&file_name) {
            continue;
        }
        if KNOWN_OVERSIZED.iter().any(|s| path_ends_with(path, s)) {
            continue;
        }
        let loc = loc_of(path);
        if loc > MAX_LOC {
            violations.push(format!("{}: {} LOC (max {})", path.display(), loc, MAX_LOC));
        }
    }

    assert!(
        violations.is_empty(),
        "MCP handler files exceed {MAX_LOC} LOC limit (split them, or — only \
         if intentionally a registry like dispatch.rs — add to SKIP_FILES):\n{}",
        violations.join("\n")
    );

    // Keep the grandfather list honest: every KNOWN_OVERSIZED entry must
    // still exist AND still be over the limit. Once a file is split back
    // under MAX_LOC, this fails until the entry is removed — preventing the
    // list from quietly masking a newly-regrown file under an old name.
    for known in KNOWN_OVERSIZED {
        let path = Path::new(known);
        assert!(
            path.exists(),
            "KNOWN_OVERSIZED entry {known} no longer exists — remove it from the list"
        );
        let loc = loc_of(path);
        assert!(
            loc > MAX_LOC,
            "KNOWN_OVERSIZED entry {known} is now {loc} LOC (<= {MAX_LOC}) — it has \
             been split; remove it from KNOWN_OVERSIZED so the guard re-arms for it"
        );
    }
}
