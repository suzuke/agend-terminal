#![allow(clippy::unwrap_used, clippy::expect_used)]

//! xcut-concurrency F2 (durability): `store::atomic_write` is the SOLE durable
//! write chokepoint for the daemon (watch state, bindings, decisions, fleet
//! metadata, ci-handoff tracks, operator-mode.json, lease markers, ...). It
//! writes a temp file, `f.sync_all()`s the FILE CONTENTS, then
//! `std::fs::rename(&tmp, path)`. But it never fsyncs the PARENT DIRECTORY after
//! the rename. On crash/power-loss after `rename(2)` returns but before the
//! directory entry is flushed, the rename can be lost even though the content
//! was synced — defeating the "atomic AND durable replace" contract.
//!
//! Correct behavior: after the rename, open the parent directory and
//! `sync_all()` it (a SECOND sync_all, occurring AFTER the rename line). This
//! test extracts the `atomic_write` fn body and asserts a `sync_all` call exists
//! AFTER the `std::fs::rename` line. Red now (the only sync_all is on the temp
//! file, BEFORE the rename); green once the post-rename directory fsync lands.

use std::path::PathBuf;

#[test]
#[ignore = "xcut-concurrency F2: red until fix; remove #[ignore] after fix to confirm"]
fn atomic_write_fsyncs_parent_dir_after_rename_xcut_concurrency() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/store.rs");
    let text =
        std::fs::read_to_string(&path).expect("xcut-concurrency F2: src/store.rs must exist");
    let lines: Vec<&str> = text.lines().collect();

    // Locate the `atomic_write` fn opener.
    let start = lines
        .iter()
        .position(|l| l.trim_start().starts_with("pub fn atomic_write"))
        .expect("xcut-concurrency F2: `pub fn atomic_write` must exist in store.rs");

    // The fn body ends at the next top-level `fn`/`pub fn` opener (column 0).
    // `save_atomic` is the documented next item, but scan generically for
    // robustness against reordering.
    let mut end = lines.len();
    for (off, l) in lines.iter().enumerate().skip(start + 1) {
        let is_top_level_fn = (l.starts_with("pub fn ")
            || l.starts_with("fn ")
            || l.starts_with("pub(crate) fn ")
            || l.starts_with("pub(super) fn "))
            && !l.starts_with(' ');
        if is_top_level_fn {
            end = off;
            break;
        }
    }
    let body = &lines[start..end];

    // Index (within the body) of the rename that publishes the new contents.
    let rename_idx = body
        .iter()
        .position(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with('*') && t.contains("std::fs::rename")
        })
        .expect(
            "xcut-concurrency F2: atomic_write must contain `std::fs::rename` — the publish step",
        );

    // A `sync_all()` (on the opened parent directory) must appear AFTER the
    // rename. Comment/doc lines that merely mention it do not count.
    let has_post_rename_dir_sync = body.iter().skip(rename_idx + 1).any(|l| {
        let t = l.trim_start();
        !t.starts_with("//") && !t.starts_with('*') && t.contains("sync_all")
    });

    assert!(
        has_post_rename_dir_sync,
        "xcut-concurrency F2: atomic_write fsyncs the temp file but never fsyncs the PARENT \
         DIRECTORY after `std::fs::rename`. A crash after the rename returns but before the \
         directory entry is flushed can lose the rename — the durable-replace contract is \
         broken. After the rename, open the parent dir and call `.sync_all()` on it (a second \
         post-rename sync_all). Current atomic_write body has its only sync_all BEFORE the rename."
    );
}
