//! #2031/[M2]-class repro (daemon-dispatch-idle batch): `record_dispatch`'s
//! in-place re-dispatch DEDUP refresh mutates an existing sidecar (reset
//! issued_at / status / refresh_count / exceeded_at …) and persists it via a bare
//! `crate::store::atomic_write`, BYPASSING the `{dispatch_id}.lock` that
//! `scan_and_emit` holds while it re-reads and flips the SAME sidecar to Exceeded.
//!
//! Two concurrent writers (handle_send → record_dispatch vs the tick scan) on the
//! same dispatch_id can clobber each other (LOST UPDATE): the dedup refresh's
//! Pending reset can overwrite a just-written Exceeded (or be overwritten by it),
//! leaving the nudge state inconsistent for a window. The module's own [M2]
//! discipline (`delete_sidecar_locked` for deletes, `with_json_state` RMW under
//! the per-file flock for `reassign` / `refresh_issued_at`) is applied everywhere
//! EXCEPT this dedup-refresh branch.
//!
//! METHOD: static_invariant (source-scan), mirroring `tests/core_mutex_invariant.rs`.
//! The lost-update window is narrow + non-deterministic to drive through the real
//! scheduler, so we verify the FIX SHAPE structurally: the dedup-refresh block must
//! perform its read-modify-write through `crate::store::with_json_state` (which
//! takes the same per-file flock the rest of the module uses), NOT a bare
//! `atomic_write` of the existing sidecar's `pending_path`.
//!
//! RED now: the `if let Some(mut existing) = list_pending(home)` dedup block
//! contains `atomic_write` (unlocked) and no `with_json_state` → assertion fails.
//! GREEN after fix: routing the refresh through `with_json_state::<PendingDispatch,_,_>`
//! removes the bare `atomic_write` from that block and adds `with_json_state`.

use std::path::PathBuf;

/// Brace-match the block opened by the FIRST occurrence of `block_anchor` in
/// `src`, returning the block slice from its opening `{` to the matching `}`.
fn block_after<'a>(src: &'a str, block_anchor: &str) -> &'a str {
    let astart = src
        .find(block_anchor)
        .unwrap_or_else(|| panic!("anchor `{block_anchor}` not found in source"));
    let open_rel = src[astart..]
        .find('{')
        .expect("block must open with a brace");
    let block_start = astart + open_rel;
    let mut depth = 0usize;
    let mut block_end = block_start;
    for (i, c) in src[block_start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    block_end = block_start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(block_end > block_start, "block must close");
    &src[block_start..=block_end]
}

#[test]
#[ignore = "daemon-dispatch-idle dedup-refresh-unlocked: red until fix; remove #[ignore] after fix to confirm"]
fn record_dispatch_dedup_refresh_uses_locked_rmw_daemon_dispatch_idle() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("daemon")
        .join("dispatch_idle")
        .join("mod.rs");
    let src = std::fs::read_to_string(&path).expect("read dispatch_idle/mod.rs");

    // Slice off the test module so its fixtures can't satisfy/poison the scan.
    let cfg_test = ["#[cfg(", "test)]"].concat();
    let prod = match src.find(&cfg_test) {
        Some(i) => &src[..i],
        None => &src[..],
    };

    // The in-place dedup-refresh branch in `record_dispatch`.
    let block = block_after(prod, "if let Some(mut existing) = list_pending(home)");

    // Sanity: this is the refresh block (mutates the reborn-episode fields).
    assert!(
        block.contains("issued_at") && block.contains("refresh_count"),
        "dedup-refresh block anchor drifted — re-point this test (block did not \
         contain the expected in-place reset fields)"
    );

    // The bug: the refresh persists via a bare unlocked atomic_write.
    let atomic_needle = ["atomic", "_write"].concat();
    assert!(
        !block.contains(&atomic_needle),
        "record_dispatch dedup-refresh persists the existing sidecar via an UNLOCKED \
         crate::store::atomic_write, racing scan_and_emit's flocked Pending→Exceeded \
         flip on the same dispatch_id (lost update). Do the read-modify-write through \
         crate::store::with_json_state::<PendingDispatch,_,_> under the same per-file \
         flock the rest of the module uses."
    );

    // The fix shape: the RMW goes through with_json_state (the locked path).
    let locked_needle = ["with", "_json_state"].concat();
    assert!(
        block.contains(&locked_needle),
        "record_dispatch dedup-refresh must perform its RMW under the per-file flock \
         via crate::store::with_json_state (mirroring reassign_pending_for_task / \
         refresh_issued_at), eliminating the lost-update window vs scan_and_emit."
    );
}
