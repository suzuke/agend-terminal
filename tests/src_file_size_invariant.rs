#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Anti-monolith file-size ratchet for all production `src/` files.
//!
//! Complements `tests/file_size_invariant.rs` (which holds `src/mcp/handlers`
//! to a tight 750-LOC bound). This one applies a looser, repo-wide
//! anti-monolith ceiling so no production file regrows into the 4k–6k-line
//! monsters that were split during the 2026-06 refactor (`worktree_pool.rs`
//! 6011→642, `supervisor.rs` 6439→2123, `task_events.rs` 4457→2070,
//! `bin/agend-git.rs` 5532→2488). New production files must stay under
//! `MAX_LOC`; the handful already above it are grandfathered with their
//! current LOC as a can-shrink-not-grow ceiling.
//!
//! **Test files are exempt** (test modules are allowed to be large) — see
//! `is_test_file`. Re-home a large inline `mod tests {}` into a sibling
//! `foo/tests.rs` (the established split pattern) rather than carrying it in
//! the production file.

use std::path::{Path, PathBuf};

/// Repo-wide anti-monolith ceiling. Deliberately looser than the 750-LOC
/// `src/mcp/handlers` bound: the goal here is "never a monolith again", not
/// "every file tiny". Lower it over time as the grandfathered debt shrinks.
const MAX_LOC: usize = 2500;

/// Pre-existing oversized production files: `(path-suffix, ceiling)` where
/// `ceiling` is the file's LOC when grandfathered. Each may SHRINK but must not
/// grow past its ceiling; remove the entry once it drops under `MAX_LOC` so the
/// main scan re-arms for it. Sorted largest-first.
const GRANDFATHERED: &[(&str, usize)] = &[
    ("src/daemon/dispatch_idle/mod.rs", 3962),
    ("src/app/mod.rs", 3457),
    ("src/daemon/pr_state/mod.rs", 3428),
    ("src/api/handlers/messaging.rs", 3239),
    ("src/daemon/mod.rs", 3217),
    ("src/agent/mod.rs", 3216),
    ("src/vterm.rs", 3103),
    ("src/deployments.rs", 2806),
];

/// True for files allowed to be large because they are test code, not
/// production: `tests.rs` submodules, `*_tests.rs`, `*tests*.rs` (e.g.
/// `poller_tests.rs`), `review_repro_*.rs`, and perf/bench equivalence files.
fn is_test_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name == "tests.rs"
        || name.ends_with("_tests.rs")
        || name.contains("tests")
        || name.starts_with("review_repro")
        || name.ends_with("_bench.rs")
        || name.contains("perf_r3")
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
}

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
fn production_src_files_under_anti_monolith_ceiling() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src, &mut files);

    let mut violations = Vec::new();
    for path in &files {
        if is_test_file(path) {
            continue;
        }
        if GRANDFATHERED.iter().any(|(s, _)| path_ends_with(path, s)) {
            continue;
        }
        let loc = loc_of(path);
        if loc > MAX_LOC {
            violations.push(format!("{}: {} LOC (max {})", path.display(), loc, MAX_LOC));
        }
    }
    assert!(
        violations.is_empty(),
        "production src/ file(s) exceed the {MAX_LOC}-LOC anti-monolith ceiling. \
         Split the file (re-home `mod tests` into a sibling `foo/tests.rs`, or \
         extract cohesive submodules) — do NOT grandfather a new monolith:\n{}",
        violations.join("\n")
    );

    // Ratchet: every grandfathered entry must still exist, still be over
    // MAX_LOC, and be <= its recorded ceiling (can-shrink-not-grow).
    for (known, ceiling) in GRANDFATHERED {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(known);
        assert!(
            path.exists(),
            "GRANDFATHERED entry {known} no longer exists — remove it from the list"
        );
        let loc = loc_of(&path);
        assert!(
            loc > MAX_LOC,
            "GRANDFATHERED entry {known} is now {loc} LOC (<= {MAX_LOC}) — it has been \
             split; remove it from GRANDFATHERED so the main scan re-arms for it"
        );
        assert!(
            loc <= *ceiling,
            "GRANDFATHERED entry {known} grew from {ceiling} to {loc} LOC — \
             grandfathered files may shrink but must not grow past their ceiling. \
             Split it back down; do NOT raise the recorded ceiling."
        );
    }
}
