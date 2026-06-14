//! Static-invariant repro (panic_io_extra scope) for: operator-mode authority
//! gate written non-atomically.
//!
//! `set_mode` in src/operator_mode.rs persists the fleet-wide authority gate
//! (`operator-mode.json`) with `std::fs::write(path(home), &json)` — a
//! truncate-then-write that leaves a TRUNCATED file on a mid-write crash. The
//! read path is fail-closed, so the operator's configured mode is silently lost.
//! The fix routes the data file through `crate::store::atomic_write` (matching
//! binding.rs).
//!
//! The crash window cannot be driven without a fault-injection seam, so this is
//! a source-scanning guard (mirrors tests/core_mutex_invariant.rs): it asserts
//! the non-atomic production write is GONE. RED now (the needle is present at
//! the data-write line), GREEN after the fix.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

/// The exact production needle (line ~207). Test fixtures in the same file use
/// `std::fs::write(path(&home), ...)` with a byte-literal, NOT `path(home), &json`,
/// so this string is unique to the production data write and we don't need to
/// strip the `#[cfg(test)]` block.
const NEEDLE: &str = "std::fs::write(path(home), &json)";

#[test]
#[ignore = "operator_mode-nonatomic-gate: red until fix; remove #[ignore] after fix to confirm"]
fn operator_mode_data_write_is_atomic_panic_io_extra() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/operator_mode.rs");
    let text = std::fs::read_to_string(&file).expect("read src/operator_mode.rs");

    let mut violations = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            continue; // skip comment/doc lines
        }
        if line.contains(NEEDLE) {
            violations.push(format!("{}:{}: {}", file.display(), i + 1, line.trim()));
        }
    }

    assert!(
        violations.is_empty(),
        "operator-mode authority gate (operator-mode.json) is written non-atomically via \
         `std::fs::write` (truncate-then-write) instead of crate::store::atomic_write — a \
         mid-write crash truncates the fleet-wide gate and the fail-closed reader silently \
         reverts to restrictive. Use crate::store::atomic_write(path(home), json.as_bytes()):\n{}",
        violations.join("\n")
    );
}
