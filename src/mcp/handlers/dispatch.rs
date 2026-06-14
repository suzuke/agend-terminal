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
pub(crate) struct HandlerCtx<'a> {
    pub home: &'a Path,
    pub args: &'a Value,
    pub instance_name: &'a str,
    pub sender: &'a Option<Sender>,
}

/// One MCP tool's dispatcher. Function pointer (not `Box<dyn …>`) so
/// the slice in [`registered_handlers`] is `const`-friendly and
/// allocation-free.
pub(crate) type HandlerFn = fn(&HandlerCtx<'_>) -> Value;

/// #1602: validate `args` against the tool's declared `inputSchema` before
/// dispatch. The schemas declare `required[]` but nothing enforced it, so a
/// mis-named / omitted param failed LATE and misleadingly (the operator's
/// `reply` bug: a wrong key silently became an empty `text`). Now:
///
/// - a missing REQUIRED key → hard-reject with `<tool>: missing required
///   parameter '<name>'`;
/// - an UNKNOWN key → warn only (forward-compat; never reject).
///
/// Returns `Some(error_value)` to short-circuit dispatch, or `None` to proceed.
///
/// **Enforceability invariant (#1602/#1603):** every field a tool declares in
/// `required[]` MUST be one the handler genuinely needs — i.e. the handler
/// ERRORS (not defaults) when it's absent. The systematic audit found 5 tools
/// whose handler instead DEFAULTS a "required" field, so their schemas were
/// LYING. They were schema-aligned (field removed from `required[]`) rather than
/// allowlisted, keeping this validator a plain hard-reject. The 5:
/// `mode` (`action` → `"get"`), `create_instance` (`name` auto-derived in team
/// mode; single path still errors "missing 'name'"), `set_waiting_on`
/// (`condition` absent = clear), and `set_display_name` / `set_description`
/// (handler tolerates absent, sets `""` — a separate follow-up decides whether
/// set-X-without-X should hard-error).
///
/// Tools whose handler DOES error on a missing field (`reply.message`,
/// `send.message`, `delete_instance.instance`, action-based `task`/`decision`)
/// keep their `required[]` and are hard-rejected here. A new tool that declares
/// a field its handler defaults will trip the pinning test below.
fn validate_args(tool: &str, def: &Value, args: &Value) -> Option<Value> {
    let schema = &def["inputSchema"];
    if let Some(required) = schema["required"].as_array() {
        for req in required.iter().filter_map(Value::as_str) {
            if args.get(req).is_none() {
                return Some(json!({
                    "error": format!("{tool}: missing required parameter '{req}'")
                }));
            }
        }
    }
    if let (Some(props), Some(obj)) = (schema["properties"].as_object(), args.as_object()) {
        for key in obj.keys() {
            if !props.contains_key(key) {
                tracing::warn!(
                    tool, param = %key,
                    "#1602: unknown parameter (ignored) — not in the tool's inputSchema"
                );
            }
        }
    }
    None
}

/// Look the `tool` name up in the registry. Returns `Some(value)`
/// on hit; returns `None` if the tool isn't registered — the caller
/// falls back to the inline `match` in `mod.rs` for un-migrated arms.
pub(super) fn try_dispatch(tool: &str, ctx: &HandlerCtx<'_>) -> Option<Value> {
    crate::mcp::registry::all()
        .iter()
        .find(|entry| entry.name == tool)
        .map(|entry| {
            // #1602: enforce the declared inputSchema at the single dispatch
            // chokepoint — a missing required param is rejected with a clear
            // named error instead of failing late inside the handler.
            if let Some(err) = validate_args(entry.name, &(entry.definition)(), ctx.args) {
                return err;
            }
            (entry.handler)(ctx)
        })
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
        pub(crate) fn $name(ctx: &HandlerCtx<'_>) -> Value {
            $handler(ctx.home, ctx.args, ctx.instance_name)
        }
    };
    ($name:ident, ha, $handler:expr) => {
        pub(crate) fn $name(ctx: &HandlerCtx<'_>) -> Value {
            $handler(ctx.home, ctx.args)
        }
    };
    ($name:ident, hais, $handler:expr) => {
        pub(crate) fn $name(ctx: &HandlerCtx<'_>) -> Value {
            $handler(ctx.home, ctx.args, ctx.instance_name, ctx.sender)
        }
    };
    ($name:ident, has, $handler:expr) => {
        pub(crate) fn $name(ctx: &HandlerCtx<'_>) -> Value {
            $handler(ctx.home, ctx.args, ctx.sender)
        }
    };
    ($name:ident, h, $handler:expr) => {
        pub(crate) fn $name(ctx: &HandlerCtx<'_>) -> Value {
            $handler(ctx.home)
        }
    };
    ($name:ident, a, $handler:expr) => {
        pub(crate) fn $name(ctx: &HandlerCtx<'_>) -> Value {
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
        pub(crate) fn $name(ctx: &HandlerCtx<'_>) -> Value {
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
adapter!(dispatch_tokens, ha, crate::token_cost::handle_tokens);
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
adapter!(
    dispatch_restart_instance,
    ha,
    instance::handle_restart_instance
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
pub(crate) fn dispatch_inbox(ctx: &HandlerCtx<'_>) -> Value {
    if ctx.args.get("action").and_then(|v| v.as_str()) == Some("clear") {
        // #inbox-gc part a: quiet compact-clear (explicit action — never the
        // no-arg drain). Obligations stay unread; returns bounded summaries.
        comms::handle_inbox_clear(ctx.home, ctx.instance_name)
    } else if ctx
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

pub(crate) fn dispatch_tui_screenshot(ctx: &HandlerCtx<'_>) -> Value {
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
pub(crate) fn dispatch_watchdog(ctx: &HandlerCtx<'_>) -> Value {
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

pub(crate) fn dispatch_config(ctx: &HandlerCtx<'_>) -> Value {
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

/// #1339: read the operator-mode (GET-ONLY for agents). `mode get` → current
/// mode + delegate. SETTING the mode is operator-only and lives on the operator
/// transport (`agend-terminal mode <active|away|sleep>` CLI → the direct `MODE`
/// API method); the ingress gate blocks any agent `mode set` regardless, so this
/// tool exposes read access only — agents observe the mode (e.g. to back off when
/// the operator is away/asleep) but can never change operator authority.
pub(crate) fn dispatch_mode(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("get") {
        "get" => {
            let s = crate::operator_mode::get();
            json!({
                "ok": true,
                "mode": s.mode,
                "delegate_to": s.delegate_to,
                "delegate_scope": s.delegate_scope,
            })
        }
        other => json!({
            "error": format!(
                "mode is read-only via MCP (action '{other}'); set the operator mode with the \
                 `agend-terminal mode <active|away|sleep>` CLI (operator-only)"
            )
        }),
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
        let names: Vec<&'static str> = crate::mcp::registry::all().iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            vec![
                "reply",
                "download_attachment",
                "send",
                "inbox",
                "list_instances",
                "create_instance",
                "delete_instance",
                "start_instance",
                "replace_instance",
                "restart_instance",
                "interrupt",
                "set_display_name",
                "set_description",
                "set_waiting_on",
                "move_pane",
                "pane_snapshot",
                "tui_screenshot",
                "decision",
                "task",
                "task_sweep_config",
                "restart_daemon",
                "team",
                "schedule",
                "deployment",
                "ci",
                "health",
                "watchdog",
                "config",
                "repo",
                "bind_self",
                "release_worktree",
                "force_release_worktree",
                "binding_state",
                "gc_dry_run",
                "tokens",
                "mode",
            ]
        );
        assert_eq!(crate::mcp::registry::all().len(), 36);
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

    // ── #1602: inputSchema enforcement at dispatch ──────────────────────

    #[test]
    fn validate_rejects_missing_required_with_named_error() {
        // reply requires `message` (post-#1602 rename); omitting it is the
        // exact bug the operator hit (a mis-named param became an empty reply).
        let def = crate::mcp::tools::def_reply();
        let err = validate_args("reply", &def, &json!({})).expect("must reject");
        assert_eq!(
            err["error"], "reply: missing required parameter 'message'",
            "must name the tool + the missing param: {err}"
        );
    }

    #[test]
    fn validate_passes_when_required_present_and_unknown_only_warns() {
        // `message` present → no reject; an unknown key only warns (no reject).
        let def = crate::mcp::tools::def_reply();
        assert!(
            validate_args("reply", &def, &json!({"message": "hi"})).is_none(),
            "valid call must pass"
        );
        assert!(
            validate_args("reply", &def, &json!({"message": "hi", "bogus": 1})).is_none(),
            "unknown param must warn, not reject"
        );
    }

    /// #1602/#1603 audit pin. The systematic re-audit found 5 tools whose
    /// HANDLER defaults a field instead of erroring on its absence, so that
    /// field must NOT be in `required[]` (else the validator would hard-reject a
    /// legitimate call). Pins BOTH that the schema omits it from `required[]`
    /// AND that the validator lets the field-less call through. If a future
    /// tool/edit declares a handler-defaulted field required, this fails — re-run
    /// the audit (`grep unwrap_or` the handler).
    #[test]
    fn handler_defaulted_fields_are_not_declared_required() {
        use crate::mcp::tools::*;
        // (tool, def, handler-defaulted field, a legit call that omits it)
        let cases = [
            ("mode", def_mode(), "action", json!({})), // → "get" (read-only)
            (
                "create_instance",
                def_create_instance(),
                "name",
                json!({"team": "dev", "count": 2, "backend": "claude"}),
            ), // team mode auto-names; single path still errors "missing 'name'"
            (
                "set_waiting_on",
                def_set_waiting_on(),
                "condition",
                json!({}),
            ), // → clear
            (
                "set_display_name",
                def_set_display_name(),
                "name",
                json!({}),
            ), // → ""
            (
                "set_description",
                def_set_description(),
                "description",
                json!({}),
            ), // → ""
        ];
        for case in &cases {
            let (tool, def, field, args) = (case.0, &case.1, case.2, &case.3);
            let declares_required = def["inputSchema"]["required"]
                .as_array()
                .is_some_and(|r| r.iter().any(|v| v.as_str() == Some(field)));
            assert!(
                !declares_required,
                "{tool}: '{field}' is handler-defaulted — it must NOT be declared required[]"
            );
            assert!(
                validate_args(tool, def, args).is_none(),
                "{tool}: omitting handler-defaulted '{field}' must pass validation"
            );
        }
    }

    /// #1602: genuinely-required fields (the handler ERRORS on absence) stay
    /// enforced — the validator hard-rejects them with a named error.
    #[test]
    fn genuinely_required_fields_are_hard_rejected() {
        use crate::mcp::tools::*;
        let cases = [
            ("reply", def_reply(), "message"),
            ("send", def_send(), "message"),
            ("delete_instance", def_delete_instance(), "instance"),
            ("task", def_task(), "action"),
        ];
        for case in &cases {
            let (tool, def, field) = (case.0, &case.1, case.2);
            let err = validate_args(tool, def, &json!({})).expect("must reject");
            assert_eq!(
                err["error"],
                format!("{tool}: missing required parameter '{field}'"),
                "{tool} must hard-reject its missing required field"
            );
        }
    }

    #[test]
    fn try_dispatch_rejects_reply_without_message() {
        // End-to-end through the dispatch chokepoint.
        let home = std::env::temp_dir();
        let args = json!({}); // no message
        let ctx = ctx_for(&home, &args, "alpha");
        let result = try_dispatch("reply", &ctx).expect("registered tool returns Some");
        assert_eq!(
            result["error"], "reply: missing required parameter 'message'",
            "dispatch must reject reply with no message: {result}"
        );
    }
}

#[cfg(test)]
mod review_repro_mcp_dispatch_comms;
