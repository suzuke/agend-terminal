//! Review-repro guards (app-tui batch) attached to `src/app/mod.rs`.
//!
//! Two SOURCE INVARIANTS over the TUI event loop / scratch-shell liveness
//! helper. Both are runtime paths that can only be driven by standing up the
//! full crossterm event loop on a real terminal, so they are pinned as
//! body-scan invariants (the same technique `src/agent/tests.rs` uses for the
//! #1146 lock-ordering guards). Parent of this submodule file is src/app/, so
//! `include_str!("mod.rs")` is the source file under test.

/// `agent_is_alive`'s doc comment claims std::sync::Mutex poison semantics that
/// `parking_lot::Mutex` (the actual type of `AgentHandle.child`) cannot
/// provide: parking_lot never poisons — `.lock()` returns the guard directly,
/// not a `Result`, so there is no poison branch. The real failure mode is a
/// deadlock (a panic while holding the lock leaves it locked, and this
/// `handle.child.lock()` on the main loop would BLOCK). The misleading
/// "poisoned child mutex is treated as alive" sentence must be removed /
/// replaced (or the code must switch to `try_lock()` with the bogus claim
/// gone). Red while the sentence is present.
#[test]
#[ignore = "maintainability/agent_is_alive-poison-doc: red until fix; remove #[ignore] after fix to confirm"]
fn agent_is_alive_doc_drops_parking_lot_poison_claim_app_tui() {
    let src = include_str!("mod.rs");

    // Anchor the check to the helper so an unrelated future mention of
    // "poison" elsewhere can't mask a regression here.
    assert!(
        src.contains("fn agent_is_alive("),
        "slice sanity: agent_is_alive must exist in src/app/mod.rs"
    );

    assert!(
        !src.contains("poisoned child mutex is treated as alive"),
        "maintainability: `agent_is_alive`'s doc claims std::sync::Mutex \
         poison-safety, but `AgentHandle.child` is a parking_lot::Mutex which \
         NEVER poisons — there is no poison branch and none is possible. The \
         real risk is a deadlock (panic-while-locked wedges the lock and this \
         `handle.child.lock()` blocks the TUI main loop), not 'treat as alive'. \
         Remove/replace the poison sentence to reflect parking_lot semantics \
         (and, if poison-style resilience is wanted, use `try_lock()` treating \
         a contended `None` as alive)."
    );
}

/// The `Event::Resize` arm calls `resize_panes` inline but neither sets
/// `needs_resize = true` nor calls `terminal.clear()`. The dedicated
/// needs_resize path (top of the loop) deliberately runs `terminal.clear()`
/// after `resize_panes` to fix #1140 (ratatui's Buffer::diff can leave stale
/// wide-char spacer cells when wide chars become narrow across frames). A
/// terminal-driven resize reflows pane widths — the exact wide→narrow
/// transition #1140 targets — but bypasses the clear, so the ghost artifacts
/// reappear after an interactive resize. Red while the arm performs neither
/// the deferral nor the clear.
#[test]
#[ignore = "correctness/resize-arm-skips-ghost-clear: red until fix; remove #[ignore] after fix to confirm"]
fn terminal_resize_arm_performs_ghost_clear_app_tui() {
    let src = include_str!("mod.rs");

    let start = src
        .find("Event::Resize(cols, rows) => {")
        .expect("Event::Resize arm must exist in the TUI event loop");
    // The unique `recv(wakeup_rx)` crossbeam select branch sits just after the
    // event-match block and cleanly bounds the Resize arm body.
    let rel_end = src[start..]
        .find("recv(wakeup_rx)")
        .expect("recv(wakeup_rx) branch must follow and bound the Resize arm");
    let arm = &src[start..start + rel_end];

    // Sanity: the buggy arm still calls resize_panes inline; if not, drift.
    assert!(
        arm.contains("resize_panes"),
        "slice sanity: Resize arm should call resize_panes; boundaries may have \
         drifted — re-locate the arm"
    );

    let sets_needs_resize = arm.contains("needs_resize = true");
    let clears_terminal = arm.contains("terminal.clear");

    assert!(
        sets_needs_resize || clears_terminal,
        "correctness (#1140): the `Event::Resize` arm reflows pane widths but \
         skips the wide-char ghost-clear the needs_resize path performs, so \
         stale spacer cells reappear after an interactive terminal resize. \
         Either set `needs_resize = true` (let the top-of-loop block do the \
         `terminal.clear()`) or add an explicit `terminal.clear()` after the \
         inline `resize_panes`. Found needs_resize={sets_needs_resize}, \
         terminal.clear={clears_terminal} in the Resize arm."
    );
}
