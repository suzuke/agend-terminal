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
    binding_state, channel, ci, comms, instance, restart, review_assignment, schedule, task,
    usage_limit_takeover, worktree,
};

/// Shared per-call context — every common parameter `handle_tool`
/// would otherwise pass into the match arms, bundled together so each
/// [`HandlerFn`] has a single uniform shape.
pub(crate) struct HandlerCtx<'a> {
    pub home: &'a Path,
    pub args: &'a Value,
    pub instance_name: &'a str,
    pub sender: &'a Option<Sender>,
    pub runtime: Option<&'a RuntimeContext>,
}

/// Optional daemon runtime state available only when MCP tools are executed
/// through the in-process API `mcp_tool` path. Standalone bridge calls leave it
/// absent and keep the legacy socket/fallback behavior.
#[derive(Clone)]
pub(crate) struct RuntimeContext {
    pub registry: crate::agent::AgentRegistry,
    pub externals: crate::agent::ExternalRegistry,
    /// #2453 Stage R1: the API-server owner's restart capability, injected at
    /// `crate::api::serve` and carried here from the API `HandlerCtx` so
    /// `dispatch_restart_daemon` routes on an injected value, not a global.
    pub capability: crate::api::RestartCapability,
    /// #2453 Stage R2: the app owner-restart request channel + shared gate,
    /// injected at the app API composition root. `Some` only under
    /// `RestartCapability::App`; `None` everywhere else (daemon/verify fail-closed).
    pub app_restart: Option<crate::api::app_restart::AppRestart>,
    /// #2453 Stage R2 (flush barrier): a clone of the CURRENT API request's
    /// `PostFlushSlot`, carried from the API `HandlerCtx` so the restart handler can
    /// register a post-flush commit-permission ack that `handle_session` runs after
    /// it flushes THIS response. `None` off the api `mcp_tool` ingress (no request to
    /// tie a flush to → the handler cannot arm the barrier and fails closed).
    pub post_flush: Option<crate::api::app_restart::PostFlushSlot>,
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
/// allowlisted, keeping this validator a plain hard-reject. The 5 (now 4 after
/// #2547 merged `set_display_name`/`set_description` into `set_metadata`):
/// `mode` (`action` → `"get"`), `create_instance` (`name` auto-derived in team
/// mode; single path still errors "missing 'name'"), `set_waiting_on`
/// (`condition` absent = clear), and `set_metadata` (`name`/`description`
/// absent, sets `""` — a separate follow-up decides whether set-X-without-X
/// should hard-error; `action` itself IS required — see the action_adapter
/// fallthrough).
///
/// Tools whose handler DOES error on a missing field (`reply.message`,
/// `send.message`, `delete_instance.instance`, action-based `task`/`decision`)
/// keep their `required[]` and are hard-rejected here. A new tool that declares
/// a field its handler defaults will trip the pinning test below.
fn validate_args(tool: &str, def: &Value, args: &Value) -> Option<Value> {
    let schema = &def["inputSchema"];
    if let Some(required) = schema["required"].as_array() {
        for req in required.iter().filter_map(Value::as_str) {
            // Rank8: treat a present-but-JSON-null value as missing. `args.get`
            // returns `Some(Value::Null)` for `{"req": null}`, so a bare
            // `is_none()` let null slip through — the handler then defaulted it
            // (e.g. `as_str().unwrap_or("")` → an empty reply) and the failure
            // surfaced opaquely downstream (Telegram 400) instead of here. A
            // legit empty string is NOT null, so `""` still passes.
            if args.get(req).is_none_or(Value::is_null) {
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
    // Generic fn-generating arm: emit `fn $name(ctx) -> Value` that delegates to
    // the matching `@call` arm for `$shape`. Collapses the former six per-shape
    // fn arms (hai/ha/hais/has/h/a) into one — byte-identical expansion. The
    // `@call` arms below lead with `@` (not an `ident`), so an `adapter!(@call …)`
    // invocation can never match this `$name:ident` arm.
    ($name:ident, $shape:ident, $handler:expr) => {
        pub(crate) fn $name(ctx: &HandlerCtx<'_>) -> Value {
            adapter!(@call ctx, $shape, $handler)
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

pub(crate) fn dispatch_list_instances(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_list_instances_with_runtime(ctx.home, ctx.args, ctx.instance_name, ctx.runtime)
}

/// #2550 P1: folded READ-ONLY `instance` tool. Custom (not `action_adapter!`)
/// because `list` needs the runtime context (`handle_list_instances_with_runtime`),
/// a handler shape the shared `adapter!` macro doesn't cover. Only the read
/// actions are wired here and the schema's `action` enum keeps structural actions
/// off this tool; the three (name,action)-aware classifiers (operator_gate,
/// side-effect-on-timeout, role guard) independently enforce the read-only policy.
pub(crate) fn dispatch_instance(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "list" => instance::handle_list_instances_with_runtime(
            ctx.home,
            ctx.args,
            ctx.instance_name,
            ctx.runtime,
        ),
        "pane_snapshot" => instance::handle_pane_snapshot(ctx.home, ctx.args, ctx.runtime),
        other => json!({"error": format!("unknown instance action: {other}")}),
    }
}
adapter!(
    dispatch_create_instance,
    hai,
    instance::handle_create_instance
);
/// #2454: custom (not `adapter!`) so `handle_interrupt` receives the
/// `RuntimeContext` for its in-process best-effort snapshot (its INJECT stays a
/// loopback). Mirrors `dispatch_instance`/`dispatch_list_instances`.
pub(crate) fn dispatch_interrupt(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_interrupt(ctx.home, ctx.args, ctx.runtime)
}
adapter!(
    dispatch_delete_instance,
    has,
    instance::handle_delete_instance
);
adapter!(dispatch_start_instance, ha, instance::handle_start_instance);
adapter!(dispatch_bind_topic, ha, instance::handle_bind_topic);
adapter!(
    dispatch_restart_instance,
    ha,
    instance::handle_restart_instance
);
adapter!(
    dispatch_set_model,
    has,
    instance::set_model::handle_set_model
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
    dispatch_download_attachment,
    hai,
    channel::handle_download_attachment
);
adapter!(dispatch_reply, hai, channel::handle_reply);
/// #2454: custom (not `adapter!`) so the standalone `pane_snapshot` tool
/// receives the `RuntimeContext` and reads scrollback in-process via `agent_ops`.
pub(crate) fn dispatch_pane_snapshot(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_pane_snapshot(ctx.home, ctx.args, ctx.runtime)
}
/// #2453 Stage R1: custom (not `adapter!`) so the restart handler receives the
/// INJECTED host capability from the `RuntimeContext`. An absent runtime (a
/// standalone bridge call that never traversed the api `mcp_tool` ingress) maps
/// to `None` → default-deny in the handler.
pub(crate) fn dispatch_restart_daemon(ctx: &HandlerCtx<'_>) -> Value {
    restart::handle_restart_daemon(
        ctx.home,
        ctx.runtime.map(|r| r.capability),
        ctx.runtime.and_then(|r| r.app_restart.clone()),
        ctx.runtime.and_then(|r| r.post_flush.clone()),
    )
}

// ---------------------------------------------------------------------
// Action-based adapters — match on args["action"], route to per-action
// handler. Unknown actions produce tool-specific error JSON.
// ---------------------------------------------------------------------

adapter!(dispatch_task, hai, task::handle_task);
pub(crate) fn dispatch_usage_limit_takeover(ctx: &HandlerCtx<'_>) -> Value {
    usage_limit_takeover::handle_usage_limit_takeover(ctx)
}

action_adapter!(dispatch_ci, "ci", [
    "watch"       => ci::handle_watch_ci,       hai;
    "unwatch"     => ci::handle_unwatch_ci,     hai;
    "status"      => ci::handle_status_ci,      hai;
    "defer"       => ci::handle_defer_ci,       hai;
    "ack_handoff" => ci::handle_ack_handoff_ci, hai;
]);

action_adapter!(dispatch_decision, "decision", [
    "post"   => task::handle_post_decision,   hais;
    "list"   => task::handle_list_decisions,   ha;
    "update" => task::handle_update_decision,  hai;
    "answer" => task::handle_answer_decision,  hais;
]);

action_adapter!(dispatch_deployment, "deployment", [
    "deploy"   => schedule::handle_deploy_template,      hai;
    "teardown" => schedule::handle_teardown_deployment,   ha;
    "list"     => schedule::handle_list_deployments,      h;
]);

/// #2454: custom (not `action_adapter!`) because the health WRITE actions now
/// call the in-process `agent_ops` blocked-reason service and so need the
/// `RuntimeContext` forwarded — a handler shape the shared `adapter!` macro
/// doesn't cover (mirrors `dispatch_instance`/`dispatch_list_instances`).
/// `runtime = None` (the test-only `handle_tool` entry) makes the handlers
/// return an explicit "runtime unavailable" error, never a self-IPC `api::call`.
pub(crate) fn dispatch_health(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "report" => instance::handle_report_health(
            ctx.home,
            ctx.args,
            ctx.instance_name,
            ctx.sender,
            ctx.runtime,
        ),
        "clear" => instance::handle_clear_blocked_reason(ctx.home, ctx.args, ctx.runtime),
        other => json!({"error": format!("unknown health action: {other}")}),
    }
}

action_adapter!(dispatch_repo, "repo", [
    "checkout"                => ci::handle_checkout_repo,              hai;
    "release"                 => ci::handle_release_repo,               hai;
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

action_adapter!(dispatch_set_metadata, "set_metadata", [
    "display_name" => instance::handle_set_display_name, hai;
    "description"  => instance::handle_set_description,  hai;
]);

action_adapter!(dispatch_team, "team", [
    "create" => task::handle_create_team,  ha;
    "delete" => task::handle_delete_team,  ha;
    "list"   => task::handle_list_teams,   h;
    "update" => task::handle_update_team,  ha;
]);

// `inbox` — branch on `args["action"]` then arg presence:
//   - `action=ack`  → confirm processed (#2299; delivering → processed)
//   - `action=discharge` → #2622: close a channel-reply obligation reply-less
//   - `action=clear` → quiet compact-clear
//   - `message_id` present → describe single message
//   - else `thread_id` present → describe thread
//   - else → drain pending
pub(crate) fn dispatch_inbox(ctx: &HandlerCtx<'_>) -> Value {
    let action = ctx.args.get("action").and_then(|v| v.as_str());
    if action == Some("ack") {
        // #2299 explicit ack (C): confirm the agent HANDLED what it drained →
        // delivering → processed, so the reclaim-TTL won't re-deliver it.
        comms::handle_inbox_ack(ctx.home, ctx.args, ctx.instance_name)
    } else if action == Some("discharge") {
        // #2622: the deliberate exit for a channel-reply obligation that will
        // not be (or no longer needs to be) answered — durably suppresses
        // re-arm, stops the ladder, LOUDLY notifies the operator. Sibling of
        // `ack`/`clear` (all obligation-settling ops on inbox messages).
        channel::handle_discharge(ctx.home, ctx.args, ctx.instance_name)
    } else if action == Some("clear") {
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

/// #2782 slice 1: orchestrator-authorized (or operator-direct) exact
/// review-assignment revoke — the complement of the `review_assignment`
/// marker dispatch wired in `comms_delegate::review_assignment`.
pub(crate) fn dispatch_revoke_review_assignment(ctx: &HandlerCtx<'_>) -> Value {
    review_assignment::handle_revoke_review_assignment(ctx.home, ctx.args, ctx.sender)
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
        "list" => json!({"config": crate::runtime_config::list()}),
        other => json!({"error": format!("unknown config action: {other}")}),
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
            runtime: None,
        }
    }

    /// #2453 Stage R1: the restart adapter routes on the INJECTED runtime
    /// capability, not a process-global. `Some(App)` → the app fail-close arm.
    #[test]
    fn dispatch_restart_daemon_app_capability_routes_app_fail_close() {
        static EMPTY_SENDER: Option<Sender> = None;
        let home = std::env::temp_dir();
        let args = json!({});
        let runtime = RuntimeContext {
            registry: std::sync::Arc::new(
                parking_lot::Mutex::new(std::collections::HashMap::new()),
            ),
            externals: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            capability: crate::api::RestartCapability::App,
            app_restart: None,
            post_flush: None,
        };
        let ctx = HandlerCtx {
            home: &home,
            args: &args,
            instance_name: "",
            sender: &EMPTY_SENDER,
            runtime: Some(&runtime),
        };
        let resp = dispatch_restart_daemon(&ctx);
        assert_eq!(
            resp["ok"], false,
            "App capability must route to app fail-close, got {resp}"
        );
        assert!(
            resp["error"].as_str().unwrap_or("").contains("app"),
            "App route must return the app-mode fail-close message — got {resp}"
        );
    }

    /// #2453 Stage R1: an ABSENT runtime (standalone bridge, no api ingress) →
    /// `None` → default-deny, and must NOT take the app arm.
    #[test]
    fn dispatch_restart_daemon_absent_runtime_default_deny() {
        let home = std::env::temp_dir();
        let args = json!({});
        let ctx = ctx_for(&home, &args, ""); // runtime: None
        let resp = dispatch_restart_daemon(&ctx);
        assert_eq!(
            resp["ok"], false,
            "absent runtime must default-deny, got {resp}"
        );
        assert!(
            !resp["error"].as_str().unwrap_or("").contains("agend-terminal app"),
            "absent runtime must take the Unsupported/default-deny arm, not the app arm — got {resp}"
        );
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
                "restart_instance",
                "set_model",
                "bind_topic",
                "interrupt",
                "set_metadata",
                "set_waiting_on",
                "move_pane",
                "pane_snapshot",
                "instance",
                "decision",
                "task",
                "restart_daemon",
                "team",
                "schedule",
                "deployment",
                "ci",
                "health",
                "config",
                "repo",
                "bind_self",
                "release_worktree",
                "binding_state",
                "revoke_review_assignment",
                "usage_limit_takeover",
            ]
        );
        assert_eq!(crate::mcp::registry::all().len(), 32);
    }

    #[test]
    fn every_advertised_tool_is_routed_somewhere() {
        // #t-3 audit: the prior version grepped mod.rs + dispatch.rs SOURCE
        // text for the quoted tool name — a name appearing in a comment or
        // unrelated string would satisfy it without the tool actually being
        // routed (false confidence). We now drive the REAL routing path:
        // `try_dispatch` looks the name up in `mcp::registry::all()` (the
        // authoritative routing registry) and returns Some iff it routes.
        let defs = crate::mcp::tools::tool_definitions();
        let arr = defs
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tool_definitions() should return {tools: [...]}");
        let names: Vec<String> = arr
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .map(str::to_string)
            .collect();
        assert!(!names.is_empty(), "tool_definitions() advertised no tools");

        let home = std::env::temp_dir();
        let args = json!({});
        let ctx = ctx_for(&home, &args, "");
        let missing: Vec<&str> = names
            .iter()
            .filter(|name| try_dispatch(name, &ctx).is_none())
            .map(String::as_str)
            .collect();
        assert!(
            missing.is_empty(),
            "tools advertised by tool_definitions() but not routed through the dispatch registry: {missing:?}"
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
            ("ci", &["watch", "unwatch", "status", "defer"]),
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
            ("config", &["get", "list"]),
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

    /// #2550 P1: the folded `instance` tool routes its read actions AND produces
    /// byte-identical results to the standalone per-name tools (the alias
    /// contract). `runtime:None` + a temp home makes both paths deterministic:
    /// `list` short-circuits to the compact `list_agents()` view, `pane_snapshot`
    /// converges on the same handler once an `instance` is supplied — same
    /// handler, same args modulo the ignored `action` key.
    ///
    /// NOTE the ONE deliberate divergence (asserted separately below): a MISSING
    /// `instance` for pane_snapshot is caught by the SCHEMA layer for the per-name
    /// tool (`required:["instance"]`) but by the HANDLER's `require_instance` for
    /// the folded tool (`required:["action"]`, union-schema limitation). Both
    /// reject; only the error string differs. That's why we pin equivalence with
    /// `instance` PRESENT (both pass schema validation → identical handler call).
    #[test]
    fn folded_instance_read_actions_alias_per_name_tools() {
        let home = std::env::temp_dir();

        // action=list ≡ list_instances (neither declares a required `instance`, so
        // both pass schema validation and return the full compact list).
        let list_args = json!({"action": "list"});
        let ctx = ctx_for(&home, &list_args, "");
        let via_instance = try_dispatch("instance", &ctx).expect("instance(list) must route");
        let plain_args = json!({});
        let ctx = ctx_for(&home, &plain_args, "");
        let via_name = try_dispatch("list_instances", &ctx).expect("list_instances must route");
        assert_eq!(
            via_instance, via_name,
            "instance(action=list) must be byte-identical to list_instances"
        );

        // action=pane_snapshot ≡ pane_snapshot, with `instance` supplied so both
        // clear schema validation and converge on the identical handler call.
        let snap_args = json!({"action": "pane_snapshot", "instance": "alias-probe"});
        let ctx = ctx_for(&home, &snap_args, "");
        let via_instance =
            try_dispatch("instance", &ctx).expect("instance(pane_snapshot) must route");
        let plain_args = json!({"instance": "alias-probe"});
        let ctx = ctx_for(&home, &plain_args, "");
        let via_name = try_dispatch("pane_snapshot", &ctx).expect("pane_snapshot must route");
        assert_eq!(
            via_instance, via_name,
            "instance(action=pane_snapshot, instance=…) must be byte-identical to pane_snapshot(instance=…)"
        );
    }

    /// #2550 P1: the ONE intentional divergence — a MISSING `instance` for
    /// pane_snapshot is rejected at a DIFFERENT layer (schema vs handler) between
    /// the per-name and folded tools, because the folded schema can only require
    /// `action` (union limitation). Both still reject; this pins that neither
    /// silently accepts, documenting the layer difference.
    #[test]
    fn folded_instance_pane_snapshot_missing_instance_rejected_both_paths() {
        let home = std::env::temp_dir();
        let via_instance = try_dispatch(
            "instance",
            &ctx_for(&home, &json!({"action": "pane_snapshot"}), ""),
        )
        .expect("instance must route");
        let via_name =
            try_dispatch("pane_snapshot", &ctx_for(&home, &json!({}), "")).expect("must route");
        for (label, v) in [("folded", &via_instance), ("per-name", &via_name)] {
            let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("");
            assert!(
                err.contains("instance"),
                "{label} pane_snapshot without instance must error about 'instance'; got: {v:?}"
            );
        }
    }

    /// A structural / unknown action on the folded tool never reaches a handler —
    /// it falls through to the tool-specific unknown-action error (the schema's
    /// `action` enum already blocks it upstream; this is the dispatch backstop).
    #[test]
    fn folded_instance_unknown_action_errors() {
        let home = std::env::temp_dir();
        for action in ["delete", "restart", "bogus"] {
            let args = json!({ "action": action });
            let ctx = ctx_for(&home, &args, "");
            let v = try_dispatch("instance", &ctx).expect("instance must still return Some");
            let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("");
            assert!(
                err.contains("unknown instance action"),
                "instance({action}) must be an unknown-action error; got: {v:?}"
            );
        }
    }

    #[test]
    fn try_dispatch_missing_action_falls_through_to_base() {
        let home = std::env::temp_dir();
        let args = json!({}); // no "action" key
        let ctx = ctx_for(&home, &args, "");
        assert!(try_dispatch("task", &ctx).is_some());
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

    /// #1602/#1603 audit pin. The systematic re-audit found tools whose
    /// HANDLER defaults a field instead of erroring on its absence, so that
    /// field must NOT be in `required[]` (else the validator would hard-reject a
    /// legitimate call). Pins BOTH that the schema omits it from `required[]`
    /// AND that the validator lets the field-less call through. If a future
    /// tool/edit declares a handler-defaulted field required, this fails — re-run
    /// the audit (`grep unwrap_or` the handler). #2548: `mode` case removed —
    /// the tool was retired (folded into `list_instances`).
    #[test]
    fn handler_defaulted_fields_are_not_declared_required() {
        use crate::mcp::tools::*;
        // (tool, def, handler-defaulted field, a legit call that omits it)
        let cases = [
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
                "set_metadata",
                def_set_metadata(),
                "name",
                json!({"action": "display_name"}),
            ), // → ""
            (
                "set_metadata",
                def_set_metadata(),
                "description",
                json!({"action": "description"}),
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

    // ── Rank8 bug-audit: present-but-JSON-null required field ───────────────
    // `{"message": null}` slipped through validation because `args.get(req)`
    // returns `Some(Value::Null)` (not `None`), so `is_none()` saw it as
    // "present". `handle_reply` then did `as_str().unwrap_or("")` → forwarded an
    // EMPTY string → opaque downstream channel rejection (Telegram 400) instead
    // of a clean early named error. The fix treats present-but-null as missing.

    #[test]
    fn validate_rejects_present_but_null_required_field() {
        // The exact Rank8 bug: a null required value must reject like a missing
        // one, with the SAME named error — caught early at the validator, never
        // forwarded as an empty reply.
        let def = crate::mcp::tools::def_reply();
        let err = validate_args("reply", &def, &json!({"message": null}))
            .expect("a null required field must reject like a missing one");
        assert_eq!(
            err["error"], "reply: missing required parameter 'message'",
            "present-but-null must reject with the same named error as missing: {err}"
        );
    }

    #[test]
    fn validate_allows_empty_string_required_field() {
        // Precision: ONLY JSON null counts as missing. A legit empty string is a
        // real present value (null=absent, ""=present) and must still pass, so
        // the fix never wrongly blocks a caller that means to send "".
        let def = crate::mcp::tools::def_reply();
        assert!(
            validate_args("reply", &def, &json!({"message": ""})).is_none(),
            "empty-string message is a present value, not null — must not be rejected"
        );
    }

    #[test]
    fn validate_rejects_null_for_all_genuinely_required_fields() {
        // The null-as-missing rule lives in validate_args, so it benefits EVERY
        // handler — not just reply. Mirror the genuinely-required cases.
        use crate::mcp::tools::*;
        let cases = [
            ("reply", def_reply(), "message"),
            ("send", def_send(), "message"),
            ("delete_instance", def_delete_instance(), "instance"),
            ("task", def_task(), "action"),
        ];
        for case in &cases {
            let (tool, def, field) = (case.0, &case.1, case.2);
            let mut obj = serde_json::Map::new();
            obj.insert(field.to_string(), serde_json::Value::Null);
            let args = serde_json::Value::Object(obj);
            let err = validate_args(tool, def, &args).expect("a null required field must reject");
            assert_eq!(
                err["error"],
                format!("{tool}: missing required parameter '{field}'"),
                "{tool}: null '{field}' must be rejected like missing"
            );
        }
    }

    // #2454 Slice 5 RED: task health/sweep must consume the live registry
    // forwarded through the real MCP task dispatcher.  The current handler
    // still self-IPC's through the API (or reads runtime state from disk), so
    // these tests deterministically fail with no daemon/socket listener.
    fn runtime_with_external(name: &str) -> RuntimeContext {
        RuntimeContext {
            registry: std::sync::Arc::new(
                parking_lot::Mutex::new(std::collections::HashMap::new()),
            ),
            externals: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::from([(
                    name.to_string(),
                    crate::agent::ExternalAgentHandle {
                        backend_command: "codex".to_string(),
                        pid: 4242,
                    },
                )]),
            )),
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: None,
        }
    }

    fn task_runtime_ctx<'a>(
        home: &'a Path,
        args: &'a Value,
        runtime: &'a RuntimeContext,
    ) -> HandlerCtx<'a> {
        static EMPTY_SENDER: Option<Sender> = None;
        HandlerCtx {
            home,
            args,
            instance_name: "operator",
            sender: &EMPTY_SENDER,
            runtime: Some(runtime),
        }
    }

    fn backdate_task(home: &Path, task_id: &str) {
        let path = home.join("task_events.jsonl");
        let old = (chrono::Utc::now() - chrono::Duration::days(31)).to_rfc3339();
        let content = std::fs::read_to_string(&path).expect("task event log");
        let rewritten = content
            .lines()
            .map(|line| {
                let mut value: Value = serde_json::from_str(line).expect("task event JSON");
                if value["event"]["task_id"] == task_id {
                    value["timestamp"] = json!(old);
                }
                serde_json::to_string(&value).expect("serialized task event")
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, format!("{rewritten}\n")).expect("rewrite task event log");
    }

    #[test]
    fn task_health_uses_supplied_runtime_without_api_listener_2454() {
        let home = std::env::temp_dir().join(format!(
            "agend-task-health-runtime-red-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        let created = crate::tasks::handle(
            &home,
            "operator",
            &json!({
                "action": "create",
                "title": "runtime-owned task",
                "assignee": "live-agent"
            }),
        );
        let task_id = created["id"].as_str().expect("created task id");
        backdate_task(&home, task_id);

        let args = json!({"action": "health"});
        let runtime = runtime_with_external("live-agent");
        let ctx = task_runtime_ctx(&home, &args, &runtime);
        let result = try_dispatch("task", &ctx).expect("task dispatch result");
        assert_eq!(
            result["live_agents_available"], true,
            "health must use the supplied live RuntimeContext without API fallback: {result}"
        );
        let strict_owners = result["ghost_owners"]["strict_owners"]
            .as_array()
            .expect("strict owner list");
        assert!(
            !strict_owners.iter().any(|owner| owner == "live-agent"),
            "a supplied live owner must not be classified as strict/ghost: {result}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn task_sweep_uses_supplied_runtime_without_api_listener_2454() {
        let home = std::env::temp_dir().join(format!(
            "agend-task-sweep-runtime-red-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        let created = crate::tasks::handle(
            &home,
            "operator",
            &json!({
                "action": "create",
                "title": "runtime-owned stale task",
                "assignee": "live-agent"
            }),
        );
        let task_id = created["id"].as_str().expect("created task id");
        backdate_task(&home, task_id);

        let args = json!({"action": "sweep"});
        let runtime = runtime_with_external("live-agent");
        let ctx = task_runtime_ctx(&home, &args, &runtime);
        let result = try_dispatch("task", &ctx).expect("task dispatch result");
        assert_eq!(
            result["dry_run"], true,
            "sweep must return a dry-run plan: {result}"
        );
        let disbanded = result["categories"]["team_disbanded"]
            .as_array()
            .expect("team_disbanded category");
        assert!(
            !disbanded.iter().any(|candidate| candidate["id"] == task_id),
            "a supplied live owner must not be classified as team-disbanded: {result}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn task_health_and_sweep_runtime_none_are_explicit_errors_2454() {
        let home = std::env::temp_dir().join(format!(
            "agend-task-runtime-none-red-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        for action in ["health", "sweep"] {
            let args = json!({"action": action});
            let ctx = ctx_for(&home, &args, "operator");
            let result = try_dispatch("task", &ctx).expect("task dispatch result");
            assert!(
                result["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("runtime unavailable"),
                "task action={action} with runtime=None must fail explicitly, never socket-fallback: {result}"
            );
        }
        std::fs::remove_dir_all(home).ok();
    }
}
