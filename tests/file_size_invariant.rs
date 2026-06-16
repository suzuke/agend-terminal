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
//!   recorded explicitly so the guard still actively prevents *new* files and
//!   the rest of the tree from regrowing. Each entry carries the file's LOC at
//!   grandfather time as a per-file CEILING: the file may SHRINK but must not
//!   grow past it (can-shrink-not-grow), so the debt can only get smaller.
//!   These are technical debt that should be split; remove each from the list
//!   once it is back under `MAX_LOC`. They are NOT silently skipped: the test
//!   asserts each still exists, is still over `MAX_LOC`, and is `<=` its
//!   ceiling, so a stale or regrown entry fails loud.

use std::path::{Path, PathBuf};

const HANDLERS_DIR: &str = "src/mcp/handlers";
const MAX_LOC: usize = 750;

/// Files skipped by exact file name (not handler implementations).
const SKIP_FILES: &[&str] = &["dispatch.rs"];

/// Pre-existing oversized handler implementations: `(path-suffix, ceiling)`
/// where `ceiling` is the file's recorded LOC at grandfather time. Technical
/// debt — each may SHRINK but must not grow past its ceiling (can-shrink-not-
/// grow), and must be removed once split back under `MAX_LOC`.
const KNOWN_OVERSIZED: &[(&str, usize)] = &[
    // #t-61: src/mcp/handlers/ci/mod.rs was split into per-action submodules
    // (checkout/watch/merge/cleanup/release), each under MAX_LOC — its
    // KNOWN_OVERSIZED entry is removed so the guard re-arms for the file.
    // #2234 Phase 1c: +12 for the (B) in-place-checkout dispatch branch + rollback
    // (the workspace-worktree resolution itself is extracted to worktree_pool).
    // dispatch_hook/mod.rs remains slated for a split (its own follow-up).
    ("src/mcp/handlers/dispatch_hook/mod.rs", 1575),
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
        if KNOWN_OVERSIZED.iter().any(|(s, _)| path_ends_with(path, s)) {
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

    // Keep the grandfather list honest AND ratcheted. Each entry records the
    // file's LOC at grandfather time as a CEILING. Every entry must:
    //   1. still exist (else remove it),
    //   2. still be over MAX_LOC (else it was split — remove it so the guard
    //      re-arms via the main scan above), and
    //   3. be <= its recorded ceiling (can-shrink-not-grow — a regrown file
    //      fails here; do NOT raise the number to make it pass).
    for (known, ceiling) in KNOWN_OVERSIZED {
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
        assert!(
            loc <= *ceiling,
            "KNOWN_OVERSIZED entry {known} grew from {ceiling} to {loc} LOC — \
             grandfathered files may shrink but must not grow past their ceiling. \
             Split it back down; do NOT raise the recorded ceiling."
        );
    }
}

/// #2140 follow-up A: pin `KNOWN_OVERSIZED`'s paths to the merge-freshness gate's
/// grandfathered list (the shared single source of truth). Without this, adding a
/// third oversized handler here but forgetting `is_invariant_input` would silently
/// un-gate that file from the #2140 stale-base merge protection — the very
/// silent-gate-miss class #2140 closes. Both sides read
/// `GRANDFATHERED_OVERSIZED_HANDLERS` (the gate via `is_invariant_input`, this
/// test directly), so any divergence fails CI loud.
#[test]
fn known_oversized_paths_match_merge_freshness_inputs() {
    use std::collections::BTreeSet;
    let ceiling_paths: BTreeSet<&str> = KNOWN_OVERSIZED.iter().map(|(p, _)| *p).collect();
    let gate_paths: BTreeSet<&str> =
        agend_terminal::invariant_inputs::GRANDFATHERED_OVERSIZED_HANDLERS
            .iter()
            .copied()
            .collect();
    assert_eq!(
        ceiling_paths, gate_paths,
        "drift between KNOWN_OVERSIZED and the merge-freshness gate's grandfathered \
         list (GRANDFATHERED_OVERSIZED_HANDLERS): a grandfathered oversized file must \
         appear in BOTH, else it is silently un-gated from #2140's stale-base merge \
         refusal. Add the new path to GRANDFATHERED_OVERSIZED_HANDLERS (gate input) \
         and KNOWN_OVERSIZED (with its LOC ceiling)."
    );
}
