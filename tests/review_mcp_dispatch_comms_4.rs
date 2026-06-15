//! Verification/reproduction test for the `mcp-dispatch-comms` review batch,
//! finding 4 (low / resource-leak).
//!
//! A plain self-dispatch with a `branch` leases + binds a worktree BEFORE
//! the API rejects the self-send, orphaning the worktree.
//!
//! `handle_delegate_task` (src/mcp/handlers/comms.rs) rejects a self-send
//! only when team-orchestrator resolution actually CHANGED the target:
//!   `if *sender == target && raw_target != target { return ...self-loop... }`
//! A plain self-dispatch where `raw_target == resolved_target == sender`
//! skips that guard, falls through to `dispatch_auto_bind_lease_with_chain`
//! (which creates a worktree + writes a binding), and only THEN reaches the
//! API SEND which rejects the self-send — leaving a leased worktree +
//! binding for a dispatch that never delivered, with no rollback on this
//! path.
//!
//! Static-invariant method (source scan): the runtime leak cannot be
//! driven without registering instances + a real API/daemon. Instead we
//! pin the FIX: an UNCONDITIONAL self-send rejection (`*sender == target`,
//! regardless of whether resolution changed the target) must appear in
//! `handle_delegate_task` BEFORE the `dispatch_auto_bind_lease_with_chain`
//! call — so no worktree is leased for a dispatch the API will reject.
//!
//! RED now: the only `*sender == target` check before the auto-bind is the
//! qualified `*sender == target && raw_target != target` guard, so a plain
//! self-dispatch reaches the lease. No unconditional self-send rejection
//! precedes the auto-bind.
//!
//! GREEN after fix: moving an unconditional `*sender == target` rejection
//! ahead of the auto-bind makes this scan find it.

use std::path::PathBuf;

fn comms_src() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/mcp/handlers/comms.rs");
    std::fs::read_to_string(&p).expect("read src/mcp/handlers/comms.rs")
}

/// Return the lines of `fn handle_delegate_task` up to (and excluding) the
/// first `dispatch_auto_bind_lease_with_chain(` call — i.e. everything that
/// runs BEFORE the auto-bind/lease side effect. Comment/blank lines stripped.
fn pre_auto_bind_lines() -> Vec<String> {
    let src = comms_src();
    let fn_start = src
        .find("fn handle_delegate_task")
        .expect("fn handle_delegate_task not found");
    let region = &src[fn_start..];
    let auto_bind = region
        .find("dispatch_auto_bind_lease_with_chain(")
        .expect("dispatch_auto_bind_lease_with_chain call not found in handle_delegate_task");
    region[..auto_bind]
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|t| !t.starts_with("//") && !t.starts_with('*') && !t.is_empty())
        .collect()
}

#[test]
fn self_dispatch_rejected_before_auto_bind_lease_mcp_dispatch_comms() {
    let lines = pre_auto_bind_lines();

    // Sanity: the region must actually contain the auto-bind-preceding code.
    assert!(
        !lines.is_empty(),
        "could not isolate the pre-auto-bind region of handle_delegate_task"
    );

    // An UNCONDITIONAL self-send rejection: a `*sender == target` check that
    // is NOT gated behind `raw_target != target`. The current code's only
    // self-target check before the lease is the qualified team-orchestrator
    // loop guard, which lets a plain self-dispatch slip through to the lease.
    let has_unconditional_self_reject = lines
        .iter()
        .any(|l| l.contains("*sender == target") && !l.contains("raw_target != target"));

    assert!(
        has_unconditional_self_reject,
        "handle_delegate_task leases + binds a worktree for a plain self-dispatch before the API \
         rejects the self-send (orphan worktree). The only `*sender == target` check before \
         `dispatch_auto_bind_lease_with_chain` is gated behind `raw_target != target`, so a plain \
         self-dispatch (raw_target == resolved == sender) is NOT rejected pre-lease. Move an \
         unconditional `*sender == target` rejection ahead of the auto-bind."
    );
}
