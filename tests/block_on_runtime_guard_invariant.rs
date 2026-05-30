//! #1476 invariant: every shared-runtime `<name>_runtime().block_on(...)` call
//! MUST sit inside a Handle-guarded helper, so it never panics "Cannot start a
//! runtime from within a runtime" when reached from a tokio context.
//!
//! Background: telegram (#1474) and discord (#1476) shipped the SAME
//! copy-pasted bug — a sync→async bridge calling `block_on` on a shared
//! `current_thread` runtime, which is safe only until a caller invokes it from
//! within a runtime (teloxide 0.17 / reqwest 0.12 made that path reachable and
//! it panicked on the next daemon restart). The fix is a `block_on_value`-style
//! helper guarding with `Handle::try_current` → run on a scoped thread with a
//! fresh runtime. This test fails loud if a future bridge adds another raw
//! shared-runtime `block_on`, closing the copy-paste hole.
//!
//! "Guarded" = the enclosing fn (scanning backward to its `fn` opener, capped)
//! contains a `Handle::try_current` or `std::thread::scope`. Local-runtime
//! `rt.block_on` (a freshly-built, never-shared runtime) does not match the
//! `_runtime().block_on` marker and is intentionally exempt — a non-shared
//! runtime is never nested.

use std::path::{Path, PathBuf};

/// Marker for the dangerous pattern: `block_on` on a shared `*_runtime()`
/// accessor. Excludes local `rt.block_on` (own fresh runtime, never nested).
const MARKER: &str = "_runtime().block_on";

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs_files(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

/// A match line is guarded if a `Handle::try_current` / `thread::scope` appears
/// between it and the start of its enclosing fn (capped at 40 lines back).
fn enclosing_fn_is_guarded(lines: &[&str], match_idx: usize) -> bool {
    let mut i = match_idx;
    let mut scanned = 0;
    while i > 0 && scanned < 40 {
        i -= 1;
        scanned += 1;
        let line = lines[i];
        if line.contains("Handle::try_current") || line.contains("thread::scope") {
            return true;
        }
        let t = line.trim_start();
        // Reached the enclosing fn opener without finding a guard.
        if t.starts_with("fn ")
            || t.starts_with("pub fn ")
            || t.starts_with("pub(crate) fn ")
            || t.starts_with("pub(super) fn ")
            || t.starts_with("async fn ")
        {
            return false;
        }
    }
    false
}

#[test]
fn shared_runtime_block_on_must_be_handle_guarded() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    assert!(!files.is_empty(), "no .rs files found under src/");

    let mut violations = Vec::new();
    for f in &files {
        let Ok(content) = std::fs::read_to_string(f) else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            if !line.contains(MARKER) {
                continue;
            }
            // Skip comment/doc lines that merely mention the pattern.
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with("*") {
                continue;
            }
            if !enclosing_fn_is_guarded(&lines, idx) {
                violations.push(format!("{}:{}: {}", f.display(), idx + 1, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "#1476: unguarded shared-runtime block_on found — every `<name>_runtime().block_on` \
         must go through a Handle-guarded helper (e.g. `block_on_value`, mirroring telegram \
         state.rs / discord.rs) so it can't panic from within a tokio runtime:\n{}",
        violations.join("\n")
    );
}
