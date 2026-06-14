//! Review-repro static-invariant (SCOPEKEY: tasks) — FINDING #7.
//!
//! `sweep::emit_cancelled_batch` appends Cancelled events for the confirmed ids
//! via a BARE `append_batch` with no in-lock legality precondition. `handle_sweep`
//! re-scans candidates before calling this, but there is a TOCTOU window: a task
//! that merges/Done-s between the dry-run scan and this append is unconditionally
//! flipped to Cancelled at replay (`apply_cancelled` does not re-guard, and
//! `Done → Cancelled` is otherwise illegal). The #1873 `append_done_if_legal`
//! pattern exists precisely to stop daemon writes clobbering terminal state, but
//! the sweep cancel path does not use an equivalent guard.
//!
//! Guard: `emit_cancelled_batch` must NOT use the unguarded `append_batch`; it
//! must route through a `*_checked` precondition (mirroring `append_done_if_legal`)
//! that re-confirms each task is still non-terminal. RED now (bare `append_batch`);
//! GREEN once the cancel path is gated.

use std::path::PathBuf;

fn read_sweep() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/tasks/sweep.rs");
    std::fs::read_to_string(&p).expect("read src/tasks/sweep.rs")
}

/// Isolate the `emit_cancelled_batch` body.
fn emit_cancelled_body(text: &str) -> String {
    let start = text
        .find("fn emit_cancelled_batch(")
        .expect("emit_cancelled_batch exists");
    let after = &text[start..];
    let end = after[1..]
        .find("\nfn ")
        .map(|e| start + 1 + e)
        .or_else(|| after[1..].find("\npub(super) fn ").map(|e| start + 1 + e))
        .or_else(|| after[1..].find("\npub fn ").map(|e| start + 1 + e))
        .unwrap_or(text.len());
    text[start..end].to_string()
}

#[test]
#[ignore = "tasks-sweep-cancel-no-guard: red until fix; remove #[ignore] after fix to confirm"]
fn sweep_cancel_uses_legality_guard_not_bare_append_tasks() {
    let text = read_sweep();
    let body = emit_cancelled_body(&text);

    // BAD: a bare, unguarded batch append for Cancelled events.
    let uses_bare_append = body.contains("append_batch(")
        && !body.contains("append_batch_checked")
        && !body.contains("append_cancelled_if_legal")
        && !body.contains("_checked_at");

    assert!(
        !uses_bare_append,
        "FINDING #7: emit_cancelled_batch appends Cancelled events via a BARE \
         `append_batch` with no in-lock legality precondition. A task that became \
         Done between the dry-run scan and this apply is silently reverted to \
         Cancelled. Gate the cancel under a `*_checked` precondition that re-confirms \
         each task is still non-terminal (mirror append_done_if_legal)."
    );
}
