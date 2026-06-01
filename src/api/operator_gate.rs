//! #1339: Operator-mode authority gate — the SINGLE API-ingress enforcement
//! point. Called once in the dispatch path (`api::serve`), before any handler,
//! so it covers every direct API method AND the `mcp_tool` tunnel (which
//! carries all 36 MCP tools through one arm).
//!
//! Security invariants (pinned by tests below):
//!  1. **Operator is never locked out.** A request with no `instance` identity
//!     (operator TUI/CLI/Telegram surface) is ALWAYS allowed — a false deny of
//!     the operator is worse than an over-permissive edge.
//!  2. **Active = today's behavior** (zero migration): every operation allowed.
//!  3. The gate decodes the **inner** tool (`params["tool"]`) for the `mcp_tool`
//!     method, never the outer `"mcp_tool"` string (else all tools look alike).
//!  4. **Never-delegate** (structural / authority-changing) ops are blocked for
//!     agents in Away/Sleep — even in Sleep with a full `delegate_scope`.
//!  5. **Fail-closed**: an unmapped / newly-added op defaults to the
//!     delegate-scoped (deny-by-default in Away/Sleep) class, so taxonomy drift
//!     can never silently land a new mutation as "allowed".

use crate::operator_mode::{OperatorMode, OperatorModeState};
use serde_json::Value;

/// Authority class of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpClass {
    /// Read-only or fleet-normal (inter-agent coordination) — never gated, so
    /// the fleet keeps operating in every mode.
    AlwaysAllow,
    /// Operator-authority op a delegate MAY proxy in Sleep within
    /// `delegate_scope` (deny-by-default). Also the **fail-closed default** for
    /// any op not explicitly classified.
    DelegateScoped,
    /// Structural / authority-changing — blocked for agents in Away/Sleep with
    /// NO delegate escape (the never-delegate set).
    AbsolutelyNever,
}

/// Classify an operation by its decoded name (`op`) and optional sub-`action`
/// (for action-bearing tools like `team` / `repo` / `schedule` / `config`).
/// `op` is the inner MCP tool name, or — for direct API methods — the method
/// constant string (`spawn`/`delete`/…). Unmapped → `DelegateScoped` (fail-closed).
pub(crate) fn classify(op: &str, action: Option<&str>) -> OpClass {
    use OpClass::*;
    match op {
        // ── Read-only + fleet-normal coordination: never gated ──
        "list_instances" | "binding_state" | "tokens" | "pane_snapshot"
        | "tui_screenshot" | "gc_dry_run" | "health" | "download_attachment"
        | "inbox" | "send" | "task" | "reply" | "decision" | "set_waiting_on"
        | "interrupt" | "bind_self" | "set_description" | "set_display_name"
        | "watchdog" | "ci"
        // direct API read / normal methods
        | "list" | "inject" | "status" | "register_external"
        | "deregister_external" | "mcp_tools_list" | "set_blocked_reason"
        | "clear_blocked_reason" | "verify_push" => AlwaysAllow,

        // ── Action-bearing tools: read actions allow, mutating actions never ──
        "mode" => match action {
            Some("get") | None => AlwaysAllow,
            _ => AbsolutelyNever, // an agent must not flip operator authority
        },
        "config" => match action {
            Some("set") => AbsolutelyNever,
            _ => AlwaysAllow, // list / get
        },
        "schedule" => match action {
            Some("create") | Some("delete") => AbsolutelyNever,
            _ => AlwaysAllow, // list / update / run
        },
        "team" => match action {
            Some("create") | Some("delete") | Some("update") => AbsolutelyNever,
            _ => AlwaysAllow, // list
        },
        "deployment" => match action {
            Some("deploy") | Some("teardown") => AbsolutelyNever,
            _ => AlwaysAllow, // list
        },
        // repo mount/release/merge are all structural / merge-to-main.
        "repo" => AbsolutelyNever,

        // ── Structural lifecycle / daemon control: never-delegate ──
        "create_instance" | "delete_instance" | "replace_instance"
        | "restart_instance" | "start_instance" | "restart_daemon"
        | "force_release_worktree" | "release_worktree" | "move_pane"
        // direct API peers
        | "spawn" | "delete" | "kill" | "shutdown" | "create_team"
        | "update_team" => AbsolutelyNever,

        // ── Unknown / newly-added op → fail-closed (deny-if-structural). ──
        _ => DelegateScoped,
    }
}

/// Decide whether `method` (+ `params`) is allowed under the current
/// `state`. `Ok(())` = allowed; `Err(reason)` = denied (caller returns the
/// reason to the requester). Pure — no I/O, fully unit-testable.
pub(crate) fn check_operation_allowed(
    method: &str,
    params: &Value,
    state: &OperatorModeState,
) -> Result<(), String> {
    // Decode op + sub-action + caller identity. For the MCP tunnel the real op
    // is the INNER tool, not the outer "mcp_tool" method (invariant 3).
    let (op, action, caller) = if method == super::method::MCP_TOOL {
        (
            params.get("tool").and_then(Value::as_str).unwrap_or(""),
            params
                .get("arguments")
                .and_then(|a| a.get("action"))
                .and_then(Value::as_str),
            params.get("instance").and_then(Value::as_str).unwrap_or(""),
        )
    } else {
        (
            method,
            None,
            params.get("instance").and_then(Value::as_str).unwrap_or(""),
        )
    };

    // Invariant 1 (TOP): operator / operator-surface (no instance) → never deny.
    if caller.is_empty() {
        return Ok(());
    }
    // Invariant 2: Active = today's behavior.
    if state.mode == OperatorMode::Active {
        return Ok(());
    }

    // An agent caller while the operator is Away or Sleep.
    match classify(op, action) {
        OpClass::AlwaysAllow => Ok(()),
        OpClass::AbsolutelyNever => Err(format!(
            "operation '{op}' is blocked (never-delegate structural/authority op) \
             while the operator is {:?}",
            state.mode
        )),
        OpClass::DelegateScoped => match state.mode {
            // Active handled above; listed for exhaustiveness.
            OperatorMode::Active => Ok(()),
            OperatorMode::Away => Err(format!(
                "operation '{op}' is blocked while the operator is away \
                 (no delegate); queued for the operator"
            )),
            OperatorMode::Sleep => {
                let is_delegate = state.delegate_to.as_deref() == Some(caller);
                let in_scope = state.delegate_scope.iter().any(|s| s == op);
                if is_delegate && in_scope {
                    Ok(())
                } else {
                    Err(format!(
                        "operation '{op}' is outside the delegate scope \
                         (operator asleep; delegate={:?})",
                        state.delegate_to
                    ))
                }
            }
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sleep_with(delegate: &str, scope: &[&str]) -> OperatorModeState {
        OperatorModeState {
            mode: OperatorMode::Sleep,
            delegate_to: Some(delegate.to_string()),
            delegate_scope: scope.iter().map(|s| s.to_string()).collect(),
        }
    }
    fn away() -> OperatorModeState {
        OperatorModeState {
            mode: OperatorMode::Away,
            ..Default::default()
        }
    }
    fn mcp(tool: &str, instance: &str) -> Value {
        json!({"tool": tool, "instance": instance, "arguments": {}})
    }
    fn mcp_action(tool: &str, action: &str, instance: &str) -> Value {
        json!({"tool": tool, "instance": instance, "arguments": {"action": action}})
    }

    // ── MUST-PIN 1: operator lock-out — no-instance identity is NEVER denied ──
    #[test]
    fn operator_no_instance_is_never_denied_even_for_structural_in_sleep() {
        let st = sleep_with("fixup-lead", &[]);
        // mcp_tool create_instance with NO instance (operator surface) → allow.
        assert!(check_operation_allowed("mcp_tool", &mcp("create_instance", ""), &st).is_ok());
        // direct dangerous method, no instance (operator TUI) → allow.
        assert!(check_operation_allowed("delete", &json!({}), &st).is_ok());
        assert!(check_operation_allowed("shutdown", &json!({}), &st).is_ok());
    }

    // ── MUST-PIN 2: mcp_tool decodes the INNER tool, not the outer method ──
    #[test]
    fn mcp_tool_classifies_inner_tool_not_outer_method() {
        let st = away();
        // Inner = create_instance (structural) → denied.
        let denied = check_operation_allowed("mcp_tool", &mcp("create_instance", "dev-2"), &st);
        assert!(denied.is_err(), "inner structural tool must be denied");
        assert!(denied.unwrap_err().contains("create_instance"));
        // Inner = send (fleet-normal) → allowed even in away.
        assert!(check_operation_allowed("mcp_tool", &mcp("send", "dev-2"), &st).is_ok());
    }

    // ── MUST-PIN 3: fail-closed — unmapped op denied for agents in away/sleep ──
    #[test]
    fn unknown_op_is_fail_closed_for_agents() {
        assert!(
            check_operation_allowed("mcp_tool", &mcp("frobnicate_widgets", "dev-2"), &away())
                .is_err(),
            "an unmapped tool must default to deny (fail-closed) in away"
        );
        // ...but the operator (no instance) is still never denied.
        assert!(
            check_operation_allowed("mcp_tool", &mcp("frobnicate_widgets", ""), &away()).is_ok()
        );
    }

    // ── MUST-PIN 4: taxonomy-drift invariant — the dangerous set is AbsolutelyNever,
    // and a new/unknown op defaults deny-if-structural (DelegateScoped). ──
    #[test]
    fn taxonomy_never_delegate_set_and_drift_default() {
        for op in [
            "create_instance",
            "delete_instance",
            "replace_instance",
            "restart_instance",
            "restart_daemon",
            "force_release_worktree",
            "move_pane",
            "spawn",
            "delete",
            "kill",
            "shutdown",
            "create_team",
            "update_team",
            "repo",
        ] {
            assert_eq!(
                classify(op, None),
                OpClass::AbsolutelyNever,
                "{op} must be never-delegate"
            );
        }
        for (tool, action) in [
            ("team", "create"),
            ("team", "delete"),
            ("schedule", "create"),
            ("schedule", "delete"),
            ("config", "set"),
            ("deployment", "deploy"),
            ("mode", "set"),
        ] {
            assert_eq!(
                classify(tool, Some(action)),
                OpClass::AbsolutelyNever,
                "{tool}/{action} must be never-delegate"
            );
        }
        // New/unknown op → fail-closed default.
        assert_eq!(
            classify("some_new_tool_2099", None),
            OpClass::DelegateScoped
        );
        // Fleet-normal stays allowed.
        for op in ["send", "task", "inbox", "list_instances", "tokens"] {
            assert_eq!(classify(op, None), OpClass::AlwaysAllow, "{op}");
        }
    }

    // ── MUST-PIN 5 (gate half): never-delegate blocked even sleep+full-scope;
    // delegate-scope deny-by-default; active = passthrough. (Store reload-
    // coherence is pinned in operator_mode.rs::set_mode_then_reload_is_coherent.) ──
    #[test]
    fn never_delegate_blocked_even_sleep_full_scope() {
        // Sleep, the caller IS the delegate, scope lists the op — still blocked,
        // because create_instance is AbsolutelyNever.
        let st = sleep_with("fixup-lead", &["create_instance", "restart_daemon"]);
        assert!(
            check_operation_allowed("mcp_tool", &mcp("create_instance", "fixup-lead"), &st)
                .is_err(),
            "never-delegate op blocked even for the delegate with it in scope"
        );
    }

    #[test]
    fn delegate_scope_deny_by_default_and_in_scope_allow() {
        // An unmapped (DelegateScoped) op: allowed ONLY for the delegate with the
        // op in scope; denied otherwise.
        let st = sleep_with("fixup-lead", &["custom_op"]);
        assert!(
            check_operation_allowed("mcp_tool", &mcp("custom_op", "fixup-lead"), &st).is_ok(),
            "delegate + in-scope → allow"
        );
        assert!(
            check_operation_allowed("mcp_tool", &mcp("custom_op", "dev-2"), &st).is_err(),
            "non-delegate caller → deny even if op in scope"
        );
        assert!(
            check_operation_allowed("mcp_tool", &mcp("other_op", "fixup-lead"), &st).is_err(),
            "delegate but op not in scope → deny-by-default"
        );
    }

    #[test]
    fn active_is_passthrough_for_everything() {
        let st = OperatorModeState::default(); // Active
        assert!(
            check_operation_allowed("mcp_tool", &mcp("create_instance", "dev-2"), &st).is_ok(),
            "active mode = today's behavior, agent structural op allowed"
        );
    }

    #[test]
    fn action_bearing_read_actions_allowed_in_away() {
        // team list / schedule list / config get are read-side → allowed.
        for (tool, action) in [("team", "list"), ("schedule", "list"), ("config", "get")] {
            assert!(
                check_operation_allowed("mcp_tool", &mcp_action(tool, action, "dev-2"), &away())
                    .is_ok(),
                "{tool}/{action} read action must stay allowed in away"
            );
        }
    }
}
