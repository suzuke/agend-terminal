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

    match tool {
        // --- Channel ---
        "reply" => channel::handle_reply(&home, args, instance_name),
        "download_attachment" => channel::handle_download_attachment(&home, args, instance_name),

        // --- Cross-instance communication ---
        // NOTE: `send` migrated to dispatch table (#694 BLOCK 2 T-B8).
        "inbox" => {
            if args.get("message_id").and_then(|v| v.as_str()).is_some() {
                comms::handle_describe_message(&home, args, instance_name)
            } else if args.get("thread_id").and_then(|v| v.as_str()).is_some() {
                comms::handle_describe_thread(&home, args)
            } else {
                comms::handle_inbox(&home, instance_name)
            }
        }

        // --- Instance management ---
        // NOTE: 8 instance-lifecycle arms migrated to dispatch table
        // (#694 BLOCK 2 T-B7): list_instances, create_instance,
        // delete_instance, start_instance, replace_instance,
        // set_description, interrupt, move_pane. T-B8 adds
        // `set_waiting_on`. See dispatch.rs. Remaining action-based +
        // channel-heavy arms stay inline for T-B9+.
        "set_display_name" => instance::handle_set_display_name(&home, args, instance_name),
        "pane_snapshot" => instance::handle_pane_snapshot(&home, args),
        // Consolidated: health action=report/clear
        "health" => match args["action"].as_str().unwrap_or("") {
            "report" => instance::handle_report_health(&home, args, instance_name, &sender),
            "clear" => instance::handle_clear_blocked_reason(&home, args),
            other => json!({"error": format!("unknown health action: {other}")}),
        },

        // --- Decisions (consolidated: decision action=post/list/update) ---
        "decision" => match args["action"].as_str().unwrap_or("") {
            "post" => task::handle_post_decision(&home, args, instance_name, &sender),
            "list" => task::handle_list_decisions(&home, args),
            "update" => task::handle_update_decision(&home, args, instance_name),
            other => json!({"error": format!("unknown decision action: {other}")}),
        },

        // --- Task board ---
        // NOTE: `task` migrated to dispatch table (#694 BLOCK 2 T-B8),
        // including a sub-routing table for the `action` parameter.
        // T-B8 wires `action=list` to a sub-handler; the other four
        // actions (create/claim/update/done) fall through to the base
        // handler `dispatch_task` which forwards to `task::handle_task`,
        // preserving pre-migration behavior. T-B9+ wires the rest.

        // --- Task sweep config ---
        "task_sweep_config" => task::handle_task_sweep_config(&home, args),

        // --- Teams (consolidated: team action=create/delete/list/update) ---
        "team" => match args["action"].as_str().unwrap_or("") {
            "create" => task::handle_create_team(&home, args),
            "delete" => task::handle_delete_team(&home, args),
            "list" => task::handle_list_teams(&home),
            "update" => task::handle_update_team(&home, args),
            other => json!({"error": format!("unknown team action: {other}")}),
        },

        // --- Scheduling (consolidated: schedule action=create/list/update/delete) ---
        "schedule" => match args["action"].as_str().unwrap_or("") {
            "create" => schedule::handle_create_schedule(&home, args, instance_name),
            "list" => schedule::handle_list_schedules(&home, args),
            "update" => schedule::handle_update_schedule(&home, args),
            "delete" => schedule::handle_delete_schedule(&home, args),
            other => json!({"error": format!("unknown schedule action: {other}")}),
        },

        // --- Deployments (consolidated: deployment action=deploy/teardown/list) ---
        "deployment" => match args["action"].as_str().unwrap_or("") {
            "deploy" => schedule::handle_deploy_template(&home, args, instance_name),
            "teardown" => schedule::handle_teardown_deployment(&home, args),
            "list" => schedule::handle_list_deployments(&home),
            other => json!({"error": format!("unknown deployment action: {other}")}),
        },

        // --- Repo access (consolidated: repo action=checkout/release) ---
        "repo" => match args["action"].as_str().unwrap_or("") {
            "checkout" => ci::handle_checkout_repo(&home, args, instance_name),
            "release" => ci::handle_release_repo(args),
            other => json!({"error": format!("unknown repo action: {other}")}),
        },

        // --- CI watch (consolidated: ci action=watch/unwatch) ---
        "ci" => match args["action"].as_str().unwrap_or("") {
            "watch" => ci::handle_watch_ci(&home, args, instance_name),
            "unwatch" => ci::handle_unwatch_ci(&home, args),
            // Sprint 54 P0-5 (sub-scope C): aggregate health snapshot.
            "status" => ci::handle_status_ci(&home, args, instance_name),
            other => json!({"error": format!("unknown ci action: {other}")}),
        },

        // NOTE: bind_self / release_worktree / force_release_worktree /
        // binding_state / gc_dry_run migrated to dispatch table (#694
        // BLOCK 2 T-B8). See dispatch.rs `dispatch_<name>` adapters.

        // --- Sprint 60 W1 PR-3 (#P0-3): operator restart MCP tool ---
        "restart_daemon" => restart::handle_restart_daemon(&home),

        _ => json!({"error": format!("unknown tool: {tool}")}),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;

// Sprint 55 P0-B tests in sibling file (Sprint 54 PR #517 / Sprint 55 PR #522/#526 precedent).
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod p0b_tests;
