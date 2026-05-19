//! MCP tool dispatch — handle_tool() routes tool calls to implementations.

mod anti_stall;
mod binding_state;
mod channel;
pub(crate) mod ci;
mod comms;
mod dispatch;
pub(crate) mod dispatch_hook;
mod force_release;
pub(crate) mod instance;
pub(crate) mod instance_lifecycle;
mod instance_spawn;
mod restart;
mod schedule;
pub(crate) mod sha_gate;
mod task;
mod worktree;

/// Test-only thin shim into `release_worktree`'s production handler. Used by
/// `worktree_pool::tests::p0x_release_full_via_handle_release_worktree_end_to_end`
/// to exercise the full MCP dispatch path without setting up a sender.
#[cfg(test)]
pub(crate) fn worktree_test_release(home: &std::path::Path, args: &Value) -> Value {
    worktree::handle_release_worktree(home, args, &None)
}

use crate::agent_ops::save_metadata;
use crate::identity::Sender;
use serde_json::{json, Value};

// Re-exported for tests (via `use super::*`).
#[cfg(test)]
use crate::agent_ops::{cleanup_working_dir, merge_metadata};

/// True iff the MCP handler output should be treated as a success for
/// `FleetEvent` emission purposes. Handlers that wrap `send_to` return
/// `{"target": …}` (API path) or `{"target": …, "note": …}` (fallback
/// path) on success, and `{"error": …}` on failure; we mirror that
/// check here so a failed delegate_task / broadcast doesn't pollute
/// the fleet_binding with events that never actually left the daemon.
fn is_ok_result(value: &Value) -> bool {
    value.get("error").is_none()
}

/// Error payload for cross-instance tools invoked without a resolvable
/// `AGEND_INSTANCE_NAME`. Without this guard the message would land at the
/// receiver as `[from:]` with no originator.
fn err_needs_identity(tool: &str) -> Value {
    json!({
        "error": format!(
            "{tool} requires AGEND_INSTANCE_NAME to be set — cross-instance messaging needs a named sender"
        )
    })
}

/// Build the INJECT API params for an interrupt ESC byte injection.
/// Extracted for testability — unit tests verify the exact params
/// without needing a running daemon.
pub fn interrupt_esc_params(target: &str) -> Value {
    json!({
        "method": crate::api::method::INJECT,
        "params": {"name": target, "data": "\x1b", "raw": true}
    })
}

// Re-export for tests that use `use super::*`.
#[cfg(test)]
use instance::resolve_team_layout;

pub fn handle_tool(tool: &str, args: &Value, instance_name: &str) -> Value {
    let home = crate::home_dir();
    // Explicit arg beats env var. Cross-instance arms require `Some`;
    // anonymous/standalone arms tolerate the empty `&str` view.
    let sender: Option<Sender> = Sender::new(instance_name).or_else(Sender::from_env);
    let instance_name: &str = sender.as_ref().map(Sender::as_str).unwrap_or("");

    // Implicit heartbeat: any MCP tool call = agent is alive.
    // Sprint 23 P0 F6 fix: update in-memory pair lock atomically with the
    // disk write so supervisor's pair-read during stale-decay never sees
    // (heartbeat=fresh + waiting_on_since=stale) or vice versa. Lock
    // ordering: pair lock acquired BEFORE disk I/O, released AFTER (lock
    // is leaf-level per docs/DAEMON-LOCK-ORDERING.md).
    if !instance_name.is_empty() {
        crate::daemon::heartbeat_pair::update_with(instance_name, |p| {
            p.heartbeat_at_ms = crate::daemon::heartbeat_pair::now_ms();
        });
        save_metadata(
            &home,
            instance_name,
            "last_heartbeat",
            json!(chrono::Utc::now().to_rfc3339()),
        );
    }

    // #694 BLOCK 2 — dispatch table lookup. Returns `Some` for migrated
    // tools; falls through to the inline match below for un-migrated
    // arms. See `dispatch.rs` for the registered handler list and the
    // signature-bundling rationale.
    let dispatch_ctx = dispatch::HandlerCtx {
        home: &home,
        args,
        instance_name,
        sender: &sender,
    };
    if let Some(value) = dispatch::try_dispatch(tool, &dispatch_ctx) {
        return value;
    }

    // #694 BLOCK 2 fully migrated — all 30 advertised MCP tools flow
    // through `dispatch::try_dispatch` above. The inline match is gone;
    // a reached-this-line tool name must be unregistered. (Defensive
    // catch-all — `every_advertised_tool_is_routed_somewhere` in
    // dispatch.rs::tests pins that every tool in `tool_definitions()`
    // is routed by `dispatch.rs`.)
    json!({"error": format!("unknown tool: {tool}")})
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;

// Sprint 55 P0-B tests in sibling file (Sprint 54 PR #517 / Sprint 55 PR #522/#526 precedent).
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod p0b_tests;

// #964: MCP create_instance ordering regression tests live in sibling
// file to keep instance.rs under the 750-LOC file_size_invariant.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod instance_964_tests;
