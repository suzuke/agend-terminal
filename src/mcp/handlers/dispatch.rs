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
//! **Action sub-routing (T-B8)** — tools whose API consolidates several
//! sub-operations under one name (`task`, `decision`, `team`, …) carry
//! an optional [`HandlerEntry::actions`] table. [`try_dispatch`] inspects
//! `ctx.args["action"]`, looks the value up in that table, and routes to
//! the matching sub-handler if found. Missing-or-unknown action falls
//! through to the entry's base [`HandlerFn`] — which is the tool's
//! existing pre-migration handler, so error-handling for unknown
//! actions remains byte-identical to the pre-extraction code.
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
    /// Optional action sub-routing table. When `Some`, `try_dispatch`
    /// reads `ctx.args["action"]` and routes to the matching sub-handler;
    /// missing-or-unknown action falls through to `handler` (the base).
    /// `None` means flat dispatch — `ctx.args["action"]` is ignored.
    pub actions: Option<&'static [(&'static str, HandlerFn)]>,
}

/// Look the `tool` name up in the dispatch table. Returns `Some(value)`
/// on hit (including the action-routing path); returns `None` if the
/// tool isn't registered — the caller falls back to the inline `match`
/// in `mod.rs` for un-migrated arms.
pub(super) fn try_dispatch(tool: &str, ctx: &HandlerCtx<'_>) -> Option<Value> {
    for entry in registered_handlers() {
        if entry.name == tool {
            if let Some(action_table) = entry.actions {
                if let Some(action) = ctx.args["action"].as_str() {
                    // `HandlerFn` is `Copy`, so destructuring the
                    // ref-to-tuple as `&(_, sub_handler)` binds the
                    // fn pointer by value.
                    if let Some(&(_, sub_handler)) = action_table.iter().find(|(a, _)| *a == action)
                    {
                        return Some(sub_handler(ctx));
                    }
                }
                // Action missing or unknown → fall through to base
                // handler below. Preserves the pre-migration error
                // path (the base handler is the existing pub fn that
                // already has its own "unknown action" branch).
            }
            return Some((entry.handler)(ctx));
        }
    }
    None
}

// ---------------------------------------------------------------------
// Adapter fns — one per migrated tool. Each pulls the fields its
// underlying handler needs out of `HandlerCtx` and forwards the call.
// ---------------------------------------------------------------------

// Shape 1 — takes `instance_name`.

fn dispatch_list_instances(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_list_instances(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_create_instance(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_create_instance(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_set_description(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_set_description(ctx.home, ctx.args, ctx.instance_name)
}

// Shape 2 — ignores `instance_name`.

fn dispatch_interrupt(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_interrupt(ctx.home, ctx.args)
}

fn dispatch_delete_instance(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_delete_instance(ctx.home, ctx.args)
}

fn dispatch_start_instance(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_start_instance(ctx.home, ctx.args)
}

fn dispatch_replace_instance(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_replace_instance(ctx.home, ctx.args)
}

fn dispatch_move_pane(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_move_pane(ctx.home, ctx.args)
}

// Shape 3 — takes `instance_name` AND `sender`.

fn dispatch_set_waiting_on(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_set_waiting_on(ctx.home, ctx.args, ctx.instance_name, ctx.sender)
}

fn dispatch_send(ctx: &HandlerCtx<'_>) -> Value {
    comms::handle_unified_send(ctx.home, ctx.args, ctx.sender)
}

// Shape 4 — takes `sender` but ignores `instance_name`.

fn dispatch_bind_self(ctx: &HandlerCtx<'_>) -> Value {
    worktree::handle_bind_self(ctx.home, ctx.args, ctx.sender)
}

fn dispatch_binding_state(ctx: &HandlerCtx<'_>) -> Value {
    binding_state::handle_binding_state(ctx.home, ctx.args, ctx.sender)
}

fn dispatch_release_worktree(ctx: &HandlerCtx<'_>) -> Value {
    worktree::handle_release_worktree(ctx.home, ctx.args, ctx.sender)
}

fn dispatch_force_release_worktree(ctx: &HandlerCtx<'_>) -> Value {
    force_release::handle_force_release_worktree(ctx.home, ctx.args, ctx.sender)
}

fn dispatch_gc_dry_run(ctx: &HandlerCtx<'_>) -> Value {
    worktree::handle_gc_dry_run(ctx.home, ctx.args, ctx.sender)
}

// `task` — action sub-routing validation case (T-B8).
//
// All 5 sub-handlers (`dispatch_task_create` / `..._list` / `..._claim`
// / `..._update` / `..._done`) AND the base `dispatch_task` all forward
// to `task::handle_task`. The forwarding is identical at runtime — the
// actions table is purely structural here, adding a routing-table layer
// without changing what gets called.
//
// `task::handle_task` is itself a 1-line forward to `crate::tasks::handle`,
// which has its own internal match on `args["action"]`. So:
//
// * recognized action (e.g. "list") → dispatch table actions[…] hit →
//   matching sub-handler → task::handle_task → tasks::handle → matching
//   arm of internal match
// * unrecognized action (e.g. "frobnicate") → dispatch table actions miss
//   → fall-through to base `dispatch_task` → task::handle_task →
//   tasks::handle → "unknown action" error from internal match
//
// The "double match" (action-routing in dispatch.rs AND internal match
// in `tasks::handle`) is redundant work but preserves ZERO behavior
// change. Refactoring `tasks::handle` to expose per-action `pub fn`s
// is left as a future optimization once the dispatch table is stable
// across all action-based tools.

fn dispatch_task(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_task_create(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_task_list(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_task_claim(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_task_update(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_task_done(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_task_sweep(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_task_health(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task(ctx.home, ctx.args, ctx.instance_name)
}

/// Static sub-handler table for `task` action routing. Lifted out of
/// the entry literal so the table address can be `'static`-borrowed.
/// Action set matches `tasks::handle`'s internal `match` arms.
static TASK_ACTIONS: &[(&str, HandlerFn)] = &[
    ("create", dispatch_task_create),
    ("list", dispatch_task_list),
    ("claim", dispatch_task_claim),
    ("update", dispatch_task_update),
    ("done", dispatch_task_done),
    ("sweep", dispatch_task_sweep),
    ("health", dispatch_task_health),
];

// ---------------------------------------------------------------------
// Cohort migration (T-B9) — 7 action-based tools with N sub-handlers
// each. Pattern (per T-B8 spot-check ACK + T-B9 dispatch):
//
//   - `dispatch_<tool>` base handler relocates the inline `match` from
//     `mod.rs`. Recognized arms point to the same per-action fns the
//     inline match used; the `other` arm produces the same error JSON.
//   - Per-action sub-handlers (`dispatch_<tool>_<action>`) forward
//     directly to the per-action fns. The dispatch table catches
//     recognized actions and routes through these.
//   - `<TOOL>_ACTIONS` static lists the (action_name, sub_handler)
//     pairs.
//
// Runtime path: recognized action → dispatch table actions[…] hit →
// sub-handler → per-action fn. Unknown / missing action → fall through
// to `dispatch_<tool>` base → its `match` → `other` arm → unknown-
// action error JSON. The base handler's recognized-action arms are
// unreachable at runtime (dispatch table catches them first); they're
// retained because they mirror the pre-migration inline match exactly,
// which is the double-match safety net the dispatch contract requires.

// `ci` — actions: watch / unwatch / status.

fn dispatch_ci(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "watch" => ci::handle_watch_ci(ctx.home, ctx.args, ctx.instance_name),
        "unwatch" => ci::handle_unwatch_ci(ctx.home, ctx.args),
        "status" => ci::handle_status_ci(ctx.home, ctx.args, ctx.instance_name),
        other => json!({"error": format!("unknown ci action: {other}")}),
    }
}

fn dispatch_ci_watch(ctx: &HandlerCtx<'_>) -> Value {
    ci::handle_watch_ci(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_ci_unwatch(ctx: &HandlerCtx<'_>) -> Value {
    ci::handle_unwatch_ci(ctx.home, ctx.args)
}

fn dispatch_ci_status(ctx: &HandlerCtx<'_>) -> Value {
    ci::handle_status_ci(ctx.home, ctx.args, ctx.instance_name)
}

static CI_ACTIONS: &[(&str, HandlerFn)] = &[
    ("watch", dispatch_ci_watch),
    ("unwatch", dispatch_ci_unwatch),
    ("status", dispatch_ci_status),
];

// `decision` — actions: post / list / update.

fn dispatch_decision(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "post" => task::handle_post_decision(ctx.home, ctx.args, ctx.instance_name, ctx.sender),
        "list" => task::handle_list_decisions(ctx.home, ctx.args),
        "update" => task::handle_update_decision(ctx.home, ctx.args, ctx.instance_name),
        other => json!({"error": format!("unknown decision action: {other}")}),
    }
}

fn dispatch_decision_post(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_post_decision(ctx.home, ctx.args, ctx.instance_name, ctx.sender)
}

fn dispatch_decision_list(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_list_decisions(ctx.home, ctx.args)
}

fn dispatch_decision_update(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_update_decision(ctx.home, ctx.args, ctx.instance_name)
}

static DECISION_ACTIONS: &[(&str, HandlerFn)] = &[
    ("post", dispatch_decision_post),
    ("list", dispatch_decision_list),
    ("update", dispatch_decision_update),
];

// `deployment` — actions: deploy / teardown / list.

fn dispatch_deployment(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "deploy" => schedule::handle_deploy_template(ctx.home, ctx.args, ctx.instance_name),
        "teardown" => schedule::handle_teardown_deployment(ctx.home, ctx.args),
        "list" => schedule::handle_list_deployments(ctx.home),
        other => json!({"error": format!("unknown deployment action: {other}")}),
    }
}

fn dispatch_deployment_deploy(ctx: &HandlerCtx<'_>) -> Value {
    schedule::handle_deploy_template(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_deployment_teardown(ctx: &HandlerCtx<'_>) -> Value {
    schedule::handle_teardown_deployment(ctx.home, ctx.args)
}

fn dispatch_deployment_list(ctx: &HandlerCtx<'_>) -> Value {
    schedule::handle_list_deployments(ctx.home)
}

static DEPLOYMENT_ACTIONS: &[(&str, HandlerFn)] = &[
    ("deploy", dispatch_deployment_deploy),
    ("teardown", dispatch_deployment_teardown),
    ("list", dispatch_deployment_list),
];

// `health` — actions: report / clear.

fn dispatch_health(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "report" => {
            instance::handle_report_health(ctx.home, ctx.args, ctx.instance_name, ctx.sender)
        }
        "clear" => instance::handle_clear_blocked_reason(ctx.home, ctx.args),
        other => json!({"error": format!("unknown health action: {other}")}),
    }
}

fn dispatch_health_report(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_report_health(ctx.home, ctx.args, ctx.instance_name, ctx.sender)
}

fn dispatch_health_clear(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_clear_blocked_reason(ctx.home, ctx.args)
}

static HEALTH_ACTIONS: &[(&str, HandlerFn)] = &[
    ("report", dispatch_health_report),
    ("clear", dispatch_health_clear),
];

// `watchdog` — actions: snooze / resume / status (#1084).

fn dispatch_watchdog(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "snooze" => dispatch_watchdog_snooze(ctx),
        "resume" => dispatch_watchdog_resume(ctx),
        "status" => dispatch_watchdog_status(ctx),
        other => json!({"error": format!("unknown watchdog action: {other}")}),
    }
}

fn dispatch_watchdog_snooze(ctx: &HandlerCtx<'_>) -> Value {
    use crate::daemon::idle_watchdog;

    const MAX_SNOOZE_SECS: i64 = 4 * 3600; // 4h clamp

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
    if idle_watchdog::is_fleet_idle_snoozed(ctx.home) {
        let content =
            std::fs::read_to_string(ctx.home.join("fleet-idle-snooze.json")).unwrap_or_default();
        let snooze: idle_watchdog::FleetIdleSnooze =
            serde_json::from_str(&content).unwrap_or_default();
        let remaining = chrono::DateTime::parse_from_rfc3339(&snooze.snoozed_until)
            .ok()
            .map(|dt| {
                let r = dt
                    .with_timezone(&chrono::Utc)
                    .signed_duration_since(chrono::Utc::now())
                    .num_seconds();
                r.max(0)
            })
            .unwrap_or(0);
        json!({
            "snoozed": true,
            "snoozed_until": snooze.snoozed_until,
            "remaining_secs": remaining,
            "actor": snooze.actor,
        })
    } else {
        json!({"snoozed": false})
    }
}

/// Parse human-friendly duration strings like "2h", "30m", "1h30m".
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

static WATCHDOG_ACTIONS: &[(&str, HandlerFn)] = &[
    ("snooze", dispatch_watchdog_snooze),
    ("resume", dispatch_watchdog_resume),
    ("status", dispatch_watchdog_status),
];

// `repo` — actions: checkout / release.

fn dispatch_repo(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "checkout" => ci::handle_checkout_repo(ctx.home, ctx.args, ctx.instance_name),
        "release" => ci::handle_release_repo(ctx.args),
        "cleanup_init_commits" => {
            ci::handle_cleanup_init_commits(ctx.home, ctx.args, ctx.instance_name)
        }
        "cleanup_merged_branches" => {
            ci::handle_cleanup_merged_branches(ctx.home, ctx.args, ctx.instance_name)
        }
        other => json!({"error": format!("unknown repo action: {other}")}),
    }
}

fn dispatch_repo_checkout(ctx: &HandlerCtx<'_>) -> Value {
    ci::handle_checkout_repo(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_repo_release(ctx: &HandlerCtx<'_>) -> Value {
    ci::handle_release_repo(ctx.args)
}

fn dispatch_repo_cleanup_init_commits(ctx: &HandlerCtx<'_>) -> Value {
    ci::handle_cleanup_init_commits(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_repo_cleanup_merged_branches(ctx: &HandlerCtx<'_>) -> Value {
    ci::handle_cleanup_merged_branches(ctx.home, ctx.args, ctx.instance_name)
}

static REPO_ACTIONS: &[(&str, HandlerFn)] = &[
    ("checkout", dispatch_repo_checkout),
    ("release", dispatch_repo_release),
    ("cleanup_init_commits", dispatch_repo_cleanup_init_commits),
    (
        "cleanup_merged_branches",
        dispatch_repo_cleanup_merged_branches,
    ),
];

// `schedule` — actions: create / list / update / delete.

fn dispatch_schedule(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "create" => schedule::handle_create_schedule(ctx.home, ctx.args, ctx.instance_name),
        "list" => schedule::handle_list_schedules(ctx.home, ctx.args),
        "update" => schedule::handle_update_schedule(ctx.home, ctx.args),
        "delete" => schedule::handle_delete_schedule(ctx.home, ctx.args),
        other => json!({"error": format!("unknown schedule action: {other}")}),
    }
}

fn dispatch_schedule_create(ctx: &HandlerCtx<'_>) -> Value {
    schedule::handle_create_schedule(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_schedule_list(ctx: &HandlerCtx<'_>) -> Value {
    schedule::handle_list_schedules(ctx.home, ctx.args)
}

fn dispatch_schedule_update(ctx: &HandlerCtx<'_>) -> Value {
    schedule::handle_update_schedule(ctx.home, ctx.args)
}

fn dispatch_schedule_delete(ctx: &HandlerCtx<'_>) -> Value {
    schedule::handle_delete_schedule(ctx.home, ctx.args)
}

static SCHEDULE_ACTIONS: &[(&str, HandlerFn)] = &[
    ("create", dispatch_schedule_create),
    ("list", dispatch_schedule_list),
    ("update", dispatch_schedule_update),
    ("delete", dispatch_schedule_delete),
];

// `team` — actions: create / delete / list / update.

fn dispatch_team(ctx: &HandlerCtx<'_>) -> Value {
    match ctx.args["action"].as_str().unwrap_or("") {
        "create" => task::handle_create_team(ctx.home, ctx.args),
        "delete" => task::handle_delete_team(ctx.home, ctx.args),
        "list" => task::handle_list_teams(ctx.home),
        "update" => task::handle_update_team(ctx.home, ctx.args),
        other => json!({"error": format!("unknown team action: {other}")}),
    }
}

fn dispatch_team_create(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_create_team(ctx.home, ctx.args)
}

fn dispatch_team_delete(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_delete_team(ctx.home, ctx.args)
}

fn dispatch_team_list(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_list_teams(ctx.home)
}

fn dispatch_team_update(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_update_team(ctx.home, ctx.args)
}

static TEAM_ACTIONS: &[(&str, HandlerFn)] = &[
    ("create", dispatch_team_create),
    ("delete", dispatch_team_delete),
    ("list", dispatch_team_list),
    ("update", dispatch_team_update),
];

// ---------------------------------------------------------------------
// Channel-heavy cohort (T-B10) — `download_attachment` / `inbox` /
// `reply`. Selection is by arg presence (not by `args["action"]`), so
// every tool here uses `actions: None` and the dispatching branch lives
// inside the base adapter, exactly mirroring the pre-migration inline
// match arm. Side effects (telegram channel writes, filesystem media
// download, inbox storage RMW) are unchanged — adapters are pure
// forwards.

fn dispatch_download_attachment(ctx: &HandlerCtx<'_>) -> Value {
    channel::handle_download_attachment(ctx.home, ctx.args, ctx.instance_name)
}

// `inbox` — three-way branch on arg presence (NOT `args["action"]`):
//   - `message_id` present → `comms::handle_describe_message`
//   - else `thread_id` present → `comms::handle_describe_thread`
//   - else → `comms::handle_inbox` (drain pending)
// Order matters: the original inline match arm preferred message_id
// over thread_id, so this `if / else if / else` chain preserves byte-
// identical routing for callers that (incorrectly) pass both.
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

fn dispatch_reply(ctx: &HandlerCtx<'_>) -> Value {
    channel::handle_reply(ctx.home, ctx.args, ctx.instance_name)
}

// Final-cut flat tools (T-B11) — closing out #694 BLOCK 2 at 30/30+
// arms. All four are stateless: no `args["action"]` sub-routing, no
// per-tool counters, no env flags. Each adapter is a 1-line forward
// to the existing per-tool fn.

fn dispatch_set_display_name(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_set_display_name(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_pane_snapshot(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_pane_snapshot(ctx.home, ctx.args)
}

fn dispatch_task_sweep_config(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task_sweep_config(ctx.home, ctx.args)
}

// `restart_daemon` is high-sensitivity (Sprint 60 W1 PR-3): the
// daemon re-execs itself when this handler runs. ZERO behavior
// change requires byte-identical forwarding to the existing
// `restart::handle_restart_daemon`; the adapter is a verbatim 1-line
// forward — no added logic, no extra logging, no field reads beyond
// `ctx.home`.
fn dispatch_restart_daemon(ctx: &HandlerCtx<'_>) -> Value {
    restart::handle_restart_daemon(ctx.home)
}

/// Registered tool dispatchers. **Adding a tool**: write its adapter
/// above, append a [`HandlerEntry`] here. **Removing**: delete both
/// halves. Order doesn't affect correctness (linear-scan match by
/// `name`), but keeping similar tools clustered helps grep.
///
/// **Why `static` instead of inline `&[...]` return**: Rust's
/// const-promotion turns an `&[T {...}]` array literal in fn-return
/// position into a `'static` slice automatically — but only if every
/// field of `T` is itself const-promotable in that context. Once
/// `HandlerEntry` gained `actions: Option<&'static [(&str, HandlerFn)]>`
/// in T-B8, the inner static-slice field defeats promotion of the
/// outer array and rustc raises E0515 ("returns a reference to data
/// owned by the current function"). Lifting the array to a top-level
/// `static` gives the slice an unambiguous `'static` origin and side-
/// steps the gotcha. Symptom for future readers: if you add a field
/// to `HandlerEntry` and the array literal stops compiling, this
/// `static` is why.
static REGISTERED: &[HandlerEntry] = &[
    // Instance lifecycle — shape 1 (T-B7)
    HandlerEntry {
        name: "list_instances",
        handler: dispatch_list_instances,
        actions: None,
    },
    HandlerEntry {
        name: "create_instance",
        handler: dispatch_create_instance,
        actions: None,
    },
    HandlerEntry {
        name: "set_description",
        handler: dispatch_set_description,
        actions: None,
    },
    // Instance lifecycle — shape 2 (T-B7)
    HandlerEntry {
        name: "interrupt",
        handler: dispatch_interrupt,
        actions: None,
    },
    HandlerEntry {
        name: "delete_instance",
        handler: dispatch_delete_instance,
        actions: None,
    },
    HandlerEntry {
        name: "start_instance",
        handler: dispatch_start_instance,
        actions: None,
    },
    HandlerEntry {
        name: "replace_instance",
        handler: dispatch_replace_instance,
        actions: None,
    },
    HandlerEntry {
        name: "move_pane",
        handler: dispatch_move_pane,
        actions: None,
    },
    // Sender-style — shape 3 (T-B8)
    HandlerEntry {
        name: "set_waiting_on",
        handler: dispatch_set_waiting_on,
        actions: None,
    },
    HandlerEntry {
        name: "send",
        handler: dispatch_send,
        actions: None,
    },
    // Sender-style — shape 4 (T-B8)
    HandlerEntry {
        name: "bind_self",
        handler: dispatch_bind_self,
        actions: None,
    },
    HandlerEntry {
        name: "binding_state",
        handler: dispatch_binding_state,
        actions: None,
    },
    HandlerEntry {
        name: "release_worktree",
        handler: dispatch_release_worktree,
        actions: None,
    },
    HandlerEntry {
        name: "force_release_worktree",
        handler: dispatch_force_release_worktree,
        actions: None,
    },
    HandlerEntry {
        name: "gc_dry_run",
        handler: dispatch_gc_dry_run,
        actions: None,
    },
    // Action sub-routing (T-B8) — all 5 actions wired (`task`).
    HandlerEntry {
        name: "task",
        handler: dispatch_task,
        actions: Some(TASK_ACTIONS),
    },
    // Action-based cohort (T-B9): ci / decision / deployment / health
    // / repo / schedule / team. Alphabetical per dispatch contract's
    // suggested decomposition.
    HandlerEntry {
        name: "ci",
        handler: dispatch_ci,
        actions: Some(CI_ACTIONS),
    },
    HandlerEntry {
        name: "decision",
        handler: dispatch_decision,
        actions: Some(DECISION_ACTIONS),
    },
    HandlerEntry {
        name: "deployment",
        handler: dispatch_deployment,
        actions: Some(DEPLOYMENT_ACTIONS),
    },
    HandlerEntry {
        name: "health",
        handler: dispatch_health,
        actions: Some(HEALTH_ACTIONS),
    },
    // #1084: watchdog snooze/resume/status
    HandlerEntry {
        name: "watchdog",
        handler: dispatch_watchdog,
        actions: Some(WATCHDOG_ACTIONS),
    },
    HandlerEntry {
        name: "repo",
        handler: dispatch_repo,
        actions: Some(REPO_ACTIONS),
    },
    HandlerEntry {
        name: "schedule",
        handler: dispatch_schedule,
        actions: Some(SCHEDULE_ACTIONS),
    },
    HandlerEntry {
        name: "team",
        handler: dispatch_team,
        actions: Some(TEAM_ACTIONS),
    },
    // Channel-heavy cohort (T-B10): download_attachment / inbox / reply.
    // Flat dispatch (no `args["action"]` sub-routing) — the conceptual
    // "actions" for `inbox` (drain / describe / thread) are selected by
    // arg presence inside `dispatch_inbox`, not by an `action` field.
    HandlerEntry {
        name: "download_attachment",
        handler: dispatch_download_attachment,
        actions: None,
    },
    HandlerEntry {
        name: "inbox",
        handler: dispatch_inbox,
        actions: None,
    },
    HandlerEntry {
        name: "reply",
        handler: dispatch_reply,
        actions: None,
    },
    // Final-cut flat tools (T-B11) — closing out BLOCK 2 at 30/30+.
    HandlerEntry {
        name: "set_display_name",
        handler: dispatch_set_display_name,
        actions: None,
    },
    HandlerEntry {
        name: "pane_snapshot",
        handler: dispatch_pane_snapshot,
        actions: None,
    },
    HandlerEntry {
        name: "task_sweep_config",
        handler: dispatch_task_sweep_config,
        actions: None,
    },
    HandlerEntry {
        name: "restart_daemon",
        handler: dispatch_restart_daemon,
        actions: None,
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
        // Sender field carries `None` for the `&sender`-style adapters
        // — every one of them handles the no-sender case gracefully.
        static EMPTY_SENDER: Option<Sender> = None;
        HandlerCtx {
            home,
            args,
            instance_name: instance,
            sender: &EMPTY_SENDER,
        }
    }

    /// Unknown tool name → `None`, so `handle_tool` falls back to the
    /// inline match (which has its own `unknown tool` catch-all).
    #[test]
    fn try_dispatch_returns_none_for_unregistered_tool() {
        let home = std::env::temp_dir();
        let args = json!({});
        let ctx = ctx_for(&home, &args, "");
        assert!(try_dispatch("definitely_not_a_real_tool", &ctx).is_none());
    }

    /// Registered tool name → `Some(_)` — proves the table actually
    /// routes through to the adapter.
    #[test]
    fn try_dispatch_returns_some_for_registered_tool() {
        let home = std::env::temp_dir();
        let args = json!({});
        let ctx = ctx_for(&home, &args, "");
        assert!(try_dispatch("list_instances", &ctx).is_some());
    }

    /// Regression guard: pin the expected registered tool names + count.
    /// Future PRs that migrate more arms update this list.
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
                "repo",
                "schedule",
                "team",
                "download_attachment",
                "inbox",
                "reply",
                "set_display_name",
                "pane_snapshot",
                "task_sweep_config",
                "restart_daemon",
            ]
        );
        assert_eq!(registered_handlers().len(), 31);
    }

    /// Coverage test: every tool name advertised by
    /// [`crate::mcp::tools::tool_definitions`] must be routed somewhere
    /// — either by the dispatch table (`dispatch.rs`) or by the
    /// fallback inline `match` (`mod.rs`). Catches the bug class
    /// "tool added to the catalogue but routing forgotten".
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

    /// Action sub-routing — every recognized action across every
    /// action-based tool routes through its sub-handler. Parameterized
    /// on (tool, action) pairs to avoid 8+ near-identical test fns.
    /// The list mirrors the `actions` fields declared in `REGISTERED`
    /// and the inline-match arms removed from `mod.rs`; if a new
    /// action gets added to a tool, add it here so the routing is
    /// pinned.
    #[test]
    fn try_dispatch_routes_known_action_through_sub_handler() {
        let home = std::env::temp_dir();
        let cases: &[(&str, &[&str])] = &[
            (
                "task",
                &["create", "list", "claim", "update", "done", "sweep"],
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
            ("watchdog", &["snooze", "resume", "status"]),
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

    /// Cohort invariant — every action-based tool has `actions:
    /// Some(...)` populated (not `None`). Pins the "we wired the
    /// sub-routing infrastructure for every cohort member" property.
    #[test]
    fn action_based_tools_have_actions_populated() {
        let action_tools = [
            "task",
            "ci",
            "decision",
            "deployment",
            "health",
            "repo",
            "schedule",
            "team",
        ];
        for tool in action_tools {
            let entry = registered_handlers()
                .iter()
                .find(|e| e.name == tool)
                .unwrap_or_else(|| panic!("tool '{tool}' should be in dispatch table"));
            assert!(
                entry.actions.is_some(),
                "tool '{tool}' should have action sub-routing populated"
            );
        }
    }

    /// Action sub-routing — unknown action falls through to the base
    /// handler. The base handler is `dispatch_task` which forwards to
    /// `task::handle_task`. Internally `tasks::handle` will return its
    /// own "unknown action" error JSON. We only assert `Some(_)` to
    /// confirm the fall-through path executed (vs panicking or
    /// returning `None`).
    #[test]
    fn try_dispatch_unknown_action_falls_through_to_base() {
        let home = std::env::temp_dir();
        let args = json!({"action": "frobnicate-not-a-real-action"});
        let ctx = ctx_for(&home, &args, "");
        let result = try_dispatch("task", &ctx);
        assert!(result.is_some(), "fall-through must reach base handler");
        // Sanity-check the base handler ran by looking for the
        // upstream "unknown" error wording. (`tasks::handle` returns
        // `{"error": "unknown task action: ..."}` for unrecognized
        // actions; pinning this proves the fall-through actually
        // reached `tasks::handle` and not an empty Some(json!(null)).)
        let v = result.unwrap();
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(
            err.contains("unknown") || err.contains("action"),
            "expected unknown-action error from base; got: {v:?}"
        );
    }

    /// Action sub-routing — missing `action` arg falls through to the
    /// base handler (same path as unknown-action). The base handler's
    /// internal match handles the missing case its own way.
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
