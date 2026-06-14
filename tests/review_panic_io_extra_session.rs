//! Static-invariant repro (panic_io_extra scope) for: session.json written
//! non-atomically (and error dropped).
//!
//! `save_session` / `save_session_if_changed` in src/app/session.rs write the
//! throttled layout snapshot with `std::fs::write(&path, &json)`. That truncates
//! first, so a hard crash DURING the write corrupts/truncates session.json — the
//! exact crash the throttled write exists to survive (it's the file meant to
//! preserve the on-screen layout across kill -9 / power loss). The fix routes
//! the data file through `crate::store::atomic_write`.
//!
//! The crash window cannot be driven without a fault-injection seam, so this is
//! a source-scanning guard (mirrors tests/core_mutex_invariant.rs): it asserts
//! the non-atomic production write is GONE. RED now (the needle is present at
//! both production write sites), GREEN after the fix.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

/// The exact production needle (lines ~137 and ~160). Test fixtures in the same
/// file use `std::fs::write(&path, serde_json::to_string_pretty(...))`, NOT
/// `&path, &json`, so this string is unique to the two production writes and we
/// don't need to strip the `#[cfg(test)]` block.
const NEEDLE: &str = "std::fs::write(&path, &json)";

#[test]
#[ignore = "session-json-nonatomic: red until fix; remove #[ignore] after fix to confirm"]
fn session_json_data_write_is_atomic_panic_io_extra() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/app/session.rs");
    let text = std::fs::read_to_string(&file).expect("read src/app/session.rs");

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
        "session.json is written non-atomically via `std::fs::write` (truncate-then-write), so a \
         crash during the throttled write corrupts the very layout file it exists to preserve. \
         Route through crate::store::atomic_write(&path, json.as_bytes()):\n{}",
        violations.join("\n")
    );
}
