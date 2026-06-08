//! MCP tool proxy handler — daemon-side dispatch for MCP tool calls.
//!
//! Sprint 25 P0 Option F: the MCP subprocess is a zero-state stdio↔TCP
//! relay. All tool calls arrive here via the daemon API and dispatch
//! through the existing [`crate::mcp::handlers::handle_tool`].
//!
//! Sprint 25 P1 F1: per-tool timeout overrides + request budget enforcement.

use serde_json::{json, Value};
use std::time::Duration;

use super::HandlerCtx;

/// Per-tool timeout in milliseconds. Fast read-only tools get a short
/// timeout; slow spawn/deploy tools get a longer one.
///
/// Made `pub(crate)` for `request_dedup::method_wait_timeout` to reuse
/// the same mapping when the API method is `mcp_tool` — shared source
/// of truth keeps the dispatch budget and the dedup wait budget in lock-step.
pub(crate) fn tool_timeout(tool: &str) -> Duration {
    match tool {
        // Fast read-only tools (~5s)
        "inbox" | "describe_message" | "describe_thread" | "list_instances"
        | "describe_instance" | "list_teams" | "list_decisions" | "list_schedules"
        | "list_deployments" | "set_waiting_on" | "set_display_name" | "set_description"
        | "react" | "report_health" => Duration::from_secs(5),

        // Slow tools that spawn processes or do network I/O (~60s).
        // #1814: restart_daemon (self-respawn) spawns a successor + runs a
        // ≤30s Phase-1 health gate before returning — give it the 60s budget so
        // the gate can't collide with the default 30s tool timeout.
        "create_instance" | "deploy_template" | "replace_instance" | "watch_ci"
        | "checkout_repo" | "restart_daemon" => Duration::from_secs(60),

        // Default for everything else (~30s)
        _ => Duration::from_secs(30),
    }
}

/// R3#1 candidate 2 (timeout-IN_PROGRESS): does a TIMEOUT on this tool need to
/// be hidden from the agent as "accepted, completing in background" rather than
/// surfaced as a retryable error?
///
/// **Why**: on timeout the execution thread is NOT killed — it runs to
/// completion in the background (see `handle_mcp_tool_inner`). If the timeout
/// surfaces as `{ok:false, error:"timed out"}`, the agent (Claude Code) treats
/// it as a failure and RE-ISSUES the tool call (a fresh logical call with a new
/// bridge `request_id`, which `request_dedup` cannot catch) → the action fires
/// TWICE. For a non-idempotent side-effect tool (send / reply / task-create /
/// decision-post / spawn …) that is a duplicate message / task / spawn — the
/// retry-storm root cause. Returning `accepted_in_progress` instead tells the
/// agent the action was taken (once) and NOT to resend.
///
/// The list below is the SAFE set — read-only or idempotent tools where a
/// repeat is harmless AND the agent genuinely needs the real result, so they
/// keep the retryable error. Everything else (incl. NEW tools and multi-action
/// tools with any mutating action) defaults to side-effect — the conservative
/// choice: a lost read result is recoverable, a duplicated action is the bug.
///
/// **Defense-in-depth (candidate 3, free)**: several side-effect tools ALSO have
/// handler-side natural idempotency keys, so even a model that resends despite
/// `accepted_in_progress` is protected: `task` done/update/claim are keyed on
/// `task_id` (a repeat is an illegal-transition no-op), and `create_instance`
/// is keyed on the instance name (a repeat errors, no second spawn). `send` /
/// `reply` / `decision`-post have NO natural key — they rely on this gate.
fn is_side_effect_tool(tool: &str) -> bool {
    !matches!(
        tool,
        // Pure reads — agent needs the result, repeat is harmless.
        "inbox"
            | "list_instances"
            | "binding_state"
            | "gc_dry_run"
            | "mode"
            | "tokens"
            | "pane_snapshot"
            | "tui_screenshot"
            | "download_attachment"
            // Idempotent mutations — a repeat converges to the same state.
            | "set_waiting_on"
            | "set_display_name"
            | "set_description"
            | "health"
            | "bind_self"
            | "release_worktree"
            | "force_release_worktree"
            | "ci"
    )
}

/// Stable hash of `(tool, args)` for the timeout instrument. NOT used for any
/// dedup decision — purely so a post-restart log grep can tell whether the SAME
/// logical call reappears in the timeout probe (⇒ the agent retried despite
/// `accepted_in_progress`), the empirical validation of candidate 2.
fn content_key(tool: &str, args: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tool.hash(&mut h);
    args.to_string().hash(&mut h);
    h.finish()
}

/// Build the response for a tool that exceeded its timeout budget. Side-effect
/// tools → `accepted_in_progress` (don't resend); read/idempotent tools → the
/// retryable `timed out` error. Emits the `#r3-1-timeout-probe` instrument.
fn timeout_response(tool: &str, timeout: Duration, content_key: u64) -> Value {
    let side_effect = is_side_effect_tool(tool);
    // #r3-1 instrument ("doubt → add log"): this PR's effect (agent stops
    // retrying) is only observable after a daemon restart (self-dogfood
    // anti-pattern), so log every timeout with a stable content_key. If the
    // same content_key reappears in this probe after restart, the agent retried
    // anyway → candidate 2 insufficient (escalate to candidate 3 / schema key).
    tracing::warn!(
        target: "mcp_timeout",
        marker = "#r3-1-timeout-probe",
        tool,
        ?timeout,
        side_effect,
        content_key,
        "mcp_tool timed out — returning {}",
        if side_effect {
            "accepted_in_progress (agent must NOT resend)"
        } else {
            "error (retry-safe)"
        }
    );
    if side_effect {
        json!({
            "ok": true,
            "status": "accepted_in_progress",
            "note": format!(
                "'{tool}' was accepted and is completing in the background; its side effect \
                 will occur exactly once. Do NOT call it again — a repeat would duplicate the action."
            )
        })
    } else {
        json!({"ok": false, "error": format!("tool '{tool}' timed out after {timeout:?}")})
    }
}

/// Handle `mcp_tool` API method: proxy a tool call through the daemon
/// where process-global state (ACTIVE_CHANNEL, heartbeat_pair, etc.)
/// is available. Applies per-tool timeout.
pub(crate) fn handle_mcp_tool(params: &Value, _ctx: &HandlerCtx) -> Value {
    let tool = match params["tool"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => return json!({"ok": false, "error": "missing 'tool' parameter"}),
    };
    let args = params["arguments"].clone();
    let instance = params["instance"].as_str().unwrap_or("").to_string();
    let timeout = tool_timeout(tool);
    handle_mcp_tool_inner(tool, args, instance, timeout, crate::mcp::execute_tool)
}

/// Inner proxy with an injectable executor + explicit timeout — the §3.9 test
/// seam (drive the timeout path with a sleeping executor + tiny budget, no real
/// tool and no 30s wait).
fn handle_mcp_tool_inner(
    tool: &str,
    args: Value,
    instance: String,
    timeout: Duration,
    exec: impl FnOnce(&str, &Value, &str) -> Value + Send + 'static,
) -> Value {
    let key = content_key(tool, &args);
    let tool_owned = tool.to_string();

    // Execute in a scoped thread with per-tool timeout. This prevents a stuck
    // tool from blocking the API session thread beyond the tool's budget.
    // fire-and-forget: short-lived tool execution thread; dies on completion
    // or timeout. Result sent via mpsc channel; thread is not joined — the
    // recv_timeout below bounds the caller's wait. If the tool outlives the
    // timeout, the thread runs to completion in the background (no leak —
    // handle_tool is stateless and the thread exits when done). Candidate 2
    // relies on this: the side effect DOES complete once, so a timeout is
    // truthfully "accepted_in_progress", not a failure.
    let exec_tool = tool_owned.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::Builder::new()
        .name(format!("mcp_tool_{tool_owned}"))
        .spawn(move || {
            let result = exec(&exec_tool, &args, &instance);
            let _ = tx.send(result);
        });

    match handle {
        Ok(_) => match rx.recv_timeout(timeout) {
            Ok(result) => json!({"ok": true, "result": result}),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => timeout_response(tool, timeout, key),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                json!({"ok": false, "error": format!("tool '{tool}' thread panicked")})
            }
        },
        Err(e) => json!({"ok": false, "error": format!("spawn failed: {e}")}),
    }
}

/// Handle `mcp_tools_list` API method: return the tool definitions.
pub(crate) fn handle_mcp_tools_list(_params: &Value, _ctx: &HandlerCtx) -> Value {
    json!({"ok": true, "result": crate::mcp::tools::tool_definitions()})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_tools_get_short_timeout() {
        assert_eq!(tool_timeout("inbox"), Duration::from_secs(5));
        assert_eq!(tool_timeout("list_instances"), Duration::from_secs(5));
        assert_eq!(tool_timeout("describe_message"), Duration::from_secs(5));
    }

    #[test]
    fn slow_tools_get_long_timeout() {
        assert_eq!(tool_timeout("create_instance"), Duration::from_secs(60));
        assert_eq!(tool_timeout("deploy_template"), Duration::from_secs(60));
    }

    #[test]
    fn default_tools_get_30s() {
        assert_eq!(tool_timeout("send_to_instance"), Duration::from_secs(30));
        assert_eq!(tool_timeout("delegate_task"), Duration::from_secs(30));
        assert_eq!(tool_timeout("unknown_tool"), Duration::from_secs(30));
    }

    // ── R3#1 candidate 2: timeout → accepted_in_progress for side-effect tools ──

    #[test]
    fn side_effect_classifier_covers_the_storm_offenders_and_reads() {
        // Non-idempotent side-effect tools (the retry-storm offenders) → true.
        for t in [
            "send",
            "reply",
            "interrupt",
            "decision",
            "task",
            "team",
            "schedule",
            "deployment",
            "create_instance",
            "delete_instance",
            "replace_instance",
            "restart_instance",
            "start_instance",
            "repo",
            "restart_daemon",
        ] {
            assert!(is_side_effect_tool(t), "{t} must be treated as side-effect");
        }
        // Read / idempotent tools → false (keep retryable error).
        for t in [
            "inbox",
            "list_instances",
            "pane_snapshot",
            "tokens",
            "mode",
            "binding_state",
            "gc_dry_run",
            "tui_screenshot",
            "download_attachment",
            "set_waiting_on",
            "set_display_name",
            "set_description",
            "health",
            "bind_self",
            "release_worktree",
            "force_release_worktree",
            "ci",
        ] {
            assert!(!is_side_effect_tool(t), "{t} must stay retry-safe");
        }
        // NEW / unknown tools default to side-effect (conservative).
        assert!(
            is_side_effect_tool("some_future_tool"),
            "unknown defaults to side-effect"
        );
    }

    /// §3.9: a side-effect tool that exceeds its timeout returns
    /// `accepted_in_progress` (ok:true, NO error) so the agent does not resend
    /// and double-fire. Drives the real `handle_mcp_tool_inner` entry with an
    /// injected sleeping executor + tiny budget (no real tool, no 30s wait).
    #[test]
    fn side_effect_tool_timeout_returns_accepted_in_progress() {
        let slow = |_: &str, _: &Value, _: &str| {
            std::thread::sleep(Duration::from_millis(300));
            json!({"sent": true})
        };
        let resp = handle_mcp_tool_inner(
            "send",
            json!({"instance": "x", "message": "hi"}),
            "caller".to_string(),
            Duration::from_millis(20),
            slow,
        );
        assert_eq!(
            resp["ok"], true,
            "side-effect timeout must NOT look like failure (else agent retries): {resp}"
        );
        assert_eq!(resp["status"], "accepted_in_progress", "got {resp}");
        assert!(
            resp.get("error").is_none(),
            "no error field → agent will not resend: {resp}"
        );
    }

    /// §3.9: a read/idempotent tool that times out keeps the retryable `error`
    /// (the agent needs the real result and a repeat is harmless).
    #[test]
    fn read_tool_timeout_returns_error() {
        let slow = |_: &str, _: &Value, _: &str| {
            std::thread::sleep(Duration::from_millis(300));
            json!({})
        };
        let resp = handle_mcp_tool_inner(
            "inbox",
            json!({}),
            "caller".to_string(),
            Duration::from_millis(20),
            slow,
        );
        assert_eq!(
            resp["ok"], false,
            "read tool timeout stays a retryable error: {resp}"
        );
        assert!(
            resp["error"].as_str().unwrap_or("").contains("timed out"),
            "got {resp}"
        );
    }

    /// A tool that COMPLETES within budget returns its real result regardless of
    /// classification — the accepted_in_progress path is timeout-only.
    #[test]
    fn completed_side_effect_tool_returns_real_result() {
        let fast = |_: &str, _: &Value, _: &str| json!({"done": 1});
        let resp = handle_mcp_tool_inner(
            "send",
            json!({}),
            "caller".to_string(),
            Duration::from_secs(5),
            fast,
        );
        assert_eq!(resp["ok"], true);
        assert_eq!(
            resp["result"]["done"], 1,
            "a completed side-effect tool returns its result, not accepted_in_progress: {resp}"
        );
        assert!(
            resp.get("status").is_none(),
            "no accepted_in_progress on success: {resp}"
        );
    }
}
