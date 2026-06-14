//! #1092/#1116-class repro (daemon-dispatch-idle batch): `record_pending_decision`
//! cancels a prior same-sender pending decision with a BARE `std::fs::remove_file`
//! that is NOT taken under the `{decision_id}.lock` that `scan_and_emit` (tick
//! thread) and `mark_resolved_for_sender` both hold across their read-modify-write.
//!
//! That unlocked delete can land inside `scan_and_emit`'s read→flip→write window:
//! scan reads the sidecar, `record_pending_decision` removes it, then scan's
//! `write_decision` (atomic_write) re-creates it as `status="timeout"` — the
//! just-cancelled sidecar is RESURRECTED and lingers forever, and a timeout event
//! fires for a decision the operator was superseding.
//!
//! METHOD: static_invariant (source-scan), mirroring `tests/core_mutex_invariant.rs`
//! and the dispatch_idle sibling's `delete_sidecar_locked` discipline. The
//! resurrection window is narrow + non-deterministic to drive through the real
//! tick scheduler, so we verify the FIX SHAPE structurally: the cancel inside
//! `record_pending_decision` must take the per-decision flock
//! (`acquire_file_lock` on `decision_lock_path`) before it removes the sidecar —
//! exactly what `mark_resolved_for_sender` and `scan_and_emit` already do.
//!
//! RED now: `record_pending_decision`'s body calls `remove_file` but NEVER calls
//! `acquire_file_lock` → the cancel is unlocked → assertion fails.
//! GREEN after fix: routing the cancel through a locked delete (mirroring
//! `dispatch_idle::delete_sidecar_locked`) introduces `acquire_file_lock` into the
//! fn body.

use std::path::PathBuf;

/// Brace-match the body of the named free `fn` in `src`, returning its body slice
/// (between the first `{` after the signature and its matching `}`).
fn fn_body<'a>(src: &'a str, fn_anchor: &str) -> &'a str {
    let astart = src
        .find(fn_anchor)
        .unwrap_or_else(|| panic!("anchor `{fn_anchor}` not found in source"));
    let open_rel = src[astart..]
        .find('{')
        .expect("function body must open with a brace");
    let body_start = astart + open_rel;
    let mut depth = 0usize;
    let mut body_end = body_start;
    for (i, c) in src[body_start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    body_end = body_start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(body_end > body_start, "function body must close");
    &src[body_start..=body_end]
}

#[test]
#[ignore = "daemon-dispatch-idle #1092-cancel-unlocked: red until fix; remove #[ignore] after fix to confirm"]
fn record_pending_decision_cancel_is_flocked_daemon_dispatch_idle() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("daemon")
        .join("decision_timeout.rs");
    let src = std::fs::read_to_string(&path).expect("read decision_timeout.rs");

    // Slice off the test module so its `#[cfg(test)]` fixtures can't satisfy us.
    let cfg_test = ["#[cfg(", "test)]"].concat();
    let prod = match src.find(&cfg_test) {
        Some(i) => &src[..i],
        None => &src[..],
    };

    let body = fn_body(prod, "fn record_pending_decision");

    // Sanity: this fn DOES delete a sidecar (the same-sender cancel). If the cancel
    // is ever removed/refactored away this guard would silently pass; pin it.
    let remove_needle = ["remove", "_file"].concat();
    assert!(
        body.contains(&remove_needle),
        "record_pending_decision must still delete the prior same-sender sidecar \
         (the cancel) — guard anchor missing, re-point this test"
    );

    // The bug + the fix: that delete must happen UNDER the per-decision flock, the
    // same `acquire_file_lock(&decision_lock_path(..))` mark_resolved/scan take.
    let lock_needle = ["acquire", "_file_lock"].concat();
    assert!(
        body.contains(&lock_needle),
        "record_pending_decision cancels a prior pending sidecar with an UNLOCKED \
         remove_file — it must acquire the {{decision_id}}.lock (acquire_file_lock on \
         decision_lock_path) before deleting, mirroring mark_resolved_for_sender / \
         scan_and_emit, so the cancel is mutually exclusive with scan_and_emit's \
         read→flip→write window (no resurrection / no lost-fire race)."
    );
}
