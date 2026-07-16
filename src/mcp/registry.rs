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
            "instance",
            "task",
            "decision",
            "ci",
            "repo",
            "config",
            "set_waiting_on",
            "health",
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
            "instance",
            "task",
            "decision",
            "ci",
            "repo",
            "config",
            "set_waiting_on",
            "health",
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
            "instance",
            "task",
            "decision",
            "config",
            "set_waiting_on",
            "health",
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

/// #2300 P1 / #2055: execution-time companion to [`tool_subset_for_role`].
/// `tools/list` visibility alone is advisory; the daemon must also hard-deny a
/// registered tool call that is hidden from the caller's typed role. Unknown
/// tool names stay allowed here so the normal executor can return the existing
/// unknown-tool error instead of turning typos into capability denials.
pub(crate) fn tool_allowed_for_role(role_kind: Option<crate::fleet::RoleKind>, tool: &str) -> bool {
    all().iter().all(|e| e.name != tool)
        || tool_subset_for_role(role_kind)
            .iter()
            .any(|entry| entry.name == tool)
}

// ── #2550 P0: instance-family action policy (single source of truth) ──
//
// Once the `instance` tool folds the per-name instance lifecycle tools (P1+),
// the three name-based classifiers below — operator_gate authority
// (`api::operator_gate::classify`), retry side-effect
// (`side_effect_on_timeout_for`), and role visibility/deny
// (`tool_allowed_for_role_action`) — must agree on each action's policy or they
// drift into a security hole (#2158: role that may `instance(action=list)` must
// NOT thereby gain `instance(action=delete)`). This table is that shared source
// of truth. Until the `instance` tool actually ships, it is DORMANT (no caller
// passes `tool == "instance"`), and every value below is chosen to be
// byte-equivalent to the CURRENT per-name tool's classifier (pinned by the unit
// tests in this module). Excludes `create`/`restart_daemon` (operator decision:
// stay standalone) and `set_metadata` (already action-bearing; folding deferred).

/// operator_gate authority for a folded instance action, expressed in a
/// registry-local enum so `mcp` need not depend on `api::operator_gate`
/// (`operator_gate` maps this onto its own `OpClass`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstanceAuthority {
    AlwaysAllow,
    DelegateScoped,
    AbsolutelyNever,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InstanceActionPolicy {
    pub authority: InstanceAuthority,
    pub side_effect_on_timeout: bool,
    pub timeout: ToolTimeoutClass,
    /// Whether the read/report roles (reviewer/planner/explorer) may use this
    /// action — mirrors the pre-fold per-name membership in `ROLE_TOOL_SUBSETS`.
    pub read_role_allowed: bool,
}

/// Policy for a folded `instance` action; `None` for any action NOT folded into
/// the `instance` tool (create / restart_daemon / set_metadata / unknown).
pub(crate) fn instance_action_policy(action: &str) -> Option<InstanceActionPolicy> {
    use InstanceAuthority::*;
    use ToolTimeoutClass::*;
    let p = |authority, side_effect_on_timeout, timeout, read_role_allowed| {
        Some(InstanceActionPolicy {
            authority,
            side_effect_on_timeout,
            timeout,
            read_role_allowed,
        })
    };
    // Values MUST mirror the current per-name tool (byte-equivalent); see the
    // baseline table in the unit tests below.
    match action {
        "list" => p(AlwaysAllow, false, Fast, true), // list_instances
        "pane_snapshot" => p(AlwaysAllow, false, Default, true), // pane_snapshot
        "set_waiting_on" => p(AlwaysAllow, false, Fast, true), // set_waiting_on
        "interrupt" => p(AlwaysAllow, true, Default, false), // interrupt
        "bind_topic" => p(DelegateScoped, true, Default, false), // bind_topic (gate fallback)
        "move_pane" => p(AbsolutelyNever, true, Default, false), // move_pane
        "delete" => p(AbsolutelyNever, true, Default, false), // delete_instance
        "start" => p(AbsolutelyNever, true, Default, false), // start_instance
        "restart" => p(AbsolutelyNever, true, Default, false), // restart_instance
        _ => None,
    }
}

/// (name, action)-aware [`side_effect_on_timeout`]. For the folded `instance`
/// tool, per-action; otherwise byte-identical to `side_effect_on_timeout(name)`.
/// An `instance` call with an unknown/unfolded action stays conservative
/// (side-effect = true), matching the pre-existing unknown-tool default.
pub(crate) fn side_effect_on_timeout_for(name: &str, action: Option<&str>) -> bool {
    if name == "instance" {
        return action
            .and_then(instance_action_policy)
            .map(|p| p.side_effect_on_timeout)
            .unwrap_or(true);
    }
    side_effect_on_timeout(name)
}

/// Whether `role_kind` narrows to a curated read/report subset (reviewer /
/// planner / explorer) rather than the full-capability all-open surface.
fn role_has_read_subset(role_kind: Option<crate::fleet::RoleKind>) -> bool {
    use crate::fleet::RoleKind;
    matches!(
        role_kind,
        Some(RoleKind::Reviewer) | Some(RoleKind::Planner) | Some(RoleKind::Explorer)
    )
}

/// (name, action)-aware [`tool_allowed_for_role`] (the #2158 privilege-escalation
/// guard). For the folded `instance` tool, a curated read/report role is allowed
/// ONLY the `read_role_allowed` actions (e.g. `list`/`pane_snapshot`), never a
/// structural action (`delete`/`restart`/…) — so folding `list` into the same
/// tool as `delete` cannot silently widen a restricted role. Full-capability
/// roles keep the full surface. For any non-`instance` tool this is
/// byte-identical to `tool_allowed_for_role(role_kind, tool)`.
pub(crate) fn tool_allowed_for_role_action(
    role_kind: Option<crate::fleet::RoleKind>,
    tool: &str,
    action: Option<&str>,
) -> bool {
    if tool == "instance" {
        if !role_has_read_subset(role_kind) {
            return true; // full-capability role → all instance actions
        }
        return action
            .and_then(instance_action_policy)
            .map(|p| p.read_role_allowed)
            .unwrap_or(false);
    }
    tool_allowed_for_role(role_kind, tool)
}

static ALL_TOOLS: [ToolEntry; 32] = [
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
        name: "restart_instance",
        definition: super::tools::def_restart_instance,
        handler: super::handlers::dispatch::dispatch_restart_instance,
        class: ToolClass::SIDE_EFFECT,
    },
    ToolEntry {
        name: "set_model",
        definition: super::tools::def_set_model,
        handler: super::handlers::dispatch::dispatch_set_model,
        class: ToolClass::SIDE_EFFECT,
    },
    ToolEntry {
        name: "bind_topic",
        definition: super::tools::def_bind_topic,
        handler: super::handlers::dispatch::dispatch_bind_topic,
        class: ToolClass::SIDE_EFFECT,
    },
    ToolEntry {
        name: "interrupt",
        definition: super::tools::def_interrupt,
        handler: super::handlers::dispatch::dispatch_interrupt,
        class: ToolClass::SIDE_EFFECT,
    },
    ToolEntry {
        name: "set_metadata",
        definition: super::tools::def_set_metadata,
        handler: super::handlers::dispatch::dispatch_set_metadata,
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
    // #2550 P1: folded read-only alias for list_instances + pane_snapshot. The
    // ToolClass is deliberately RETRY_SAFE (not READ_ONLY): the folded tool is a
    // forward-looking mixed read/write surface (P2+ will add mutating actions), so
    // a name-level disk-skip would wrongly apply to future mutations. The read
    // disk-skip optimization is traded away; the security-relevant classification
    // is (name,action)-aware (side_effect_on_timeout_for / operator_gate / role
    // guard), independent of this ToolClass.
    ToolEntry {
        name: "instance",
        definition: super::tools::def_instance,
        handler: super::handlers::dispatch::dispatch_instance,
        class: ToolClass::RETRY_SAFE,
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
        name: "binding_state",
        definition: super::tools::def_binding_state,
        handler: super::handlers::dispatch::dispatch_binding_state,
        class: ToolClass::READ_ONLY,
    },
    // #2782 slice 1: orchestrator-authorized exact review-assignment revoke.
    ToolEntry {
        name: "revoke_review_assignment",
        definition: super::tools::def_revoke_review_assignment,
        handler: super::handlers::dispatch::dispatch_revoke_review_assignment,
        class: ToolClass::SIDE_EFFECT,
    },
    ToolEntry {
        name: "usage_limit_takeover",
        definition: super::tools::def_usage_limit_takeover,
        handler: super::handlers::dispatch::dispatch_usage_limit_takeover,
        class: ToolClass::SIDE_EFFECT,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::RoleKind;

    /// #2550 P0 byte-equivalence: each folded `instance` action's policy equals
    /// the CURRENT per-name tool's three classifiers (the instance tool is
    /// dormant until P1). Baseline read from ALL_TOOLS + operator_gate +
    /// ROLE_TOOL_SUBSETS — if a per-name tool's class changes, this must too.
    #[test]
    fn instance_action_policy_matches_per_name_baseline() {
        use InstanceAuthority::*;
        use ToolTimeoutClass::*;
        // (action, authority, side_effect_on_timeout, timeout, read_role_allowed)
        let cases: &[(&str, InstanceAuthority, bool, ToolTimeoutClass, bool)] = &[
            ("list", AlwaysAllow, false, Fast, true),
            ("pane_snapshot", AlwaysAllow, false, Default, true),
            ("set_waiting_on", AlwaysAllow, false, Fast, true),
            ("interrupt", AlwaysAllow, true, Default, false),
            ("bind_topic", DelegateScoped, true, Default, false),
            ("move_pane", AbsolutelyNever, true, Default, false),
            ("delete", AbsolutelyNever, true, Default, false),
            ("start", AbsolutelyNever, true, Default, false),
            ("restart", AbsolutelyNever, true, Default, false),
        ];
        for (a, auth, se, to, rr) in cases {
            let p = instance_action_policy(a).unwrap_or_else(|| panic!("no policy for {a}"));
            assert_eq!(p.authority, *auth, "{a} authority");
            assert_eq!(p.side_effect_on_timeout, *se, "{a} side_effect");
            assert_eq!(p.timeout, *to, "{a} timeout");
            assert_eq!(p.read_role_allowed, *rr, "{a} read_role");
            assert_eq!(
                side_effect_on_timeout_for("instance", Some(a)),
                *se,
                "{a} variant"
            );
        }
        // create / restart_daemon (operator: standalone) + set_metadata (already
        // action-bearing) + unknown are NOT folded → no policy.
        for a in ["create", "restart_daemon", "set_metadata", "bogus"] {
            assert!(
                instance_action_policy(a).is_none(),
                "{a} must not be folded"
            );
        }
    }

    /// The (name,action)-aware variants IGNORE action for every non-`instance`
    /// tool → byte-identical to the name-only classifiers (zero behavior change
    /// before the alias ships).
    #[test]
    fn non_instance_classifiers_ignore_action() {
        for name in ["send", "delete_instance", "list_instances", "inbox", "task"] {
            assert_eq!(
                side_effect_on_timeout_for(name, Some("x")),
                side_effect_on_timeout(name),
                "{name} side_effect"
            );
            assert_eq!(
                side_effect_on_timeout_for(name, None),
                side_effect_on_timeout(name)
            );
        }
    }

    /// #2158 privilege-escalation guard: a curated read role may
    /// `instance(list/pane_snapshot/set_waiting_on)` but NEVER a structural
    /// action — folding `list` and `delete` into one tool must not widen the role.
    #[test]
    fn instance_role_guard_blocks_structural_for_read_roles() {
        let rev = Some(RoleKind::Reviewer);
        for a in ["list", "pane_snapshot", "set_waiting_on"] {
            assert!(
                tool_allowed_for_role_action(rev, "instance", Some(a)),
                "reviewer must keep instance({a})"
            );
        }
        for a in [
            "delete",
            "restart",
            "start",
            "move_pane",
            "bind_topic",
            "interrupt",
        ] {
            assert!(
                !tool_allowed_for_role_action(rev, "instance", Some(a)),
                "reviewer must NOT gain instance({a}) via the folded tool"
            );
        }
        assert!(!tool_allowed_for_role_action(rev, "instance", None));
        // Full-capability role (undeclared) keeps every instance action.
        for a in ["delete", "list", "restart"] {
            assert!(tool_allowed_for_role_action(None, "instance", Some(a)));
        }
        // Non-instance tools: byte-identical to the name-only guard.
        assert_eq!(
            tool_allowed_for_role_action(rev, "delete_instance", Some("x")),
            tool_allowed_for_role(rev, "delete_instance")
        );
        assert_eq!(
            tool_allowed_for_role_action(rev, "list_instances", None),
            tool_allowed_for_role(rev, "list_instances")
        );
    }

    fn names(role: Option<RoleKind>) -> Vec<&'static str> {
        tool_subset_for_role(role).iter().map(|e| e.name).collect()
    }

    /// #2300 P0 byte-identical invariant: full-capability roles (orchestrator /
    /// implementer / utility / proxy) and `None` (undeclared role_kind) surface the
    /// ENTIRE registry in registry order — zero behavior change. If this breaks,
    /// default-all-open regressed.
    #[test]
    fn full_capability_roles_surface_all_30_byte_identical() {
        let all_names: Vec<&str> = all().iter().map(|e| e.name).collect();
        assert_eq!(
            all_names.len(),
            32,
            "registry baseline is 32 tools (+ usage_limit_takeover Architecture-14 item 5 Slice 2A)"
        );
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
                "role {role:?} must surface all 32 tools in registry order (default-all-open)"
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
            "create_instance",
            "delete_instance",
            "restart_instance",
            "set_model",
            "start_instance",
            "restart_daemon",
            "team",
            "deployment",
            "schedule",
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

    #[test]
    fn execution_capability_matches_visible_subset_without_changing_unknown_tools() {
        assert!(
            !tool_allowed_for_role(Some(RoleKind::Reviewer), "create_instance"),
            "reviewer must be execution-denied for hidden registered lifecycle tools"
        );
        assert!(
            tool_allowed_for_role(Some(RoleKind::Reviewer), "inbox"),
            "reviewer must still execute visible workflow tools"
        );
        assert!(
            tool_allowed_for_role(Some(RoleKind::Implementer), "create_instance"),
            "full-capability roles stay all-open"
        );
        assert!(
            tool_allowed_for_role(None, "create_instance"),
            "absent role_kind stays default-all-open"
        );
        assert!(
            tool_allowed_for_role(Some(RoleKind::Reviewer), "not_a_registered_tool"),
            "unknown tool names should reach the executor's existing unknown-tool error"
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

    /// #2550 P1: the folded `instance` read actions must track the LIVE per-name
    /// registry entries, not just hardcoded literals. If a future change flips
    /// `list_instances` / `pane_snapshot` to side-effect-on-timeout, this fails —
    /// forcing the fold's policy to stay in lockstep with the tool it aliases.
    #[test]
    fn folded_instance_read_actions_track_live_per_name_side_effect() {
        assert_eq!(
            side_effect_on_timeout_for("instance", Some("list")),
            side_effect_on_timeout("list_instances"),
            "instance(list) side-effect must track live list_instances"
        );
        assert_eq!(
            side_effect_on_timeout_for("instance", Some("pane_snapshot")),
            side_effect_on_timeout("pane_snapshot"),
            "instance(pane_snapshot) side-effect must track live pane_snapshot"
        );
    }

    /// #2550 P1: now that the tool is registered, the curated read roles SEE the
    /// folded `instance` tool (its read actions are their legitimate surface). The
    /// #2158 guard (`instance_role_guard_blocks_structural_for_read_roles`) still
    /// blocks the structural actions per-action — visibility here is read-only.
    #[test]
    fn read_roles_gain_folded_instance_visibility() {
        for role in [RoleKind::Reviewer, RoleKind::Planner, RoleKind::Explorer] {
            assert!(
                names(Some(role)).contains(&"instance"),
                "read role {role:?} must see the folded instance tool"
            );
        }
        // Registered → default-all-open full-capability roles see it too.
        assert!(names(None).contains(&"instance"));
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
