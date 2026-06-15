//! Review-repro static-invariant (SCOPEKEY: tasks) — FINDING #3.
//!
//! lifecycle archive write is non-atomic, unfsynced, and swallows its error.
//! `archive_done_tasks` appends each Done task to `tasks-archive.jsonl` via a
//! raw `OpenOptions::append` + `let _ = writeln!(...)` (error dropped) with NO
//! `sync_all`, and there is no ordering/atomicity guarantee with the subsequent
//! board-removal `Cancelled` emit. On crash between the two the task stays Done
//! and is re-archived next boot → unbounded duplicate growth.
//!
//! This guard pins the two concretely-testable parts of the fix: the archive
//! append must (a) surface its error rather than `let _ = writeln!`, and (b)
//! be fsynced (`sync_all`). RED now (error swallowed + no fsync); GREEN once
//! the archive write is durable + error-checked.

use std::path::{Path, PathBuf};

fn read_lifecycle() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/tasks/lifecycle.rs");
    std::fs::read_to_string(&p).expect("read src/tasks/lifecycle.rs")
}

/// Return only the production region (everything before the `#[cfg(test)]`
/// module) so test fixtures can't mask or trip the scan.
fn production_region(text: &str) -> &str {
    match text.find("#[cfg(test)]") {
        Some(i) => &text[..i],
        None => text,
    }
}

#[test]
fn lifecycle_archive_write_is_durable_and_error_checked_tasks() {
    let text = read_lifecycle();
    let prod = production_region(&text);

    // (a) The archive append must NOT swallow its error with `let _ = writeln!`.
    let swallows_error = prod.contains("let _ = writeln!");
    assert!(
        !swallows_error,
        "FINDING #3: the archive append swallows its IO error with `let _ = writeln!` \
         — a failed archive write is invisible. Surface (log/return) the error instead."
    );

    // (b) The archive file must be fsynced so a crash can't leave a torn/lost
    // append that re-archives next boot.
    let fsynced = prod.contains("sync_all") || prod.contains("sync_data");
    assert!(
        fsynced,
        "FINDING #3: the archive file is written with no fsync (no `sync_all`), so the \
         append is not durable and a crash can re-archive the same Done task — \
         unbounded duplicate growth in tasks-archive.jsonl."
    );

    let _ = Path::new("");
}
