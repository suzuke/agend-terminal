//! Single source of truth for MCP tool registration.
//!
//! Each `ToolEntry` pairs a tool's JSON schema definition with its dispatch
//! handler. Adding a new tool means adding one entry here + implementing the
//! handler function. `tools.rs` and `dispatch.rs` both read from this registry.

use super::handlers::dispatch::HandlerFn;
use serde_json::Value;

pub(crate) struct ToolEntry {
    pub name: &'static str,
    pub definition: fn() -> Value,
    pub handler: HandlerFn,
}

pub(crate) fn all() -> &'static [ToolEntry] {
    &ALL_TOOLS
}

static ALL_TOOLS: [ToolEntry; 36] = [
    // ── Channel ──
    ToolEntry {
        name: "reply",
        definition: super::tools::def_reply,
        handler: super::handlers::dispatch::dispatch_reply,
    },
    ToolEntry {
        name: "download_attachment",
        definition: super::tools::def_download_attachment,
        handler: super::handlers::dispatch::dispatch_download_attachment,
    },
    // ── Communication ──
    ToolEntry {
        name: "send",
        definition: super::tools::def_send,
        handler: super::handlers::dispatch::dispatch_send,
    },
    ToolEntry {
        name: "inbox",
        definition: super::tools::def_inbox,
        handler: super::handlers::dispatch::dispatch_inbox,
    },
    // ── Instance ──
    ToolEntry {
        name: "list_instances",
        definition: super::tools::def_list_instances,
        handler: super::handlers::dispatch::dispatch_list_instances,
    },
    ToolEntry {
        name: "create_instance",
        definition: super::tools::def_create_instance,
        handler: super::handlers::dispatch::dispatch_create_instance,
    },
    ToolEntry {
        name: "delete_instance",
        definition: super::tools::def_delete_instance,
        handler: super::handlers::dispatch::dispatch_delete_instance,
    },
    ToolEntry {
        name: "start_instance",
        definition: super::tools::def_start_instance,
        handler: super::handlers::dispatch::dispatch_start_instance,
    },
    ToolEntry {
        name: "replace_instance",
        definition: super::tools::def_replace_instance,
        handler: super::handlers::dispatch::dispatch_replace_instance,
    },
    ToolEntry {
        name: "restart_instance",
        definition: super::tools::def_restart_instance,
        handler: super::handlers::dispatch::dispatch_restart_instance,
    },
    ToolEntry {
        name: "interrupt",
        definition: super::tools::def_interrupt,
        handler: super::handlers::dispatch::dispatch_interrupt,
    },
    ToolEntry {
        name: "set_display_name",
        definition: super::tools::def_set_display_name,
        handler: super::handlers::dispatch::dispatch_set_display_name,
    },
    ToolEntry {
        name: "set_description",
        definition: super::tools::def_set_description,
        handler: super::handlers::dispatch::dispatch_set_description,
    },
    ToolEntry {
        name: "set_waiting_on",
        definition: super::tools::def_set_waiting_on,
        handler: super::handlers::dispatch::dispatch_set_waiting_on,
    },
    ToolEntry {
        name: "move_pane",
        definition: super::tools::def_move_pane,
        handler: super::handlers::dispatch::dispatch_move_pane,
    },
    ToolEntry {
        name: "pane_snapshot",
        definition: super::tools::def_pane_snapshot,
        handler: super::handlers::dispatch::dispatch_pane_snapshot,
    },
    ToolEntry {
        name: "tui_screenshot",
        definition: super::tools::def_tui_screenshot,
        handler: super::handlers::dispatch::dispatch_tui_screenshot,
    },
    // ── Decision ──
    ToolEntry {
        name: "decision",
        definition: super::tools::def_decision,
        handler: super::handlers::dispatch::dispatch_decision,
    },
    // ── Task ──
    ToolEntry {
        name: "task",
        definition: super::tools::def_task,
        handler: super::handlers::dispatch::dispatch_task,
    },
    ToolEntry {
        name: "task_sweep_config",
        definition: super::tools::def_task_sweep_config,
        handler: super::handlers::dispatch::dispatch_task_sweep_config,
    },
    ToolEntry {
        name: "restart_daemon",
        definition: super::tools::def_restart_daemon,
        handler: super::handlers::dispatch::dispatch_restart_daemon,
    },
    // ── Team ──
    ToolEntry {
        name: "team",
        definition: super::tools::def_team,
        handler: super::handlers::dispatch::dispatch_team,
    },
    // ── Schedule ──
    ToolEntry {
        name: "schedule",
        definition: super::tools::def_schedule,
        handler: super::handlers::dispatch::dispatch_schedule,
    },
    // ── Deploy ──
    ToolEntry {
        name: "deployment",
        definition: super::tools::def_deployment,
        handler: super::handlers::dispatch::dispatch_deployment,
    },
    // ── CI ──
    ToolEntry {
        name: "ci",
        definition: super::tools::def_ci,
        handler: super::handlers::dispatch::dispatch_ci,
    },
    // ── Health ──
    ToolEntry {
        name: "health",
        definition: super::tools::def_health,
        handler: super::handlers::dispatch::dispatch_health,
    },
    // ── Watchdog ──
    ToolEntry {
        name: "watchdog",
        definition: super::tools::def_watchdog,
        handler: super::handlers::dispatch::dispatch_watchdog,
    },
    // ── Config ──
    ToolEntry {
        name: "config",
        definition: super::tools::def_config,
        handler: super::handlers::dispatch::dispatch_config,
    },
    // ── Repo ──
    ToolEntry {
        name: "repo",
        definition: super::tools::def_repo,
        handler: super::handlers::dispatch::dispatch_repo,
    },
    // ── Worktree ──
    ToolEntry {
        name: "bind_self",
        definition: super::tools::def_bind_self,
        handler: super::handlers::dispatch::dispatch_bind_self,
    },
    ToolEntry {
        name: "release_worktree",
        definition: super::tools::def_release_worktree,
        handler: super::handlers::dispatch::dispatch_release_worktree,
    },
    ToolEntry {
        name: "force_release_worktree",
        definition: super::tools::def_force_release_worktree,
        handler: super::handlers::dispatch::dispatch_force_release_worktree,
    },
    ToolEntry {
        name: "binding_state",
        definition: super::tools::def_binding_state,
        handler: super::handlers::dispatch::dispatch_binding_state,
    },
    ToolEntry {
        name: "gc_dry_run",
        definition: super::tools::def_gc_dry_run,
        handler: super::handlers::dispatch::dispatch_gc_dry_run,
    },
    // ── Observability ──
    ToolEntry {
        name: "tokens",
        definition: super::tools::def_tokens,
        handler: super::handlers::dispatch::dispatch_tokens,
    },
    // ── #1339: Operator mode ──
    ToolEntry {
        name: "mode",
        definition: super::tools::def_mode,
        handler: super::handlers::dispatch::dispatch_mode,
    },
];
