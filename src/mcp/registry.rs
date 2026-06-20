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

/// #2300 P0: declarative per-role MCP tool subsets (capability registry).
///
/// Keyed by a typed [`crate::fleet::RoleKind`]'s canonical (snake_case) name →
/// the tool names that role may SEE (#2344: was a free-text `role` label that
/// never matched real prose, so the subsetting was inert).
/// **Default-all-open**: a role NOT listed here — including dev / lead /
/// orchestrator / unlabeled — gets the full `all()` surface, byte-identical to
/// pre-#2300. Only the listed read/report roles are trimmed, hiding footgun
/// lifecycle/bind/orchestration tools they never legitimately use (cuts #2055
/// surface + mis-trigger). P0 is VISIBILITY-ONLY: a trimmed tool is hidden from
/// the role's `tools/list` but still dispatches if hard-called (behavior-
/// preserving); the structural DENY is P1 (#2158). Conservative — when unsure a
/// tool is KEPT (default-all-open backs any gap). Subsets vetted by lead/operator.
const ROLE_TOOL_SUBSETS: &[(&str, &[&str])] = &[
    // reviewer: reads code + runs/checks CI + checks out the PR (`repo`) +
    // reports verdicts (`send`/`decision`) + tracks its own review `task`. Drops
    // instance & worktree lifecycle (the #2158 vector) + fleet orchestration.
    (
        "reviewer",
        &[
            "reply",
            "send",
            "inbox",
            "download_attachment",
            "list_instances",
            "binding_state",
            "pane_snapshot",
            "tui_screenshot",
            "task",
            "decision",
            "ci",
            "repo",
            "config",
            "set_waiting_on",
            "tokens",
            "health",
            "mode",
        ],
    ),
    // planner: reads code/CI to plan + reports. Same read/report surface as
    // reviewer (no lifecycle/bind/orchestration).
    (
        "planner",
        &[
            "reply",
            "send",
            "inbox",
            "download_attachment",
            "list_instances",
            "binding_state",
            "pane_snapshot",
            "tui_screenshot",
            "task",
            "decision",
            "ci",
            "repo",
            "config",
            "set_waiting_on",
            "tokens",
            "health",
            "mode",
        ],
    ),
    // explorer: read-only investigation + report. Strictest — also drops `repo`
    // (checkout = provisioning) and `ci` (run/dispatch); keeps comms + read +
    // observe + self-status.
    (
        "explorer",
        &[
            "reply",
            "send",
            "inbox",
            "download_attachment",
            "list_instances",
            "binding_state",
            "pane_snapshot",
            "tui_screenshot",
            "task",
            "decision",
            "config",
            "set_waiting_on",
            "tokens",
            "health",
            "mode",
        ],
    ),
];

/// #2300 P0 / #2344: the MCP tool entries a given typed [`crate::fleet::RoleKind`]
/// may SEE. Default-all-open: `None` (no `role_kind` declared — opt-in) or a
/// full-capability role (`Orchestrator` / `Implementer` / `Utility` / `Proxy`) →
/// the full `all()` surface (byte-identical, registry order). The three read/report
/// roles narrow to their curated subset; a subset name not in the registry is
/// ignored (never widens). Order always follows the registry, not the subset list.
///
/// #2344: the match on `RoleKind` is EXHAUSTIVE on purpose — adding a variant
/// forces a deliberate subset-vs-all-open decision here at compile time. (Was a
/// free-text `role` string exact-match that no real prose role ever hit → the
/// per-role subsetting was silently inert.)
pub(crate) fn tool_subset_for_role(
    role_kind: Option<crate::fleet::RoleKind>,
) -> Vec<&'static ToolEntry> {
    use crate::fleet::RoleKind;
    let subset_key: Option<&str> = match role_kind {
        Some(RoleKind::Reviewer) => Some("reviewer"),
        Some(RoleKind::Planner) => Some("planner"),
        Some(RoleKind::Explorer) => Some("explorer"),
        // Full-capability roles + undeclared (opt-in) → all-open.
        Some(RoleKind::Orchestrator)
        | Some(RoleKind::Implementer)
        | Some(RoleKind::Utility)
        | Some(RoleKind::Proxy)
        | None => None,
    };
    let subset = subset_key.and_then(|r| {
        ROLE_TOOL_SUBSETS
            .iter()
            .find(|(name, _)| *name == r)
            .map(|(_, tools)| *tools)
    });
    match subset {
        Some(names) => all().iter().filter(|e| names.contains(&e.name)).collect(),
        None => all().iter().collect(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::RoleKind;

    fn names(role: Option<RoleKind>) -> Vec<&'static str> {
        tool_subset_for_role(role).iter().map(|e| e.name).collect()
    }

    /// #2300 P0 byte-identical invariant: full-capability roles (orchestrator /
    /// implementer / utility / proxy) and `None` (undeclared role_kind) surface the
    /// ENTIRE registry in registry order — zero behavior change. If this breaks,
    /// default-all-open regressed.
    #[test]
    fn full_capability_roles_surface_all_36_byte_identical() {
        let all_names: Vec<&str> = all().iter().map(|e| e.name).collect();
        assert_eq!(all_names.len(), 36, "registry baseline is 36 tools");
        for role in [
            None,
            Some(RoleKind::Orchestrator),
            Some(RoleKind::Implementer),
            Some(RoleKind::Utility),
            Some(RoleKind::Proxy),
        ] {
            assert_eq!(
                names(role),
                all_names,
                "role {role:?} must surface all 36 tools in registry order (default-all-open)"
            );
        }
    }

    /// A narrowed role drops the footgun lifecycle/bind tools and KEEPS the
    /// tools its workflow needs. Pins the conservative subset contract.
    #[test]
    fn reviewer_drops_lifecycle_keeps_review_tools() {
        let r = names(Some(RoleKind::Reviewer));
        // Dropped: instance/worktree lifecycle (the #2158 vector) + orchestration.
        for cut in [
            "bind_self",
            "release_worktree",
            "force_release_worktree",
            "create_instance",
            "delete_instance",
            "restart_instance",
            "replace_instance",
            "start_instance",
            "restart_daemon",
            "team",
            "deployment",
            "schedule",
            "watchdog",
        ] {
            assert!(
                !r.contains(&cut),
                "reviewer must NOT see lifecycle/orchestration tool '{cut}'"
            );
        }
        // Kept: a reviewer must still review (repo/ci), report (send/decision),
        // and track its task — cutting these would break its workflow.
        for keep in [
            "reply",
            "send",
            "inbox",
            "task",
            "decision",
            "ci",
            "repo",
            "set_waiting_on",
        ] {
            assert!(
                r.contains(&keep),
                "reviewer must still see review tool '{keep}'"
            );
        }
        // Narrowed (strictly fewer than the full surface).
        assert!(
            r.len() < all().len(),
            "reviewer subset must be narrower than full"
        );
    }

    /// explorer is the strictest read-only role: also drops `repo` (provisioning)
    /// and `ci` (run/dispatch) that reviewer/planner keep.
    #[test]
    fn explorer_is_strictest_read_only() {
        let e = names(Some(RoleKind::Explorer));
        for cut in ["repo", "ci", "bind_self", "create_instance"] {
            assert!(
                !e.contains(&cut),
                "explorer (read-only) must NOT see '{cut}'"
            );
        }
        for keep in [
            "reply",
            "send",
            "inbox",
            "task",
            "list_instances",
            "pane_snapshot",
        ] {
            assert!(
                e.contains(&keep),
                "explorer must still see read/report tool '{keep}'"
            );
        }
    }

    /// Every name in a subset list must be a REAL registered tool — a typo'd
    /// subset entry is silently ignored by the filter, so this guards intent.
    #[test]
    fn subset_names_are_all_registered_tools() {
        let registered: std::collections::HashSet<&str> = all().iter().map(|e| e.name).collect();
        for (role, tools) in ROLE_TOOL_SUBSETS {
            for t in *tools {
                assert!(
                    registered.contains(t),
                    "role '{role}' subset names '{t}' which is not a registered tool (typo?)"
                );
            }
        }
    }
}
