//! MCP tool dispatch — handle_tool() routes tool calls to implementations.

mod binding_state;
mod channel;
pub(crate) mod ci;
mod comms;
pub(crate) mod comms_gates;
pub(crate) mod dispatch;
pub(crate) mod dispatch_hook;
mod force_release;
pub(crate) mod instance;
mod instance_metadata;
mod instance_queries;
pub(crate) mod instance_state;
mod restart;
mod schedule;
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

/// #2050 simplify PR-B (⑤): the uniform "require the `instance` arg" extraction
/// shared by the Variant-A MCP handlers (those for which `args["instance"]` is
/// mandatory). Returns the instance name, or the byte-identical
/// `{"error": "missing 'instance'"}` Value for the caller to return. Name-format
/// validation stays at each call site (`crate::validate_name_or_err!`), and the
/// `unwrap_or("")`/empty-string-guard handlers are intentionally NOT migrated.
///
/// Call sites use `let x = match super::require_instance(args) { Ok(t) => t,
/// Err(e) => return e };` (the `?` operator can't be used in a `-> Value` fn).
fn require_instance(args: &Value) -> Result<&str, Value> {
    match args["instance"].as_str() {
        Some(t) => Ok(t),
        None => Err(json!({"error": "missing 'instance'"})),
    }
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
    // arch F5 (t-…47102): a read-only tool (pure query, no state change) skips the
    // two per-call DISK side-effects below — the usage append and the heartbeat RMW
    // — so a polling agent doesn't pay disk on every `list_instances`/`binding_state`.
    // The in-mem heartbeat (the authoritative liveness signal) still fires; see below.
    let read_only = is_read_only_tool(tool);
    // #2055 step 1: instrument-only usage stats — record THAT this tool was
    // called + which optional params it carried, into <home>/mcp-usage-stats.jsonl.
    // Best-effort, zero behaviour change: it never touches `args` or the result
    // and swallows all errors. This is the single dispatch chokepoint, so every
    // (non-read-only) tool call is observed exactly once.
    if !read_only {
        crate::mcp::usage_stats::record(&home, tool, args);
    }
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
        // The in-mem pair heartbeat is the AUTHORITATIVE liveness source
        // (supervisor/router/dispatch_idle all read `heartbeat_pair`). It is cheap
        // (no disk), so it fires for EVERY tool call incl. read-only — a read-only
        // poll still proves the agent is alive.
        let cold_start =
            crate::daemon::heartbeat_pair::snapshot_for(instance_name).heartbeat_at_ms == 0;
        crate::daemon::heartbeat_pair::update_with(instance_name, |p| {
            p.heartbeat_at_ms = crate::daemon::heartbeat_pair::now_ms();
        });
        // Disk `last_heartbeat` RMW: skip for a read-only tool (the perf win) EXCEPT
        // on the cold→warm first call after a (re)start. The supervisor reads disk
        // `last_heartbeat` ONLY as the crash-recovery fallback while the in-mem pair
        // is still cold (`heartbeat_at_ms == 0`, supervisor.rs ~1248); refreshing it
        // on that one transition keeps the post-restart fallback fresh, so a
        // read-only-only agent can't be falsely stale-escalated. Once the pair is
        // warm the fallback is never consulted, so subsequent read-only polls skip
        // the disk write.
        if !read_only || cold_start {
            save_metadata(
                &home,
                instance_name,
                "last_heartbeat",
                json!(chrono::Utc::now().to_rfc3339()),
            );
        }
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

/// arch F5 (t-…47102): MCP tools that are PURE QUERIES — no state change, no
/// outbound effect. `handle_tool` skips its two per-call disk side-effects (the
/// usage append + the heartbeat RMW) for these so a polling agent doesn't pay
/// disk on every call; the in-mem heartbeat still fires, so liveness is preserved.
///
/// Conservative allowlist — only tools that NEVER mutate. Deliberately NOT here:
/// `inbox` (drain transitions unread→delivering), `download_attachment` (writes a
/// file), and every action-based tool (`task`/`decision`/`team`/`schedule`/
/// `deployment`/`ci`/`health`/`repo`/`mode`) whose read-vs-write splits by
/// `action` — those keep the full path rather than coupling this chokepoint to
/// each tool's per-action semantics (the minority of read actions pay the IO).
fn is_read_only_tool(tool: &str) -> bool {
    matches!(
        tool,
        "list_instances"
            | "binding_state"
            | "gc_dry_run"
            | "tokens"
            | "pane_snapshot"
            | "tui_screenshot"
    )
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod review_repro_mcp_core_surface;

// Re-homed here (not in ci/mod.rs / dispatch_hook/mod.rs) to keep those
// KNOWN_OVERSIZED handler files under their file_size_invariant ceilings —
// same precedent as instance_964_tests above. The tests reach the handlers'
// pub(crate) entry points via absolute `crate::` paths.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "ci/review_repro_mcp_ci_worktree.rs"]
mod review_repro_mcp_ci_worktree;

// no outer allow here: the file carries its own `#![allow(clippy::expect_used)]`
// and uses no `.unwrap()`, so an outer dup would trip `duplicated_attributes`.
#[cfg(test)]
#[path = "dispatch_hook/review_repro_mcp_dispatch_comms.rs"]
mod review_repro_mcp_dispatch_hook;
