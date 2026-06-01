//! #1339: Operator-mode authority gate — the SINGLE API-ingress enforcement
//! point. Called once in the dispatch path (`api::serve`), before any handler,
//! so it covers every direct API method AND the `mcp_tool` tunnel (which
//! carries all 36 MCP tools through one arm).
//!
//! ## Scope — this gate governs SOCKET-INGRESS principals only.
//! It classifies the two principals that arrive over the API socket: the
//! **operator transport** (direct methods / CLI — full authority) and the
//! **agent transport** (`mcp_tool` — gated). There is a THIRD principal it does
//! NOT (and must not) see: **daemon-autonomic self-heal** — crash-respawn
//! (`daemon::crash_respawn`), hang-recovery restart
//! (`daemon::per_tick::recovery_dispatcher`), and merged-branch worktree release
//! (`daemon::auto_release`). Those run in the per-tick daemon loop and call
//! lifecycle directly; they never traverse this socket, so they are gate-exempt
//! BY CONSTRUCTION (not by an exception here). They are trusted internal,
//! triggered only by internal events (process exit / hang detection / merge
//! detection) and are not agent-invocable — so the fleet keeps self-healing even
//! while the operator is away/asleep. See the gate-exempt markers on those entry
//! points.
//!
//! Security invariants (pinned by tests below):
//!  1. **Operator-ness is proven by TRANSPORT, never by the payload.** The agent
//!     MCP bridge only ever sends `mcp_tool`/`mcp_tools_list`; every other
//!     (direct) method is the operator's CLI surface, and the operator TUI runs
//!     in-process (never hits this socket). So a request on a direct method is
//!     the operator (full authority); a request on `mcp_tool` is an AGENT and is
//!     gated. An agent can NOT impersonate the operator by sending an empty/forged
//!     `params["instance"]` (the #1575 identity-trust bypass this closes).
//!  2. **Active = today's behavior** (zero migration): every agent op allowed.
//!  3. The gate decodes the **inner** tool (`params["tool"]`) for the `mcp_tool`
//!     method, never the outer `"mcp_tool"` string (else all tools look alike).
//!  4. **Never-delegate** (structural / authority-changing) ops are blocked for
//!     agents in Away/Sleep — even in Sleep with a full `delegate_scope`.
//!  5. **Fail-closed**: an empty/unidentified agent is the most-restricted caller
//!     (never the delegate, never the operator), and an unmapped / newly-added op
//!     defaults to the delegate-scoped (deny-by-default in Away/Sleep) class — so
//!     neither an empty `instance` nor taxonomy drift can land a mutation as
//!     "allowed".

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
    /// Structural / authority-changing — blocked for an AGENT/DELEGATE-INITIATED
    /// request (over the `mcp_tool` socket transport) in Away/Sleep, with NO
    /// delegate escape (the never-delegate set). NOTE: this label is about
    /// socket-ingress agent ops ONLY — it does NOT mean the mutation never
    /// happens in Away/Sleep: daemon-autonomic self-heal (crash-respawn,
    /// hang-recovery restart, merged-branch release) may still perform the same
    /// structural change, gate-exempt by construction (see the module scope note).
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
    // ── TRUSTED TRANSPORT discriminator (NOT the spoofable payload `instance`). ──
    // The agent MCP bridge ONLY ever sends `mcp_tool` / `mcp_tools_list`
    // (verified: `agend-mcp-bridge.rs` emits exactly those two methods). Every
    // OTHER (direct) method is the operator's CLI surface, and the operator TUI
    // executes IN-PROCESS — it never reaches this socket gate at all. So
    // operator-ness is proven by the TRANSPORT/method, and an agent can NEVER
    // claim operator authority by sending an empty/forged `params["instance"]`
    // (the #1575 identity-trust bypass this redesign closes).
    let is_agent_transport =
        method == super::method::MCP_TOOL || method == super::method::MCP_TOOLS_LIST;
    if !is_agent_transport {
        // Operator CLI surface (direct API methods) — trusted, full authority.
        return Ok(());
    }
    // `mcp_tools_list` is read-only tool enumeration — harmless, always allow.
    if method == super::method::MCP_TOOLS_LIST {
        return Ok(());
    }

    // ── Agent transport (`mcp_tool`). Decode the INNER tool + self-declared id. ──
    let op = params.get("tool").and_then(Value::as_str).unwrap_or("");
    let action = params
        .get("arguments")
        .and_then(|a| a.get("action"))
        .and_then(Value::as_str);
    // Self-declared; an agent could claim ANOTHER AGENT's name (a lesser concern —
    // the never-delegate set is blocked for every agent regardless), but it can no
    // longer claim to be the operator (that is transport-gated above).
    let caller = params.get("instance").and_then(Value::as_str).unwrap_or("");

    // An agent must NEVER change operator authority — in ANY mode, incl. Active.
    // Operator mode control lives on the operator transport (CLI/direct), not here.
    if op == "mode" && matches!(action, Some("set")) {
        return Err(
            "operation 'mode set' is operator-only (operator transport / operator-mode.json); \
             agents cannot change operator authority"
                .to_string(),
        );
    }

    // Active = today's behavior (zero migration): agents unrestricted.
    if state.mode == OperatorMode::Active {
        return Ok(());
    }

    // Agent, operator Away or Sleep.
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
                // The delegate must be a NON-EMPTY, exactly-matching instance — an
                // empty/unidentified caller is never the delegate (fail-closed).
                let is_delegate =
                    !caller.is_empty() && state.delegate_to.as_deref() == Some(caller);
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

/// #1339: handler for the operator-only `MODE` direct method. Reached only via
/// the operator transport (a direct API method; the gate lets direct methods
/// through as the operator surface, and agents can only send `mcp_tool`). This
/// is the AUTHORITATIVE mode-set path: an agent overwriting `operator-mode.json`
/// is governed by the same operator-owned-config convention as
/// `runtime-config.json` / `fleet.yaml` (raw-FS integrity is a fleet-wide
/// follow-up, out of #1339 scope).
pub(crate) fn handle_mode_set(params: &Value, home: &std::path::Path) -> Value {
    use serde_json::json;
    let Some(mode_str) = params.get("mode").and_then(Value::as_str) else {
        return json!({"ok": false, "error": "mode requires `mode` (active|away|sleep)"});
    };
    let mode = match crate::operator_mode::parse_mode(mode_str) {
        Ok(m) => m,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let delegate_to = params
        .get("delegate")
        .and_then(Value::as_str)
        .map(str::to_string);
    let delegate_scope: Vec<String> = params
        .get("scope")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    match crate::operator_mode::set_mode(home, mode, delegate_to, delegate_scope) {
        Ok(s) => json!({
            "ok": true,
            "mode": s.mode,
            "delegate_to": s.delegate_to,
            "delegate_scope": s.delegate_scope,
        }),
        Err(e) => json!({"ok": false, "error": e}),
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

    // ── MUST-PIN 1a: operator-ness from TRANSPORT — direct methods are the
    // operator CLI surface and stay allowed (operator never locked out). ──
    #[test]
    fn operator_direct_method_always_allowed_even_in_sleep() {
        let st = sleep_with("fixup-lead", &[]);
        // Direct API methods (operator CLI surface) → allowed, even structural.
        assert!(check_operation_allowed("delete", &json!({}), &st).is_ok());
        assert!(check_operation_allowed("spawn", &json!({}), &st).is_ok());
        assert!(check_operation_allowed("shutdown", &json!({}), &st).is_ok());
        // mcp_tools_list (read-only enumeration) → allowed.
        assert!(check_operation_allowed("mcp_tools_list", &json!({}), &st).is_ok());
    }

    // ── MUST-PIN 1b (THE #1575 must-fix): an agent CANNOT impersonate the
    // operator by sending an empty/forged `instance` on the mcp_tool transport.
    // The reviewed exploit: {tool: restart_instance, instance:"", args:{name:victim}}. ──
    #[test]
    fn agent_empty_instance_cannot_impersonate_operator() {
        for st in [away(), sleep_with("fixup-lead", &[])] {
            let exploit = json!({
                "tool": "restart_instance",
                "instance": "",
                "arguments": {"name": "victim-agent"}
            });
            let denied = check_operation_allowed("mcp_tool", &exploit, &st);
            assert!(
                denied.is_err(),
                "empty-instance agent must NOT be treated as operator ({:?})",
                st.mode
            );
        }
        // ...but in Active (today's behavior) an agent op is still a passthrough.
        let active = OperatorModeState::default();
        assert!(check_operation_allowed(
            "mcp_tool",
            &json!({"tool": "restart_instance", "instance": "", "arguments": {}}),
            &active
        )
        .is_ok());
    }

    // ── An agent must never change operator authority (mode set), in ANY mode. ──
    #[test]
    fn agent_cannot_set_operator_mode() {
        for st in [
            OperatorModeState::default(), // Active
            away(),
            sleep_with("dev-2", &["mode"]), // even with mode in its own scope
        ] {
            let denied =
                check_operation_allowed("mcp_tool", &mcp_action("mode", "set", "dev-2"), &st);
            assert!(
                denied.is_err(),
                "agent mode-set must be denied in {:?}",
                st.mode
            );
        }
        // Reading the mode is fine for agents.
        assert!(
            check_operation_allowed("mcp_tool", &mcp_action("mode", "get", "dev-2"), &away())
                .is_ok()
        );
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

    // ── MUST-PIN 3: fail-closed — unmapped op AND empty-instance agent denied ──
    #[test]
    fn unknown_op_and_empty_instance_are_fail_closed_for_agents() {
        // Unmapped tool from a named agent → deny (fail-closed) in away.
        assert!(
            check_operation_allowed("mcp_tool", &mcp("frobnicate_widgets", "dev-2"), &away())
                .is_err(),
            "an unmapped tool must default to deny (fail-closed) in away"
        );
        // Empty-instance agent (unidentified) → deny on the agent transport — it is
        // NOT the operator (that is transport-gated) and NOT a delegate.
        assert!(
            check_operation_allowed("mcp_tool", &mcp("frobnicate_widgets", ""), &away()).is_err(),
            "empty-instance agent must be fail-closed, never elevated"
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

    // ── mode-control: the operator's `MODE` direct method passes the gate (it is
    // the operator transport — NOT mcp_tool), even in sleep, with NO instance. ──
    #[test]
    fn operator_direct_mode_method_passes_gate_in_any_mode() {
        let params = json!({"mode": "active"});
        for st in [
            OperatorModeState::default(),
            away(),
            sleep_with("fixup-lead", &[]),
        ] {
            assert!(
                check_operation_allowed(super::super::method::MODE, &params, &st).is_ok(),
                "direct MODE method is operator transport → allowed in {:?}",
                st.mode
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn handle_mode_set_persists_and_updates_global() {
        let dir = std::env::temp_dir().join(format!("agend-modeset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let resp = handle_mode_set(
            &json!({"mode": "sleep", "delegate": "fixup-lead", "scope": ["custom_op"]}),
            &dir,
        );
        assert_eq!(resp["ok"], json!(true));
        // The global (what the gate reads) reflects it immediately.
        let s = crate::operator_mode::get();
        assert_eq!(s.mode, OperatorMode::Sleep);
        assert_eq!(s.delegate_to.as_deref(), Some("fixup-lead"));
        // Bad mode string → error, no change.
        assert_eq!(
            handle_mode_set(&json!({"mode": "dnd"}), &dir)["ok"],
            json!(false)
        );
        // Reset global so we don't leak Sleep into other tests.
        crate::operator_mode::set_mode(&dir, OperatorMode::Active, None, vec![]).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ② invariant: the daemon-autonomic self-heal paths are gate-exempt BY
    // CONSTRUCTION — they must NOT reference the gate, and must carry the marker. ──
    #[test]
    fn autonomic_paths_are_gate_exempt_by_construction() {
        let root = env!("CARGO_MANIFEST_DIR");
        for rel in [
            "src/daemon/crash_respawn.rs",
            "src/daemon/auto_release.rs",
            "src/daemon/per_tick/recovery_dispatcher.rs",
        ] {
            let src = std::fs::read_to_string(format!("{root}/{rel}")).expect(rel);
            assert!(
                !src.contains("check_operation_allowed"),
                "{rel} must NOT call the operator-mode gate (gate-exempt by construction)"
            );
            assert!(
                src.contains("DAEMON-AUTONOMIC, GATE-EXEMPT BY DESIGN"),
                "{rel} must carry the #1339 gate-exempt marker"
            );
        }
    }
}
