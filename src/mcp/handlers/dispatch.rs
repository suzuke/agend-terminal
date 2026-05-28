//! MCP tool dispatch table — #694 BLOCK 2.
//!
//! `handle_tool` (in `mod.rs`) historically routed 30+ MCP tools through
//! a 143-line `match` literal. This module introduces a linear-scan
//! dispatch table so tools can register their handlers as data instead
//! of as match arms. Adding a tool becomes "append an entry"; un-
//! migrated tools fall through to the (shrinking) inline match.
//!
//! **Signature design** — the 30+ arms have at least four distinct
//! handler shapes (`(home, args, instance)`, `(home, args)`,
//! `(home, args, sender)`, `(home, args, instance, sender)`). Rather
//! than commit to one shape, this module uses a single uniform
//! [`HandlerFn`] keyed on a [`HandlerCtx`] struct that bundles every
//! common parameter. Each migrated tool gets a tiny adapter fn that
//! pulls the fields it needs out of `HandlerCtx`.
//!
//! **Linear scan** — `<10ns` for 30 entries vs `~50ns` allocator hit
//! for HashMap/phf, and the table size is bounded by the MCP tool
//! catalogue, so static-search is cheap and avoids the deps.
//!
//! **Fallback in mod.rs** — [`try_dispatch`] returns `Option<Value>`
//! (`None` = "tool name not in table"). `handle_tool` falls back to the
//! existing inline match for un-migrated arms; the catch-all
//! `unknown tool` branch in that match still handles fully-unknown
//! names.

use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

use super::{
    binding_state, channel, ci, comms, force_release, instance, restart, schedule, task, worktree,
};

/// Shared per-call context — every common parameter `handle_tool`
/// would otherwise pass into the match arms, bundled together so each
/// [`HandlerFn`] has a single uniform shape.
pub(super) struct HandlerCtx<'a> {
    pub home: &'a Path,
    pub args: &'a Value,
    pub instance_name: &'a str,
    pub sender: &'a Option<Sender>,
}

/// One MCP tool's dispatcher. Function pointer (not `Box<dyn …>`) so
/// the slice in [`registered_handlers`] is `const`-friendly and
/// allocation-free.
pub(super) type HandlerFn = fn(&HandlerCtx<'_>) -> Value;

pub(super) struct HandlerEntry {
    pub name: &'static str,
    pub handler: HandlerFn,
}

/// Look the `tool` name up in the dispatch table. Returns `Some(value)`
/// on hit; returns `None` if the tool isn't registered — the caller
/// falls back to the inline `match` in `mod.rs` for un-migrated arms.
pub(super) fn try_dispatch(tool: &str, ctx: &HandlerCtx<'_>) -> Option<Value> {
    for entry in registered_handlers() {
        if entry.name == tool {
            return Some((entry.handler)(ctx));
        }
    }
    None
}

// ---------------------------------------------------------------------
// Adapter generation macro — eliminates per-tool boilerplate.
//
// Each invocation generates:
//   fn dispatch_<ident>(ctx: &HandlerCtx<'_>) -> Value { <body> }
//
// Four shapes match the four handler signatures in the codebase:
//   (home, args, instance)        → shape: hai
//   (home, args)                  → shape: ha
//   (home, args, instance, sender)→ shape: hais
//   (home, args, sender)          → shape: has
//   (home)                        → shape: h
//   (args)                        → shape: a
//   custom body                   → shape: custom
// ---------------------------------------------------------------------
macro_rules! adapter {
    ($name:ident, hai, $handler:expr) => {
        fn $name(ctx: &HandlerCtx<'_>) -> Value {
            $handler(ctx.home, ctx.args, ctx.instance_name)
        }
    };
    ($name:ident, ha, $handler:expr) => {
        fn $name(ctx: &HandlerCtx<'_>) -> Value {
            $handler(ctx.home, ctx.args)
        }
    };
    ($name:ident, hais, $handler:expr) => {
        fn $name(ctx: &HandlerCtx<'_>) -> Value {
            $handler(ctx.home, ctx.args, ctx.instance_name, ctx.sender)
        }
    };
    ($name:ident, has, $handler:expr) => {
        fn $name(ctx: &HandlerCtx<'_>) -> Value {
            $handler(ctx.home, ctx.args, ctx.sender)
        }
    };
    ($name:ident, h, $handler:expr) => {
        fn $name(ctx: &HandlerCtx<'_>) -> Value {
            $handler(ctx.home)
        }
    };
    ($name:ident, a, $handler:expr) => {
        fn $name(ctx: &HandlerCtx<'_>) -> Value {
            $handler(ctx.args)
        }
    };
    (@call $ctx:ident, hai, $handler:expr) => {
        $handler($ctx.home, $ctx.args, $ctx.instance_name)
    };
    (@call $ctx:ident, ha, $handler:expr) => {
        $handler($ctx.home, $ctx.args)
    };
    (@call $ctx:ident, hais, $handler:expr) => {
        $handler($ctx.home, $ctx.args, $ctx.instance_name, $ctx.sender)
    };
    (@call $ctx:ident, has, $handler:expr) => {
        $handler($ctx.home, $ctx.args, $ctx.sender)
    };
    (@call $ctx:ident, h, $handler:expr) => {
        $handler($ctx.home)
    };
    (@call $ctx:ident, a, $handler:expr) => {
        $handler($ctx.args)
    };
}

macro_rules! action_adapter {
    ($name:ident, $tool_label:literal, [ $( $action:literal => $handler:expr , $shape:ident );+ $(;)? ]) => {
        fn $name(ctx: &HandlerCtx<'_>) -> Value {
            match ctx.args["action"].as_str().unwrap_or("") {
                $( $action => { adapter!(@call ctx, $shape, $handler) } )+
                other => json!({"error": format!(concat!("unknown ", $tool_label, " action: {}"), other)}),
            }
        }
    };
}

// ---------------------------------------------------------------------
// Flat adapters — one per simple (non-action-based) tool.
// ---------------------------------------------------------------------

adapter!(
    dispatch_list_instances,
    hai,
    instance::handle_list_instances
);
adapter!(
    dispatch_create_instance,
    hai,
    instance::handle_create_instance
);
adapter!(
    dispatch_set_description,
    hai,
    instance::handle_set_description
);
adapter!(dispatch_interrupt, ha, instance::handle_interrupt);
adapter!(
    dispatch_delete_instance,
    ha,
    instance::handle_delete_instance
);
adapter!(dispatch_start_instance, ha, instance::handle_start_instance);
adapter!(
    dispatch_replace_instance,
    ha,
    instance::handle_replace_instance
);
adapter!(dispatch_move_pane, ha, instance::handle_move_pane);
adapter!(
    dispatch_set_waiting_on,
    hais,
    instance::handle_set_waiting_on
);
adapter!(dispatch_send, has, comms::handle_unified_send);
adapter!(dispatch_bind_self, has, worktree::handle_bind_self);
adapter!(
    dispatch_binding_state,
    has,
    binding_state::handle_binding_state
);
adapter!(
    dispatch_release_worktree,
    has,
    worktree::handle_release_worktree
);
adapter!(
    dispatch_force_release_worktree,
    has,
    force_release::handle_force_release_worktree
);
adapter!(dispatch_gc_dry_run, has, worktree::handle_gc_dry_run);
adapter!(
    dispatch_download_attachment,
    hai,
    channel::handle_download_attachment
);
adapter!(dispatch_reply, hai, channel::handle_reply);
adapter!(
    dispatch_set_display_name,
    hai,
    instance::handle_set_display_name
);
adapter!(dispatch_pane_snapshot, ha, instance::handle_pane_snapshot);
adapter!(
    dispatch_task_sweep_config,
    ha,
    task::handle_task_sweep_config
);
adapter!(dispatch_restart_daemon, h, restart::handle_restart_daemon);

// ---------------------------------------------------------------------
// Action-based adapters — match on args["action"], route to per-action
// handler. Unknown actions produce tool-specific error JSON.
// ---------------------------------------------------------------------

adapter!(dispatch_task, hai, task::handle_task);

action_adapter!(dispatch_ci, "ci", [
    "watch"   => ci::handle_watch_ci,   hai;
    "unwatch" => ci::handle_unwatch_ci, ha;
    "status"  => ci::handle_status_ci,  hai;
]);

action_adapter!(dispatch_decision, "decision", [
    "post"   => task::handle_post_decision,   hais;
    "list"   => task::handle_list_decisions,   ha;
    "update" => task::handle_update_decision,  hai;
]);

action_adapter!(dispatch_deployment, "deployment", [
    "deploy"   => schedule::handle_deploy_template,      hai;
    "teardown" => schedule::handle_teardown_deployment,   ha;
    "list"     => schedule::handle_list_deployments,      h;
]);

action_adapter!(dispatch_health, "health", [
    "report" => instance::handle_report_health,         hais;
    "clear"  => instance::handle_clear_blocked_reason,  ha;
]);

action_adapter!(dispatch_repo, "repo", [
    "checkout"                => ci::handle_checkout_repo,              hai;
    "release"                 => ci::handle_release_repo,               a;
    "cleanup_init_commits"    => ci::handle_cleanup_init_commits,       hai;
    "cleanup_merged_branches" => ci::handle_cleanup_merged_branches,    hai;
    "merge"                   => ci::handle_merge_repo,                 hai;
]);

action_adapter!(dispatch_schedule, "schedule", [
    "create" => schedule::handle_create_schedule,  hai;
    "list"   => schedule::handle_list_schedules,   ha;
    "update" => schedule::handle_update_schedule,  ha;
    "delete" => schedule::handle_delete_schedule,  ha;
]);

action_adapter!(dispatch_team, "team", [
    "create" => task::handle_create_team,  ha;
    "delete" => task::handle_delete_team,  ha;
    "list"   => task::handle_list_teams,   h;
    "update" => task::handle_update_team,  ha;
]);

// `inbox` — three-way branch on arg presence (NOT `args["action"]`):
//   - `message_id` present → describe single message
//   - else `thread_id` present → describe thread
//   - else → drain pending
fn dispatch_inbox(ctx: &HandlerCtx<'_>) -> Value {
    if ctx
        .args
        .get("message_id")
        .and_then(|v| v.as_str())
        .is_some()
    {
        comms::handle_describe_message(ctx.home, ctx.args, ctx.instance_name)
    } else if ctx.args.get("thread_id").and_then(|v| v.as_str()).is_some() {
        comms::handle_describe_thread(ctx.home, ctx.args)
    } else {
        comms::handle_inbox(ctx.home, ctx.instance_name)
    }
}

fn dispatch_tui_screenshot(ctx: &HandlerCtx<'_>) -> Value {
    match crate::api::call(
        ctx.home,
        &serde_json::json!({"method": crate::api::method::TUI_SCREENSHOT, "params": {}}),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            serde_json::json!({"svg": resp["svg"]})
        }
        Ok(resp) => {
            serde_json::json!({"error": resp["error"].as_str().unwrap_or("tui_screenshot failed")})
        }
        Err(e) => serde_json::json!({"error": format!("tui_screenshot: {e}")}),
    }
}

// `watchdog` — actions with inline business logic (not just forwarding).
fn dispatch_watchdog(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "snooze" => dispatch_watchdog_snooze(ctx),
        "resume" => dispatch_watchdog_resume(ctx),
        "status" => dispatch_watchdog_status(ctx),
        "ack" => dispatch_watchdog_ack(ctx),
        other => json!({"error": format!("unknown watchdog action: {other}")}),
    }
}

fn dispatch_watchdog_snooze(ctx: &HandlerCtx<'_>) -> Value {
    use crate::daemon::idle_watchdog;

    const MAX_SNOOZE_SECS: i64 = 4 * 3600;

    let duration_str = ctx.args["duration"].as_str().unwrap_or("1h");
    let secs = match parse_duration_secs(duration_str) {
        Some(s) => s.min(MAX_SNOOZE_SECS),
        None => return json!({"error": format!("invalid duration: {duration_str}")}),
    };
    let until = chrono::Utc::now() + chrono::Duration::seconds(secs);
    let actor = ctx.instance_name;
    match idle_watchdog::snooze_fleet_idle(ctx.home, until, actor) {
        Ok(snooze) => {
            crate::event_log::log(
                ctx.home,
                "watchdog_snooze",
                actor,
                &format!(
                    "fleet idle snoozed until {} ({duration_str})",
                    snooze.snoozed_until
                ),
            );
            json!({
                "snoozed": true,
                "snoozed_until": snooze.snoozed_until,
                "duration_secs": secs,
            })
        }
        Err(e) => json!({"error": format!("snooze failed: {e}")}),
    }
}

fn dispatch_watchdog_resume(ctx: &HandlerCtx<'_>) -> Value {
    use crate::daemon::idle_watchdog;
    idle_watchdog::resume_fleet_idle(ctx.home);
    crate::event_log::log(
        ctx.home,
        "watchdog_resume",
        ctx.instance_name,
        "fleet idle snooze cleared",
    );
    json!({"snoozed": false})
}

fn dispatch_watchdog_status(ctx: &HandlerCtx<'_>) -> Value {
    use crate::daemon::idle_watchdog;
    if let Some(snooze) = idle_watchdog::get_snooze_state(ctx.home) {
        let remaining = chrono::DateTime::parse_from_rfc3339(&snooze.snoozed_until)
            .ok()
            .map(|dt| {
                dt.with_timezone(&chrono::Utc)
                    .signed_duration_since(chrono::Utc::now())
                    .num_seconds()
                    .max(0)
            })
            .unwrap_or(0);
        json!({
            "snoozed": true,
            "snoozed_until": snooze.snoozed_until,
            "remaining_secs": remaining,
            "actor": snooze.actor,
        })
    } else {
        let ack_info = idle_watchdog::fleet_ack_status().map(|ts| json!({"acked_at": ts}));
        json!({"snoozed": false, "ack": ack_info})
    }
}

fn dispatch_watchdog_ack(ctx: &HandlerCtx<'_>) -> Value {
    use crate::daemon::idle_watchdog;
    let ts = idle_watchdog::ack_fleet_idle();
    let actor = ctx.instance_name;
    crate::event_log::log(
        ctx.home,
        "watchdog_ack",
        actor,
        "fleet idle acked — suppressed until post-ack activity",
    );
    json!({
        "acked": true,
        "acked_at": ts,
    })
}

fn dispatch_config(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "get" => {
            let key = ctx.args["key"].as_str().unwrap_or("");
            if key.is_empty() {
                return json!({"error": "key is required for get"});
            }
            match crate::runtime_config::get_key(key) {
                Ok(v) => json!({"key": key, "value": v}),
                Err(e) => json!({"error": e}),
            }
        }
        "set" => {
            let key = ctx.args["key"].as_str().unwrap_or("");
            let value = ctx.args["value"].as_str().unwrap_or("");
            if key.is_empty() || value.is_empty() {
                return json!({"error": "key and value are required for set"});
            }
            match crate::runtime_config::set(ctx.home, key, value) {
                Ok(_) => json!({"ok": true, "key": key, "value": value}),
                Err(e) => json!({"error": e}),
            }
        }
        "list" => json!({"config": crate::runtime_config::list()}),
        other => json!({"error": format!("unknown config action: {other}")}),
    }
}

/// Parse human-friendly duration strings like "2h", "30m", "1h30m".
/// A bare number without suffix is interpreted as **minutes**.
fn parse_duration_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total: i64 = 0;
    let mut num_buf = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num_buf.push(ch);
        } else {
            let n: i64 = num_buf.parse().ok()?;
            num_buf.clear();
            match ch {
                'h' => total += n * 3600,
                'm' => total += n * 60,
                's' => total += n,
                _ => return None,
            }
        }
    }
    if !num_buf.is_empty() {
        let n: i64 = num_buf.parse().ok()?;
        total += n * 60; // bare number = minutes
    }
    if total > 0 {
        Some(total)
    } else {
        None
    }
}

// ---------------------------------------------------------------------
// Registration table
// ---------------------------------------------------------------------

static REGISTERED: &[HandlerEntry] = &[
    HandlerEntry {
        name: "list_instances",
        handler: dispatch_list_instances,
    },
    HandlerEntry {
        name: "create_instance",
        handler: dispatch_create_instance,
    },
    HandlerEntry {
        name: "set_description",
        handler: dispatch_set_description,
    },
    HandlerEntry {
        name: "interrupt",
        handler: dispatch_interrupt,
    },
    HandlerEntry {
        name: "delete_instance",
        handler: dispatch_delete_instance,
    },
    HandlerEntry {
        name: "start_instance",
        handler: dispatch_start_instance,
    },
    HandlerEntry {
        name: "replace_instance",
        handler: dispatch_replace_instance,
    },
    HandlerEntry {
        name: "move_pane",
        handler: dispatch_move_pane,
    },
    HandlerEntry {
        name: "set_waiting_on",
        handler: dispatch_set_waiting_on,
    },
    HandlerEntry {
        name: "send",
        handler: dispatch_send,
    },
    HandlerEntry {
        name: "bind_self",
        handler: dispatch_bind_self,
    },
    HandlerEntry {
        name: "binding_state",
        handler: dispatch_binding_state,
    },
    HandlerEntry {
        name: "release_worktree",
        handler: dispatch_release_worktree,
    },
    HandlerEntry {
        name: "force_release_worktree",
        handler: dispatch_force_release_worktree,
    },
    HandlerEntry {
        name: "gc_dry_run",
        handler: dispatch_gc_dry_run,
    },
    HandlerEntry {
        name: "task",
        handler: dispatch_task,
    },
    HandlerEntry {
        name: "ci",
        handler: dispatch_ci,
    },
    HandlerEntry {
        name: "decision",
        handler: dispatch_decision,
    },
    HandlerEntry {
        name: "deployment",
        handler: dispatch_deployment,
    },
    HandlerEntry {
        name: "health",
        handler: dispatch_health,
    },
    HandlerEntry {
        name: "watchdog",
        handler: dispatch_watchdog,
    },
    HandlerEntry {
        name: "config",
        handler: dispatch_config,
    },
    HandlerEntry {
        name: "repo",
        handler: dispatch_repo,
    },
    HandlerEntry {
        name: "schedule",
        handler: dispatch_schedule,
    },
    HandlerEntry {
        name: "team",
        handler: dispatch_team,
    },
    HandlerEntry {
        name: "download_attachment",
        handler: dispatch_download_attachment,
    },
    HandlerEntry {
        name: "inbox",
        handler: dispatch_inbox,
    },
    HandlerEntry {
        name: "reply",
        handler: dispatch_reply,
    },
    HandlerEntry {
        name: "set_display_name",
        handler: dispatch_set_display_name,
    },
    HandlerEntry {
        name: "pane_snapshot",
        handler: dispatch_pane_snapshot,
    },
    HandlerEntry {
        name: "tui_screenshot",
        handler: dispatch_tui_screenshot,
    },
    HandlerEntry {
        name: "task_sweep_config",
        handler: dispatch_task_sweep_config,
    },
    HandlerEntry {
        name: "restart_daemon",
        handler: dispatch_restart_daemon,
    },
];

pub(super) fn registered_handlers() -> &'static [HandlerEntry] {
    REGISTERED
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx_for<'a>(home: &'a Path, args: &'a Value, instance: &'a str) -> HandlerCtx<'a> {
        static EMPTY_SENDER: Option<Sender> = None;
        HandlerCtx {
            home,
            args,
            instance_name: instance,
            sender: &EMPTY_SENDER,
        }
    }

    #[test]
    fn try_dispatch_returns_none_for_unregistered_tool() {
        let home = std::env::temp_dir();
        let args = json!({});
        let ctx = ctx_for(&home, &args, "");
        assert!(try_dispatch("definitely_not_a_real_tool", &ctx).is_none());
    }

    #[test]
    fn try_dispatch_returns_some_for_registered_tool() {
        let home = std::env::temp_dir();
        let args = json!({});
        let ctx = ctx_for(&home, &args, "");
        assert!(try_dispatch("list_instances", &ctx).is_some());
    }

    #[test]
    fn registered_handler_names_pin() {
        let names: Vec<&'static str> = registered_handlers().iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            vec![
                "list_instances",
                "create_instance",
                "set_description",
                "interrupt",
                "delete_instance",
                "start_instance",
                "replace_instance",
                "move_pane",
                "set_waiting_on",
                "send",
                "bind_self",
                "binding_state",
                "release_worktree",
                "force_release_worktree",
                "gc_dry_run",
                "task",
                "ci",
                "decision",
                "deployment",
                "health",
                "watchdog",
                "config",
                "repo",
                "schedule",
                "team",
                "download_attachment",
                "inbox",
                "reply",
                "set_display_name",
                "pane_snapshot",
                "tui_screenshot",
                "task_sweep_config",
                "restart_daemon",
            ]
        );
        assert_eq!(registered_handlers().len(), 33);
    }

    #[test]
    fn every_advertised_tool_is_routed_somewhere() {
        let defs = crate::mcp::tools::tool_definitions();
        let arr = defs
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tool_definitions() should return {tools: [...]}");
        let names: Vec<&str> = arr
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        let mod_rs = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/mcp/handlers/mod.rs"
        ))
        .expect("read mod.rs");
        let dispatch_rs = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/mcp/handlers/dispatch.rs"
        ))
        .expect("read dispatch.rs");
        let mut missing: Vec<&str> = Vec::new();
        for name in &names {
            let quoted = format!("\"{name}\"");
            if !mod_rs.contains(&quoted) && !dispatch_rs.contains(&quoted) {
                missing.push(name);
            }
        }
        assert!(
            missing.is_empty(),
            "tools advertised by tool_definitions() but not routed in mod.rs or dispatch.rs: {missing:?}"
        );
    }

    #[test]
    fn try_dispatch_routes_known_action_through_base_handler() {
        let home = std::env::temp_dir();
        let cases: &[(&str, &[&str])] = &[
            (
                "task",
                &[
                    "create", "list", "claim", "update", "done", "sweep", "health", "activity",
                ],
            ),
            ("ci", &["watch", "unwatch", "status"]),
            ("decision", &["post", "list", "update"]),
            ("deployment", &["deploy", "teardown", "list"]),
            ("health", &["report", "clear"]),
            (
                "repo",
                &[
                    "checkout",
                    "release",
                    "cleanup_init_commits",
                    "cleanup_merged_branches",
                ],
            ),
            ("schedule", &["create", "list", "update", "delete"]),
            ("team", &["create", "delete", "list", "update"]),
            ("watchdog", &["snooze", "resume", "status", "ack"]),
            ("config", &["get", "set", "list"]),
        ];
        for (tool, actions) in cases {
            for action in actions.iter() {
                let args = json!({ "action": action });
                let ctx = ctx_for(&home, &args, "");
                assert!(
                    try_dispatch(tool, &ctx).is_some(),
                    "tool='{tool}' action='{action}' did not route through dispatch table"
                );
            }
        }
    }

    #[test]
    fn try_dispatch_unknown_action_falls_through_to_error() {
        let home = std::env::temp_dir();
        let args = json!({"action": "frobnicate-not-a-real-action"});
        let ctx = ctx_for(&home, &args, "");
        let result = try_dispatch("task", &ctx);
        assert!(result.is_some(), "base handler must still return Some");
        let v = result.unwrap();
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(
            err.contains("unknown") || err.contains("action"),
            "expected unknown-action error from base; got: {v:?}"
        );
    }

    #[test]
    fn try_dispatch_missing_action_falls_through_to_base() {
        let home = std::env::temp_dir();
        let args = json!({}); // no "action" key
        let ctx = ctx_for(&home, &args, "");
        assert!(try_dispatch("task", &ctx).is_some());
    }

    // ── #1084 watchdog snooze MCP tests ──────────────────────────

    fn watchdog_home(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agend-watchdog-mcp-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn watchdog_snooze_then_status_round_trip() {
        let home = watchdog_home("snooze-status");
        let args = json!({"action": "snooze", "duration": "1h"});
        let ctx = ctx_for(&home, &args, "test-agent");
        let result = try_dispatch("watchdog", &ctx).unwrap();
        assert_eq!(result["snoozed"], true);
        assert!(result["snoozed_until"].is_string());
        assert_eq!(result["duration_secs"], 3600);

        let status_args = json!({"action": "status"});
        let status_ctx = ctx_for(&home, &status_args, "test-agent");
        let status = try_dispatch("watchdog", &status_ctx).unwrap();
        assert_eq!(status["snoozed"], true);
        assert!(status["remaining_secs"].as_i64().unwrap() > 0);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn watchdog_snooze_duration_clamped_to_4h() {
        let home = watchdog_home("snooze-clamp");
        let args = json!({"action": "snooze", "duration": "24h"});
        let ctx = ctx_for(&home, &args, "test-agent");
        let result = try_dispatch("watchdog", &ctx).unwrap();
        assert_eq!(
            result["duration_secs"],
            4 * 3600,
            "#1084: 24h must clamp to 4h"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn watchdog_resume_clears_snooze() {
        let home = watchdog_home("resume");
        let snooze_args = json!({"action": "snooze", "duration": "2h"});
        let ctx = ctx_for(&home, &snooze_args, "test-agent");
        try_dispatch("watchdog", &ctx);

        let resume_args = json!({"action": "resume"});
        let resume_ctx = ctx_for(&home, &resume_args, "test-agent");
        let result = try_dispatch("watchdog", &resume_ctx).unwrap();
        assert_eq!(result["snoozed"], false);

        let status_args = json!({"action": "status"});
        let status_ctx = ctx_for(&home, &status_args, "test-agent");
        let status = try_dispatch("watchdog", &status_ctx).unwrap();
        assert_eq!(status["snoozed"], false);
        std::fs::remove_dir_all(&home).ok();
    }
}
