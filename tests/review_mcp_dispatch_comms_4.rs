//! Verification/reproduction test for the `mcp-dispatch-comms` review batch,
//! finding 4 (low / resource-leak).
//!
//! A plain self-dispatch with a `branch` must not lease + bind a worktree
//! before the self-send is rejected (orphan worktree).
//!
//! W2.2: `handle_delegate_task` lives in `src/mcp/handlers/comms_delegate.rs`
//! (phase pipeline). The fix is an UNCONDITIONAL self-send rejection
//! (`*sender == target`) in the **resolve** phase, which always runs before
//! the **lease** phase (`dispatch_auto_bind_lease_with_source_and_chain`).
//!
//! Static-invariant method: pin that resolve-phase still has the unconditional
//! self-reject, and that the lease call still exists in this module (so the
//! fix surface is not accidentally deleted).

use std::path::PathBuf;

fn delegate_src() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/mcp/handlers/comms_delegate.rs");
    std::fs::read_to_string(&p).expect("read src/mcp/handlers/comms_delegate.rs")
}

/// Lines of `fn resolve_delegate` (where self-dispatch rejection lives).
fn resolve_phase_lines(src: &str) -> Vec<String> {
    let start = src
        .find("fn resolve_delegate")
        .expect("fn resolve_delegate not found in comms_delegate.rs");
    let region = &src[start..];
    // End at next top-level fn after resolve_delegate body.
    let after = region.find("\nfn ").unwrap_or(region.len());
    region[..after]
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|t| !t.starts_with("//") && !t.starts_with('*') && !t.is_empty())
        .collect()
}

#[test]
fn self_dispatch_rejected_before_auto_bind_lease_mcp_dispatch_comms() {
    let src = delegate_src();

    // Lease phase still present (the side-effect this pin protects against).
    assert!(
        src.contains("dispatch_auto_bind_lease_with_source_and_chain"),
        "comms_delegate.rs must still call dispatch_auto_bind_lease_with_source_and_chain \
         (lease phase); if lease moves, update this invariant to the new module"
    );

    // Resolve phase still runs first in the public choreography.
    let handle_pos = src
        .find("fn handle_delegate_task")
        .expect("fn handle_delegate_task not found");
    let resolve_call_pos = src[handle_pos..]
        .find("resolve_delegate(")
        .expect("handle_delegate_task must call resolve_delegate");
    let lease_call_in_handle = src[handle_pos..].find("maybe_auto_bind_lease(");
    assert!(
        lease_call_in_handle.is_some_and(|lp| resolve_call_pos < lp),
        "handle_delegate_task must call resolve_delegate before maybe_auto_bind_lease"
    );

    let lines = resolve_phase_lines(&src);
    assert!(!lines.is_empty(), "resolve_delegate body empty?");

    // UNCONDITIONAL self-send rejection in resolve (not only the team-orch loop guard).
    let has_unconditional_self_reject = lines
        .iter()
        .any(|l| l.contains("*sender == target") && !l.contains("raw_target != target"));

    assert!(
        has_unconditional_self_reject,
        "resolve_delegate must reject plain self-dispatch before lease phase \
         (orphan worktree). Expected unconditional `*sender == target` rejection \
         in src/mcp/handlers/comms_delegate.rs::resolve_delegate."
    );
}
