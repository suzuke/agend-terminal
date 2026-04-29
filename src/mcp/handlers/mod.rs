//! MCP tool dispatch — handle_tool() routes tool calls to implementations.

mod channel;
mod ci;
mod comms;
mod instance;
mod schedule;
mod task;

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

/// Pure function: build tool_kill success response.
#[cfg(unix)]
pub(crate) fn build_tool_kill_result(target: &str, pgid: i32, reason: &str) -> Value {
    let mut result = json!({"ok": true, "target": target, "pgid": pgid});
    if !reason.is_empty() {
        result["reason"] = json!(reason);
    }
    result
}

/// Pure function: build tool_kill audit log detail string.
#[cfg(unix)]
pub(crate) fn build_tool_kill_audit(reason: &str, pgid: i32) -> String {
    format!("reason={reason}, pgid={pgid}")
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

    match tool {
        // --- Channel ---
        "reply" => channel::handle_reply(&home, args, instance_name),
        "react" => channel::handle_react(args, instance_name),
        "download_attachment" => channel::handle_download_attachment(&home, args, instance_name),

        // --- Cross-instance communication ---
        "send_to_instance" | "send" => comms::handle_send_to_instance(&home, args, tool, &sender),
        "delegate_task" => comms::handle_delegate_task(&home, args, &sender),
        "report_result" => comms::handle_report_result(&home, args, &sender),
        "request_information" => comms::handle_request_information(&home, args, &sender),
        "broadcast" => comms::handle_broadcast(&home, args, &sender),
        "inbox" => comms::handle_inbox(&home, instance_name),
        "describe_message" => comms::handle_describe_message(&home, args, instance_name),
        "describe_thread" => comms::handle_describe_thread(&home, args),

        // --- Instance management ---
        "list_instances" => instance::handle_list_instances(&home, instance_name),
        "create_instance" => instance::handle_create_instance(&home, args, instance_name),
        "delete_instance" => instance::handle_delete_instance(&home, args),
        "start_instance" => instance::handle_start_instance(&home, args),
        "describe_instance" => instance::handle_describe_instance(&home, args),
        "replace_instance" => instance::handle_replace_instance(&home, args),
        "set_display_name" => instance::handle_set_display_name(&home, args, instance_name),
        "set_description" => instance::handle_set_description(&home, args, instance_name),
        "interrupt" => instance::handle_interrupt(&home, args),
        "tool_kill" => instance::handle_tool_kill(&home, args),
        "set_waiting_on" => instance::handle_set_waiting_on(&home, args, instance_name, &sender),
        "move_pane" => instance::handle_move_pane(&home, args),
        "report_health" => instance::handle_report_health(&home, args, instance_name, &sender),
        "clear_blocked_reason" => instance::handle_clear_blocked_reason(&home, args),

        // --- Decisions ---
        "post_decision" => task::handle_post_decision(&home, args, instance_name, &sender),
        "list_decisions" => task::handle_list_decisions(&home, args),
        "update_decision" => task::handle_update_decision(&home, args, instance_name),

        // --- Task board ---
        "task" => task::handle_task(&home, args, instance_name),

        // --- Task sweep config ---
        "task_sweep_config" => task::handle_task_sweep_config(&home, args),

        // --- Teams ---
        "create_team" => task::handle_create_team(&home, args),
        "delete_team" => task::handle_delete_team(&home, args),
        "list_teams" => task::handle_list_teams(&home),
        "update_team" => task::handle_update_team(&home, args),

        // --- Scheduling ---
        "create_schedule" => schedule::handle_create_schedule(&home, args, instance_name),
        "list_schedules" => schedule::handle_list_schedules(&home, args),
        "update_schedule" => schedule::handle_update_schedule(&home, args),
        "delete_schedule" => schedule::handle_delete_schedule(&home, args),

        // --- Deployments ---
        "deploy_template" => schedule::handle_deploy_template(&home, args, instance_name),
        "teardown_deployment" => schedule::handle_teardown_deployment(&home, args),
        "list_deployments" => schedule::handle_list_deployments(&home),

        // --- Repo access ---
        "checkout_repo" => ci::handle_checkout_repo(&home, args, instance_name),
        "release_repo" => ci::handle_release_repo(args),

        // --- CI watch ---
        "watch_ci" => ci::handle_watch_ci(&home, args, instance_name),
        "unwatch_ci" => ci::handle_unwatch_ci(&home, args),

        _ => json!({"error": format!("unknown tool: {tool}")}),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
