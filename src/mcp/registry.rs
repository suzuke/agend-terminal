//! Single source of truth for MCP tool registration.
//!
//! Each `ToolEntry` pairs a tool's JSON schema definition with its dispatch
//! handler. Adding a new tool means adding one entry here + implementing the
//! handler function. `tools.rs` and `dispatch.rs` both read from this registry.

use super::handlers::dispatch::HandlerFn;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolTimeoutClass {
    Fast,
    Default,
    Slow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolClass {
    pub timeout: ToolTimeoutClass,
    /// True when a timeout should be hidden as `accepted_in_progress` because
    /// the background execution thread may still complete a non-idempotent
    /// side effect. Unknown future tools default to side-effect at lookup time.
    pub side_effect_on_timeout: bool,
    /// Pure-query tools skip per-call disk side effects (usage stats +
    /// heartbeat RMW); see `mcp::handlers::handle_tool`.
    pub read_only_disk_skip: bool,
}

impl ToolClass {
    pub const SIDE_EFFECT: Self = Self {
        timeout: ToolTimeoutClass::Default,
        side_effect_on_timeout: true,
        read_only_disk_skip: false,
    };
    pub const RETRY_SAFE: Self = Self {
        timeout: ToolTimeoutClass::Default,
        side_effect_on_timeout: false,
        read_only_disk_skip: false,
    };
    pub const READ_ONLY: Self = Self {
        timeout: ToolTimeoutClass::Default,
        side_effect_on_timeout: false,
        read_only_disk_skip: true,
    };
    pub const FAST_RETRY_SAFE: Self = Self {
        timeout: ToolTimeoutClass::Fast,
        side_effect_on_timeout: false,
        read_only_disk_skip: false,
    };
    pub const FAST_READ_ONLY: Self = Self {
        timeout: ToolTimeoutClass::Fast,
        side_effect_on_timeout: false,
        read_only_disk_skip: true,
    };
    pub const SLOW_SIDE_EFFECT: Self = Self {
        timeout: ToolTimeoutClass::Slow,
        side_effect_on_timeout: true,
        read_only_disk_skip: false,
    };
    pub const SLOW_RETRY_SAFE: Self = Self {
        timeout: ToolTimeoutClass::Slow,
        side_effect_on_timeout: false,
        read_only_disk_skip: false,
    };
}

pub(crate) struct ToolEntry {
    pub name: &'static str,
    pub definition: fn() -> Value,
    pub handler: HandlerFn,
    pub class: ToolClass,
}

pub(crate) fn all() -> &'static [ToolEntry] {
    &ALL_TOOLS
}

fn entry(name: &str) -> Option<&'static ToolEntry> {
    all().iter().find(|e| e.name == name)
}

/// Per-tool timeout band, from the registry single source of truth.
/// Unknown future tools fall back to the default timeout band.
pub(crate) fn timeout_class(name: &str) -> ToolTimeoutClass {
    entry(name)
        .map(|e| e.class.timeout)
        .unwrap_or(ToolTimeoutClass::Default)
}

/// Whether a timeout should return `accepted_in_progress`. Unknown future tools
/// default to side-effect (conservative: duplicate actions are worse than a lost
/// read result).
pub(crate) fn side_effect_on_timeout(name: &str) -> bool {
    entry(name)
        .map(|e| e.class.side_effect_on_timeout)
        .unwrap_or(true)
}

/// Whether `handle_tool` may skip usage-stats + heartbeat disk writes.
pub(crate) fn read_only_disk_skip(name: &str) -> bool {
    entry(name)
        .map(|e| e.class.read_only_disk_skip)
        .unwrap_or(false)
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

static ALL_TOOLS: [ToolEntry; 37] = [
    // ── Channel ──
    ToolEntry {
        name: "reply",
        definition: super::tools::def_reply,
        handler: super::handlers::dispatch::dispatch_reply,
        class: ToolClass::SIDE_EFFECT,
    },
    ToolEntry {
        name: "download_attachment",
        definition: super::tools::def_download_attachment,
        handler: super::handlers::dispatch::dispatch_download_attachment,
        class: ToolClass::RETRY_SAFE,
    },
    // ── Communication ──
    ToolEntry {
        name: "send",
        definition: super::tools::def_send,
        handler: super::handlers::dispatch::dispatch_send,
        class: ToolClass::SIDE_EFFECT,
    },
    ToolEntry {
        name: "inbox",
        definition: super::tools::def_inbox,
        handler: super::handlers::dispatch::dispatch_inbox,
        class: ToolClass::FAST_RETRY_SAFE,
    },
    // ── Instance ──
    ToolEntry {
        name: "list_instances",
        definition: super::tools::def_list_instances,
        handler: super::handlers::dispatch::dispatch_list_instances,
        class: ToolClass::FAST_READ_ONLY,
    },
    ToolEntry {
        name: "create_instance",
        definition: super::tools::def_create_instance,
        handler: super::handlers::dispatch::dispatch_create_instance,
        class: ToolClass::SLOW_SIDE_EFFECT,
    },
    ToolEntry {
        name: "delete_instance",
        definition: super::tools::def_delete_instance,
        handler: super::handlers::dispatch::dispatch_delete_instance,
        class: ToolClass::SIDE_EFFECT,
    },
    ToolEntry {
        name: "start_instance",
        definition: super::tools::def_start_instance,
        handler: super::handlers::dispatch::dispatch_start_instance,
        class: ToolClass::SIDE_EFFECT,
    },
    ToolEntry {
        name: "replace_instance",
        definition: super::tools::def_replace_instance,
        handler: super::handlers::dispatch::dispatch_replace_instance,
        class: ToolClass::SLOW_SIDE_EFFECT,
    },
    ToolEntry {
        name: "restart_instance",
        definition: super::tools::def_restart_instance,
        handler: super::handlers::dispatch::dispatch_restart_instance,
        class: ToolClass::SIDE_EFFECT,
    },
    ToolEntry {
        name: "interrupt",
        definition: super::tools::def_interrupt,
        handler: super::handlers::dispatch::dispatch_interrupt,
        class: ToolClass::SIDE_EFFECT,
    },
    ToolEntry {
        name: "set_display_name",
        definition: super::tools::def_set_display_name,
        handler: super::handlers::dispatch::dispatch_set_display_name,
        class: ToolClass::FAST_RETRY_SAFE,
    },
    ToolEntry {
        name: "set_description",
        definition: super::tools::def_set_description,
        handler: super::handlers::dispatch::dispatch_set_description,
        class: ToolClass::FAST_RETRY_SAFE,
    },
    ToolEntry {
        name: "set_waiting_on",
        definition: super::tools::def_set_waiting_on,
        handler: super::handlers::dispatch::dispatch_set_waiting_on,
        class: ToolClass::FAST_RETRY_SAFE,
    },
    ToolEntry {
        name: "move_pane",
        definition: super::tools::def_move_pane,
        handler: super::handlers::dispatch::dispatch_move_pane,
        class: ToolClass::SIDE_EFFECT,
    },
    ToolEntry {
        name: "pane_snapshot",
        definition: super::tools::def_pane_snapshot,
        handler: super::handlers::dispatch::dispatch_pane_snapshot,
        class: ToolClass::READ_ONLY,
    },
    ToolEntry {
        name: "tui_screenshot",
        definition: super::tools::def_tui_screenshot,
        handler: super::handlers::dispatch::dispatch_tui_screenshot,
        class: ToolClass::READ_ONLY,
    },
    // ── Decision ──
    ToolEntry {
        name: "decision",
        definition: super::tools::def_decision,
        handler: super::handlers::dispatch::dispatch_decision,
        class: ToolClass::SIDE_EFFECT,
    },
    // ── Task ──
    ToolEntry {
        name: "task",
        definition: super::tools::def_task,
        handler: super::handlers::dispatch::dispatch_task,
        class: ToolClass::SIDE_EFFECT,
    },
    ToolEntry {
        name: "task_sweep_config",
        definition: super::tools::def_task_sweep_config,
        handler: super::handlers::dispatch::dispatch_task_sweep_config,
        class: ToolClass::SIDE_EFFECT,
    },
    ToolEntry {
        name: "restart_daemon",
        definition: super::tools::def_restart_daemon,
        handler: super::handlers::dispatch::dispatch_restart_daemon,
        class: ToolClass::SLOW_SIDE_EFFECT,
    },
    // ── Team ──
    ToolEntry {
        name: "team",
        definition: super::tools::def_team,
        handler: super::handlers::dispatch::dispatch_team,
        class: ToolClass::SLOW_SIDE_EFFECT,
    },
    // ── Schedule ──
    ToolEntry {
        name: "schedule",
        definition: super::tools::def_schedule,
        handler: super::handlers::dispatch::dispatch_schedule,
        class: ToolClass::SIDE_EFFECT,
    },
    // ── Deploy ──
    ToolEntry {
        name: "deployment",
        definition: super::tools::def_deployment,
        handler: super::handlers::dispatch::dispatch_deployment,
        class: ToolClass::SLOW_SIDE_EFFECT,
    },
    // ── Ephemeral workers (#1967 Phase-1) ──
    ToolEntry {
        name: "ephemeral",
        definition: super::tools::def_ephemeral,
        handler: super::handlers::dispatch::dispatch_ephemeral,
        class: ToolClass::SIDE_EFFECT,
    },
    // ── CI ──
    ToolEntry {
        name: "ci",
        definition: super::tools::def_ci,
        handler: super::handlers::dispatch::dispatch_ci,
        class: ToolClass::SLOW_RETRY_SAFE,
    },
    // ── Health ──
    ToolEntry {
        name: "health",
        definition: super::tools::def_health,
        handler: super::handlers::dispatch::dispatch_health,
        class: ToolClass::FAST_RETRY_SAFE,
    },
    // ── Watchdog ──
    ToolEntry {
        name: "watchdog",
        definition: super::tools::def_watchdog,
        handler: super::handlers::dispatch::dispatch_watchdog,
        class: ToolClass::SIDE_EFFECT,
    },
    // ── Config ──
    ToolEntry {
        name: "config",
        definition: super::tools::def_config,
        handler: super::handlers::dispatch::dispatch_config,
        class: ToolClass::SIDE_EFFECT,
    },
    // ── Repo ──
    ToolEntry {
        name: "repo",
        definition: super::tools::def_repo,
        handler: super::handlers::dispatch::dispatch_repo,
        class: ToolClass::SLOW_SIDE_EFFECT,
    },
    // ── Worktree ──
    ToolEntry {
        name: "bind_self",
        definition: super::tools::def_bind_self,
        handler: super::handlers::dispatch::dispatch_bind_self,
        class: ToolClass::RETRY_SAFE,
    },
    ToolEntry {
        name: "release_worktree",
        definition: super::tools::def_release_worktree,
        handler: super::handlers::dispatch::dispatch_release_worktree,
        class: ToolClass::RETRY_SAFE,
    },
    ToolEntry {
        name: "force_release_worktree",
        definition: super::tools::def_force_release_worktree,
        handler: super::handlers::dispatch::dispatch_force_release_worktree,
        class: ToolClass::RETRY_SAFE,
    },
    ToolEntry {
        name: "binding_state",
        definition: super::tools::def_binding_state,
        handler: super::handlers::dispatch::dispatch_binding_state,
        class: ToolClass::READ_ONLY,
    },
    ToolEntry {
        name: "gc_dry_run",
        definition: super::tools::def_gc_dry_run,
        handler: super::handlers::dispatch::dispatch_gc_dry_run,
        class: ToolClass::READ_ONLY,
    },
    // ── Observability ──
    ToolEntry {
        name: "tokens",
        definition: super::tools::def_tokens,
        handler: super::handlers::dispatch::dispatch_tokens,
        class: ToolClass::READ_ONLY,
    },
    // ── #1339: Operator mode ──
    ToolEntry {
        name: "mode",
        definition: super::tools::def_mode,
        handler: super::handlers::dispatch::dispatch_mode,
        class: ToolClass::RETRY_SAFE,
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
    fn full_capability_roles_surface_all_37_byte_identical() {
        let all_names: Vec<&str> = all().iter().map(|e| e.name).collect();
        assert_eq!(all_names.len(), 37, "registry baseline is 37 tools");
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
                "role {role:?} must surface all 37 tools in registry order (default-all-open)"
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

    /// Parse the `### `name`` tool-section headers from a MCP-TOOLS doc body.
    fn doc_tool_names(body: &str) -> std::collections::BTreeSet<String> {
        body.lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("### ")?;
                let inner = rest.strip_prefix('`')?;
                let name = inner.split('`').next()?;
                Some(name.to_string())
            })
            .collect()
    }

    /// Extract the first run of ASCII digits from the doc's `# ... Reference ...`
    /// title line (handles both EN `(37 tools)` and zh `（37 個工具）`).
    fn doc_title_count(body: &str) -> Option<usize> {
        let title = body
            .lines()
            .find(|l| l.starts_with("# ") && l.contains("Reference"))?;
        let digits: String = title.chars().filter(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    }

    /// Doc drift guard: `docs/MCP-TOOLS.md` (+ its zh-TW twin) must document
    /// EXACTLY the tools in `ALL_TOOLS` — same set, same count — and the title's
    /// stated tool count must equal `ALL_TOOLS.len()`. Adding a tool to the
    /// registry without documenting it (or vice-versa) fails here, closing the
    /// silent doc-vs-registry drift class (registry had 37, docs listed 30).
    #[test]
    fn docs_match_registry_tool_set() {
        let registered: std::collections::BTreeSet<String> =
            all().iter().map(|e| e.name.to_string()).collect();
        let n = all().len();

        for rel in ["docs/MCP-TOOLS.md", "docs/MCP-TOOLS.zh-TW.md"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

            let documented = doc_tool_names(&body);
            let missing: Vec<&String> = registered.difference(&documented).collect();
            let extra: Vec<&String> = documented.difference(&registered).collect();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "{rel} tool sections drifted from ALL_TOOLS.\n  missing (in registry, not documented): {missing:?}\n  extra (documented, not in registry): {extra:?}"
            );

            let title_count = doc_title_count(&body)
                .unwrap_or_else(|| panic!("{rel}: could not parse tool count from title line"));
            assert_eq!(
                title_count, n,
                "{rel}: title says {title_count} tools but ALL_TOOLS has {n}"
            );
        }
    }
}
