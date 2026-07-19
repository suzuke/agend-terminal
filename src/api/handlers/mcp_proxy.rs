//! MCP tool proxy handler — daemon-side dispatch for MCP tool calls.
//!
//! Sprint 25 P0 Option F: the MCP subprocess is a zero-state stdio↔TCP
//! relay. All tool calls arrive here via the daemon API and dispatch
//! through the existing [`crate::mcp::handlers::handle_tool`].
//!
//! Sprint 25 P1 F1: per-tool timeout overrides + request budget enforcement.

use serde_json::{json, Value};
use std::{path::Path, time::Duration};

use super::HandlerCtx;

/// Per-tool dispatch timeout bands. Fast read-only / atomic-flip tools get a
/// short budget; tools with a process-spawning / network action get the long
/// upper-bound budget. Per-tool granularity (no per-action timeout — YAGNI):
/// a multi-action tool like `repo` or `deployment` takes its SLOWEST action's
/// band, which is a harmless upper bound for its fast actions.
///
/// The band constants are `pub(crate)` so `request_dedup::method_wait_timeout`
/// reuses them — one source of truth keeps the dispatch budget and the dedup
/// wait budget in lock-step.
pub(crate) const FAST_TOOL_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const SLOW_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn tool_timeout(tool: &str) -> Duration {
    match crate::mcp::registry::timeout_class(tool) {
        crate::mcp::registry::ToolTimeoutClass::Fast => FAST_TOOL_TIMEOUT,
        crate::mcp::registry::ToolTimeoutClass::Default => DEFAULT_TOOL_TIMEOUT,
        crate::mcp::registry::ToolTimeoutClass::Slow => SLOW_TOOL_TIMEOUT,
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
fn is_side_effect_tool(tool: &str, action: Option<&str>) -> bool {
    crate::mcp::registry::side_effect_on_timeout_for(tool, action)
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
fn timeout_response(
    tool: &str,
    action: Option<&str>,
    timeout: Duration,
    content_key: u64,
) -> Value {
    let side_effect = is_side_effect_tool(tool, action);
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
pub(crate) fn handle_mcp_tool(params: &Value, ctx: &HandlerCtx) -> Value {
    let tool = match params["tool"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => return json!({"ok": false, "error": "missing 'tool' parameter"}),
    };
    let args = params["arguments"].clone();
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .map(String::from);
    let instance = params["instance"].as_str().unwrap_or("").to_string();
    let role_kind = match role_kind_for_instance(ctx.home, &instance, "tool call") {
        Ok(role_kind) => role_kind,
        Err(resp) => return resp,
    };
    if !crate::mcp::registry::tool_allowed_for_role_action(role_kind, tool, action.as_deref()) {
        return json!({
            "ok": false,
            "error": format!(
                "tool call: '{tool}' is not allowed for instance '{instance}' with role_kind {role_kind:?}"
            ),
        });
    }
    let timeout = tool_timeout(tool);
    let runtime = crate::mcp::handlers::dispatch::RuntimeContext {
        registry: ctx.registry.clone(),
        configs: ctx.configs.clone(),
        externals: ctx.externals.clone(),
        capability: ctx.capability,
        app_restart: ctx.app_restart.cloned(),
        // #2453 R2 flush barrier: carry THIS request's slot so `restart_daemon` can
        // register its commit-permission ack, run by `handle_session` after flush.
        post_flush: Some(ctx.post_flush.clone()),
        notifier: ctx.notifier.cloned(),
        shutdown: ctx.shutdown.clone(),
    };
    handle_mcp_tool_inner(
        tool,
        args,
        instance,
        timeout,
        action,
        move |tool, args, instance| {
            crate::mcp::execute_tool_with_runtime(tool, args, instance, runtime)
        },
    )
}

/// Inner proxy with an injectable executor + explicit timeout — the §3.9 test
/// seam (drive the timeout path with a sleeping executor + tiny budget, no real
/// tool and no 30s wait).
fn handle_mcp_tool_inner(
    tool: &str,
    args: Value,
    instance: String,
    timeout: Duration,
    action: Option<String>,
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
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                timeout_response(tool, action.as_deref(), timeout, key)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                json!({"ok": false, "error": format!("tool '{tool}' thread panicked")})
            }
        },
        Err(e) => json!({"ok": false, "error": format!("spawn failed: {e}")}),
    }
}

/// Handle `mcp_tools_list` API method: return the tool definitions VISIBLE to
/// the calling agent's role (#2300 P0).
///
/// The bridge passes the caller's `instance` in params (mirroring the tool-call
/// path); we resolve its fleet `role` and subset the surface via
/// [`crate::mcp::tools::tool_definitions_for_role`]. Default-all-open:
/// no instance (old bridge / non-agent caller), unknown instance, or a role not
/// in the capability registry (dev / lead / orchestrator / …) → the full surface.
pub(crate) fn handle_mcp_tools_list(params: &Value, ctx: &HandlerCtx) -> Value {
    // #2344: subset off the typed `role_kind` (operator-declared), NOT the
    // free-text `role` description — the old exact-match against the prose role
    // never hit, so every agent saw the full tool surface.
    let inst = params
        .get("instance")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let role_kind = match role_kind_for_instance(ctx.home, inst, "tools/list") {
        Ok(role_kind) => role_kind,
        Err(resp) => return resp,
    };
    // Default-all-open hot path: no instance / no fleet.yaml / undeclared role_kind
    // → the canonical full surface (also keeps `tool_definitions` the single
    // unfiltered builder the count-invariant pins). A role only narrows via the
    // capability registry.
    let result = match role_kind {
        None => crate::mcp::tools::tool_definitions(),
        Some(rk) => crate::mcp::tools::tool_definitions_for_role(Some(rk)),
    };
    json!({ "ok": true, "result": result })
}

/// #2300 P1 / #2055: resolve the caller's typed role once for both advisory
/// visibility (`tools/list`) and execution-time hard-deny (`tool call`).
///
/// Default-all-open stays deliberate: no instance, no fleet.yaml, unknown
/// instance, or absent role_kind all return `Ok(None)`. A PRESENT but malformed
/// fleet.yaml fails closed so a broken policy file cannot silently widen tools.
fn role_kind_for_instance(
    home: &Path,
    instance: &str,
    surface: &str,
) -> Result<Option<crate::fleet::RoleKind>, Value> {
    if instance.is_empty() {
        return Ok(None);
    }

    // #2344 (r6 #2367 reject + lead nuance): distinguish a MISSING fleet.yaml
    // from a present-but-malformed one. The old `FleetConfig::load(..).ok()`
    // collapsed BOTH to `None` → the full tool surface, which let a typo'd
    // `role_kind: supervisor` FAIL strict parse yet still advertise every tool
    // — the D2 strict-deny hole r6 found.
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    if !fleet_path.exists() {
        // No fleet.yaml at all — a common setup (dev / fresh / local shell).
        // Stay ALL-OPEN; a blanket fail-closed would regress no-fleet daemons.
        return Ok(None);
    }

    match crate::fleet::FleetConfig::load(&fleet_path) {
        // Present but FAILS to load (parse error / malformed role_kind) → FAIL
        // CLOSED. Surface the error loudly (operator fixes the config); never
        // silently widen the advertised or executable surface.
        Err(e) => Err(json!({
            "ok": false,
            "error": format!(
                "{surface}: fleet.yaml is present but failed to load \
                 (refusing to use the full tool surface — fix the config): {e:#}"
            ),
        })),
        // Loaded OK → the instance's role_kind. Unknown instance OR absent
        // role_kind → all-open (the unchanged opt-in contract).
        Ok(fleet) => Ok(fleet.instances.get(instance).and_then(|i| i.role_kind)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_tools_get_short_timeout() {
        assert_eq!(tool_timeout("inbox"), Duration::from_secs(5));
        assert_eq!(tool_timeout("list_instances"), Duration::from_secs(5));
        assert_eq!(tool_timeout("health"), Duration::from_secs(5));
    }

    #[test]
    fn slow_tools_get_long_timeout() {
        assert_eq!(tool_timeout("create_instance"), Duration::from_secs(60));
        // #2050 W1.3①: the action-consolidated successors of the old stale
        // names (deploy_template / watch_ci / checkout_repo) — these were the
        // false-timeout bug (long ops silently getting the 30s default).
        assert_eq!(tool_timeout("deployment"), Duration::from_secs(60));
        assert_eq!(tool_timeout("ci"), Duration::from_secs(60));
        assert_eq!(tool_timeout("repo"), Duration::from_secs(60));
    }

    #[test]
    fn default_tools_get_30s() {
        assert_eq!(tool_timeout("send"), Duration::from_secs(30));
        assert_eq!(tool_timeout("task"), Duration::from_secs(30));
        assert_eq!(tool_timeout("unknown_tool"), Duration::from_secs(30));
    }

    /// #2050 W1.3① / P1 single-source follow-up: timeout classification is now
    /// stored on the registered `ToolEntry` itself, so the old stale-name class
    /// (FAST/SLOW lists naming removed tools) is structurally impossible. Keep a
    /// lightweight invariant that every registered entry resolves through
    /// `tool_timeout` without panicking.
    #[test]
    fn every_registered_tool_has_timeout_class() {
        for entry in crate::mcp::registry::all() {
            let timeout = tool_timeout(entry.name);
            assert!(
                [FAST_TOOL_TIMEOUT, DEFAULT_TOOL_TIMEOUT, SLOW_TOOL_TIMEOUT].contains(&timeout),
                "registered tool '{}' resolved to unexpected timeout {timeout:?}",
                entry.name
            );
        }
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
            "restart_instance",
            "start_instance",
            "repo",
            "restart_daemon",
        ] {
            assert!(
                is_side_effect_tool(t, None),
                "{t} must be treated as side-effect"
            );
        }
        // Read / idempotent tools → false (keep retryable error).
        for t in [
            "inbox",
            "list_instances",
            "pane_snapshot",
            "binding_state",
            "download_attachment",
            "set_waiting_on",
            "set_metadata",
            "health",
            "bind_self",
            "release_worktree",
            "ci",
        ] {
            assert!(!is_side_effect_tool(t, None), "{t} must stay retry-safe");
        }
        // NEW / unknown tools default to side-effect (conservative).
        assert!(
            is_side_effect_tool("some_future_tool", None),
            "unknown defaults to side-effect"
        );

        // #2550 P0: the folded `instance` tool is per-action (dormant until P1):
        // structural actions stay side-effect (no double delete/restart on a
        // timed-out call), read actions stay retry-safe (the real result is
        // needed). Byte-equivalent to today's per-name tools.
        for a in [
            "delete",
            "restart",
            "start",
            "move_pane",
            "bind_topic",
            "interrupt",
        ] {
            assert!(
                is_side_effect_tool("instance", Some(a)),
                "instance(action={a}) must be side-effect"
            );
        }
        for a in ["list", "pane_snapshot", "set_waiting_on"] {
            assert!(
                !is_side_effect_tool("instance", Some(a)),
                "instance(action={a}) must stay retry-safe"
            );
        }
        assert!(
            is_side_effect_tool("instance", None),
            "instance with no action fail-closes to side-effect"
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
            None,
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
            None,
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
            None,
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

    /// #2344 e2e: `handle_mcp_tools_list` subsets the surface for a fleet instance
    /// whose `role_kind` is a read/report role — the live path the MCP bridge hits.
    /// A reviewer sees fewer than the full registry; no instance → all-open.
    #[test]
    fn tools_list_subsets_for_role_kind_reviewer() {
        let dir =
            std::env::temp_dir().join(format!("agend-mcp-toolslist-rk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("fleet.yaml"),
            "instances:\n  rev:\n    role: \"Code reviewer\"\n    role_kind: reviewer\n    command: claude\n",
        )
        .expect("write fleet.yaml");

        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();
        let ctx = HandlerCtx {
            registry: &registry,
            configs: &configs,
            externals: &externals,
            notifier: None,
            home: &dir,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: crate::api::app_restart::PostFlushSlot::new(),
            shutdown: None,
        };

        let full = crate::mcp::tools::tool_definitions()["tools"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);

        // role_kind=reviewer → subsetted surface.
        let resp = handle_mcp_tools_list(&json!({"instance": "rev"}), &ctx);
        assert_eq!(resp["ok"], true);
        let n = resp["result"]["tools"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        assert!(
            n > 0 && n < full,
            "tools_list for role_kind=reviewer must be subsetted ({n} < {full})"
        );

        // No instance → all-open (the default hot path).
        let resp_full = handle_mcp_tools_list(&json!({}), &ctx);
        let n_full = resp_full["result"]["tools"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        assert_eq!(n_full, full, "no instance → all-open full surface");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn toollist_ctx_home(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("agend-mcp-toollist-{tag}-{}", std::process::id()))
    }

    /// #2300 P1 / #2055: `tools/list` subsetting is not a security boundary by
    /// itself. A restricted role must also be denied if it directly calls a
    /// registered lifecycle/orchestration tool through the MCP proxy.
    #[test]
    fn tool_call_denies_hidden_tool_for_role_kind_reviewer() {
        let dir = toollist_ctx_home("call-deny");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("fleet.yaml"),
            "instances:\n  rev:\n    role_kind: reviewer\n    command: claude\n",
        )
        .expect("write fleet.yaml");

        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();
        let ctx = HandlerCtx {
            registry: &registry,
            configs: &configs,
            externals: &externals,
            notifier: None,
            home: &dir,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: crate::api::app_restart::PostFlushSlot::new(),
            shutdown: None,
        };

        let resp = handle_mcp_tool(
            &json!({
                "instance": "rev",
                "tool": "create_instance",
                "arguments": {}
            }),
            &ctx,
        );
        assert_eq!(
            resp["ok"], false,
            "hidden registered tool must fail: {resp}"
        );
        let err = resp["error"].as_str().unwrap_or("");
        assert!(
            err.contains("create_instance") && err.contains("Reviewer"),
            "denial should name the denied tool and typed role, got: {err}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Same fail-closed policy as `tools/list`: if fleet.yaml is present but
    /// malformed, a tool call must not silently widen to full capability.
    #[test]
    fn tool_call_fails_closed_on_malformed_role_kind() {
        let dir = toollist_ctx_home("call-bad");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("fleet.yaml"),
            "instances:\n  bad:\n    role_kind: supervisor\n    command: claude\n",
        )
        .expect("write fleet.yaml");

        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();
        let ctx = HandlerCtx {
            registry: &registry,
            configs: &configs,
            externals: &externals,
            notifier: None,
            home: &dir,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: crate::api::app_restart::PostFlushSlot::new(),
            shutdown: None,
        };

        let resp = handle_mcp_tool(
            &json!({
                "instance": "bad",
                "tool": "inbox",
                "arguments": {}
            }),
            &ctx,
        );
        assert_eq!(
            resp["ok"], false,
            "present-but-malformed fleet.yaml must fail closed, got: {resp}"
        );
        let err = resp["error"].as_str().unwrap_or("");
        assert!(
            err.contains("supervisor") || err.contains("role_kind") || err.contains("fleet"),
            "error must explain the failure (bad value / role_kind / fleet), got: {err}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// #2344 D2 STRICT on the live bridge path (r6 #2367 reject): a fleet.yaml that
    /// is PRESENT but malformed (e.g. an unknown `role_kind`) must FAIL CLOSED in
    /// `handle_mcp_tools_list` — NOT silently fall back to the full tool surface
    /// (the old `.ok()` swallowed the parse error). Returns `ok: false` + an
    /// explanatory error, no `result.tools`, and does not panic.
    #[test]
    fn tools_list_fails_closed_on_malformed_role_kind() {
        let dir = toollist_ctx_home("bad");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("fleet.yaml"),
            "instances:\n  bad:\n    role_kind: supervisor\n    command: claude\n",
        )
        .expect("write fleet.yaml");

        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();
        let ctx = HandlerCtx {
            registry: &registry,
            configs: &configs,
            externals: &externals,
            notifier: None,
            home: &dir,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: crate::api::app_restart::PostFlushSlot::new(),
            shutdown: None,
        };

        let resp = handle_mcp_tools_list(&json!({"instance": "bad"}), &ctx);
        assert_eq!(
            resp["ok"], false,
            "present-but-malformed fleet.yaml must fail closed, got: {resp}"
        );
        let err = resp["error"].as_str().unwrap_or("");
        assert!(
            err.contains("supervisor") || err.contains("role_kind") || err.contains("fleet"),
            "error must explain the failure (bad value / role_kind / fleet), got: {err}"
        );
        assert!(
            resp.get("result").and_then(|r| r.get("tools")).is_none(),
            "fail-closed must NOT return a full tool list, got: {resp}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// #2344 (lead nuance): a MISSING fleet.yaml must stay ALL-OPEN — a no-fleet
    /// daemon (dev / fresh / local-shell setups) must NOT be fail-closed just
    /// because an instance name is passed. Only a PRESENT-but-malformed fleet.yaml
    /// fails closed (above); absence is not an error.
    #[test]
    fn tools_list_all_open_when_no_fleet_yaml() {
        let dir = toollist_ctx_home("nofleet");
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Deliberately NO fleet.yaml written.

        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();
        let ctx = HandlerCtx {
            registry: &registry,
            configs: &configs,
            externals: &externals,
            notifier: None,
            home: &dir,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: crate::api::app_restart::PostFlushSlot::new(),
            shutdown: None,
        };

        let full = crate::mcp::tools::tool_definitions()["tools"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        let resp = handle_mcp_tools_list(&json!({"instance": "anyone"}), &ctx);
        assert_eq!(
            resp["ok"], true,
            "no fleet.yaml must NOT fail closed, got: {resp}"
        );
        let n = resp["result"]["tools"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        assert_eq!(
            n, full,
            "no fleet.yaml → all-open full surface (no-fleet setups not regressed)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// #2454 Slice 6: exercise the real mcp_tool ingress so notifier
    /// propagation is covered as part of the acceptance contract, alongside
    /// the direct API characterization. This regression test preserves the
    /// no-loopback runtime path and ordered event-log/notifier semantics.
    #[test]
    fn move_pane_mcp_ingress_preserves_notifier_and_event_log_2454() {
        use crate::api::{ApiEvent, ApiNotifier, PaneMoveSplitDir};

        struct RecordingNotifier {
            events: parking_lot::Mutex<Vec<ApiEvent>>,
        }

        impl RecordingNotifier {
            fn take(&self) -> Vec<ApiEvent> {
                std::mem::take(&mut *self.events.lock())
            }
        }

        impl ApiNotifier for RecordingNotifier {
            fn notify(&self, event: ApiEvent) {
                self.events.lock().push(event);
            }
        }

        let _fleet_guard = crate::mcp::handlers::fleet_test_guard();
        let home = std::env::temp_dir().join(format!(
            "agend-mcp-move-pane-ingress-red-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);

        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();
        let notifier = std::sync::Arc::new(RecordingNotifier {
            events: parking_lot::Mutex::new(Vec::new()),
        });
        let notifier_trait: std::sync::Arc<dyn ApiNotifier> = notifier.clone();
        let ctx = HandlerCtx {
            registry: &registry,
            configs: &configs,
            externals: &externals,
            notifier: Some(&notifier_trait),
            home: &home,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: crate::api::app_restart::PostFlushSlot::new(),
            shutdown: None,
        };

        let default = handle_mcp_tool(
            &json!({
                "tool": "move_pane",
                "arguments": {"instance": "agent-a", "target_tab": "team-x"},
            }),
            &ctx,
        );
        assert_eq!(
            default["ok"], true,
            "mcp_tool outer default must succeed: {default}"
        );
        assert_eq!(
            default["result"]["ok"], true,
            "mcp_tool inner default must succeed without an API listener: {default}"
        );
        assert_eq!(default["result"]["instance"], "agent-a");
        assert_eq!(default["result"]["target_tab"], "team-x");

        let vertical = handle_mcp_tool(
            &json!({
                "tool": "move_pane",
                "arguments": {
                    "instance": "agent-b",
                    "target_tab": "team-y",
                    "split_dir": "vertical",
                },
            }),
            &ctx,
        );
        assert_eq!(
            vertical["ok"], true,
            "mcp_tool outer vertical must succeed: {vertical}"
        );
        assert_eq!(
            vertical["result"]["ok"], true,
            "mcp_tool inner vertical must succeed without an API listener: {vertical}"
        );
        assert_eq!(vertical["result"]["instance"], "agent-b");
        assert_eq!(vertical["result"]["target_tab"], "team-y");

        let invalid = handle_mcp_tool(
            &json!({
                "tool": "move_pane",
                "arguments": {
                    "instance": "bad/name",
                    "target_tab": "team-z",
                },
            }),
            &ctx,
        );
        assert_eq!(
            invalid["result"]["error"],
            "instance name 'bad/name' contains invalid characters (only a-z, 0-9, -, _ allowed)",
            "MCP move_pane must preserve the legacy API validation-error payload"
        );

        let events = notifier.take();
        assert_eq!(
            events.len(),
            2,
            "default + vertical must emit two ordered events"
        );
        assert!(matches!(
            &events[0],
            ApiEvent::PaneMoved {
                agent,
                target_tab,
                split_dir: PaneMoveSplitDir::Horizontal,
            } if agent == "agent-a" && target_tab == "team-x"
        ));
        assert!(matches!(
            &events[1],
            ApiEvent::PaneMoved {
                agent,
                target_tab,
                split_dir: PaneMoveSplitDir::Vertical,
            } if agent == "agent-b" && target_tab == "team-y"
        ));

        let log = std::fs::read_to_string(home.join("event-log.jsonl"))
            .expect("MCP move_pane must emit event-log records");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "two MCP calls must emit exactly two records"
        );
        assert!(lines[0].contains("\"kind\":\"move_pane\"") && lines[0].contains("Horizontal"));
        assert!(lines[1].contains("\"kind\":\"move_pane\"") && lines[1].contains("Vertical"));

        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }
        std::fs::remove_dir_all(home).ok();
    }

    /// #2454 Slice 7 RED: exercise the real MCP `team action=update` ingress
    /// with a supplied notifier and no API listener. The current MCP adapter
    /// falls back to the direct teams store but drops the owned runtime
    /// notifier, so effective add/remove events are absent until GREEN.
    #[test]
    fn update_team_mcp_ingress_preserves_effective_diff_events_2454() {
        use crate::api::{ApiEvent, ApiNotifier};

        struct RecordingNotifier {
            events: parking_lot::Mutex<Vec<ApiEvent>>,
        }

        impl ApiNotifier for RecordingNotifier {
            fn notify(&self, event: ApiEvent) {
                self.events.lock().push(event);
            }
        }

        let _fleet_guard = crate::mcp::handlers::fleet_test_guard();
        let home = std::env::temp_dir().join(format!(
            "agend-mcp-update-team-ingress-red-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "teams:\n  t1:\n    members: [m1, m2]\n    created_at: \"2026-01-01T00:00:00Z\"\n",
        )
        .expect("seed team");
        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);

        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();
        let notifier = std::sync::Arc::new(RecordingNotifier {
            events: parking_lot::Mutex::new(Vec::new()),
        });
        let notifier_trait: std::sync::Arc<dyn ApiNotifier> = notifier.clone();
        let ctx = HandlerCtx {
            registry: &registry,
            configs: &configs,
            externals: &externals,
            notifier: Some(&notifier_trait),
            home: &home,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: crate::api::app_restart::PostFlushSlot::new(),
            shutdown: None,
        };

        for arguments in [
            json!({"action": "update", "name": "t1", "add": ["m3"]}),
            json!({"action": "update", "name": "t1", "remove": ["m2"]}),
            json!({"action": "update", "name": "t1", "add": ["m1"]}),
        ] {
            let response = handle_mcp_tool(&json!({"tool": "team", "arguments": arguments}), &ctx);
            assert_eq!(
                response["ok"], true,
                "MCP team update outer response: {response}"
            );
            assert_eq!(
                response["result"]["status"], "updated",
                "MCP team update direct fallback response: {response}"
            );
        }

        let events = std::mem::take(&mut *notifier.events.lock());
        assert_eq!(
            events.len(),
            2,
            "add/remove must emit events while noop emits none: {events:?}"
        );
        assert!(matches!(
            &events[0],
            ApiEvent::TeamMembersChanged { name, added, removed }
                if name == "t1" && added == &["m3"] && removed.is_empty()
        ));
        assert!(matches!(
            &events[1],
            ApiEvent::TeamMembersChanged { name, added, removed }
                if name == "t1" && added.is_empty() && removed == &["m2"]
        ));

        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn update_team_shared_service_and_no_api_call_invariants_2454() {
        let source = include_str!("../../mcp/handlers/task.rs");
        let update_start = source
            .find("pub(super) fn handle_update_team(")
            .expect("MCP update_team handler");
        let update_region = &source[update_start..];
        assert!(
            !update_region.contains("api::call"),
            "MCP handle_update_team production region must not self-IPC"
        );
    }

    /// #2454 S8 real-entry: drive handle_mcp_tool → dispatch_create_instance →
    /// spawn_single_instance with SPAWN override + INJECT_INLINE. Proves
    /// RuntimeContext Some propagation, in-process inject_input routing, and
    /// actionable failure diagnostic detail in event-log.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn create_instance_delayed_inject_runtime_routing_2454() {
        use crate::mcp::handlers::instance_state::spawn::{INJECT_INLINE, SPAWN_OVERRIDE};
        let _guard = crate::mcp::handlers::fleet_test_guard();

        let home = std::env::temp_dir().join(format!(
            "agend-s8-mcp-entry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  new-s8:\n    backend: claude\n",
        )
        .unwrap();
        let prev_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);

        fn spawn_ok(
            _: &std::path::Path,
            _: &serde_json::Value,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::json!({"ok": true, "result": {"topic_id": null}}))
        }
        *SPAWN_OVERRIDE.lock() = Some((home.clone(), spawn_ok));
        *INJECT_INLINE.lock() = Some(home.clone());

        let registry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let externals =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let configs =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let post_flush = crate::api::app_restart::PostFlushSlot::new();
        let ctx = super::HandlerCtx {
            home: &home,
            registry: &registry,
            configs: &configs,
            externals: &externals,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush,
            notifier: None,
            shutdown: None,
        };
        let result = handle_mcp_tool(
            &serde_json::json!({
                "tool": "create_instance",
                "instance": "operator",
                "arguments": {
                    "name": "new-s8",
                    "backend": "claude",
                    "task": "do something"
                }
            }),
            &ctx,
        );

        *INJECT_INLINE.lock() = None;
        *SPAWN_OVERRIDE.lock() = None;
        match prev_home {
            Some(v) => std::env::set_var("AGEND_HOME", v),
            None => std::env::remove_var("AGEND_HOME"),
        }

        let inner = result.get("result").unwrap_or(&result);
        assert!(
            inner.get("error").is_none(),
            "spawn must succeed with override: {result}"
        );
        let event_log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            event_log.contains("team_spawn_inject_failed"),
            "delayed inject failure must be logged: {event_log}"
        );
        assert!(
            event_log.contains("not found"),
            "event-log must contain inject_input detail ('not found'): {event_log}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Build a no-listener fixture with a live external name collision. The
    /// runtime registries are deliberately supplied through the real MCP
    /// ingress so the RED proves the current socket fallback drops them.
    #[allow(clippy::unwrap_used)]
    fn spawn_runtime_external_fixture(
        tag: &str,
        fleet_yaml: &str,
    ) -> (
        std::path::PathBuf,
        crate::agent::AgentRegistry,
        crate::api::ConfigRegistry,
        crate::agent::ExternalRegistry,
    ) {
        let home = std::env::temp_dir().join(format!(
            "agend-s11-mcp-spawn-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(crate::fleet::fleet_yaml_path(&home), fleet_yaml).unwrap();
        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();
        externals.lock().insert(
            "collision".to_string(),
            crate::agent::ExternalAgentHandle {
                backend_command: "claude".to_string(),
                pid: 4242,
            },
        );
        (home, registry, configs, externals)
    }

    fn invoke_runtime_mcp_tool(
        home: &std::path::Path,
        registry: &crate::agent::AgentRegistry,
        configs: &crate::api::ConfigRegistry,
        externals: &crate::agent::ExternalRegistry,
        tool: &str,
        instance: &str,
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        let ctx = HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: crate::api::app_restart::PostFlushSlot::new(),
            shutdown: None,
        };
        handle_mcp_tool(
            &json!({"tool": tool, "instance": instance, "arguments": arguments}),
            &ctx,
        )
    }

    /// #2454 Slice 11 RED D6: the real MCP start ingress must use the live
    /// external registry and return the typed collision before any SPAWN RPC.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn start_instance_real_entry_runtime_external_collision_2454() {
        let _guard = crate::mcp::handlers::fleet_test_guard();
        let (home, registry, configs, externals) =
            spawn_runtime_external_fixture("d6", "instances:\n  collision:\n    backend: claude\n");
        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);
        let response = invoke_runtime_mcp_tool(
            &home,
            &registry,
            &configs,
            &externals,
            "start_instance",
            "operator",
            json!({"instance": "collision"}),
        );
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(
            response["ok"], true,
            "MCP start must return a response: {response}"
        );
        let error = response["result"]["error"].as_str().unwrap_or_default();
        assert_eq!(
            error, "agent 'collision' already exists (external)",
            "D6 must surface the typed external collision, not the socket fallback: {response}"
        );
    }

    /// #2454 Slice 11 RED D10: the real MCP create ingress must use the live
    /// external registry and reject the seeded name before issuing SPAWN.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn create_instance_real_entry_runtime_external_collision_2454() {
        let _guard = crate::mcp::handlers::fleet_test_guard();
        let (home, registry, configs, externals) =
            spawn_runtime_external_fixture("d10", "instances: {}\n");
        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);
        let response = invoke_runtime_mcp_tool(
            &home,
            &registry,
            &configs,
            &externals,
            "create_instance",
            "operator",
            json!({
                "name": "collision",
                "backend": "claude",
                "topic_binding": "skip"
            }),
        );
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(
            response["ok"], true,
            "MCP create must return a response: {response}"
        );
        let error = response["result"]["error"].as_str().unwrap_or_default();
        assert_eq!(
            error, "agent 'collision' already exists (external)",
            "D10 must surface the typed external collision, not the socket fallback: {response}"
        );
    }

    /// #2454 Slice 11 D8 characterization: restart keeps its runtime-owned
    /// delete path, and its final SPAWN uses the shared runtime service. The
    /// colliding survivor makes the service result deterministic without a PTY.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn restart_instance_real_entry_runtime_spawn_no_listener_2454() {
        let _guard = crate::mcp::handlers::fleet_test_guard();
        let (home, registry, configs, externals) = spawn_runtime_external_fixture(
            "d8",
            "instances:\n  restart-target:\n    backend: claude\n",
        );
        let shared_work_dir = home.join("shared-workdir");
        let fleet_path = crate::fleet::fleet_yaml_path(&home);
        std::fs::write(
            &fleet_path,
            format!(
                "instances:\n  restart-target:\n    backend: claude\n    working_directory: {}\n  restart-collider:\n    backend: claude\n    working_directory: {}\n",
                shared_work_dir.display(),
                shared_work_dir.display()
            ),
        )
        .unwrap();
        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);
        let response = invoke_runtime_mcp_tool(
            &home,
            &registry,
            &configs,
            &externals,
            "restart_instance",
            "operator",
            json!({"instance": "restart-target", "mode": "resume"}),
        );
        let external_survived = externals.lock().contains_key("collision");
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(
            response["ok"], true,
            "MCP restart must return a response: {response}"
        );
        assert_eq!(
            response["result"]["spawned"], false,
            "the runtime service must reject the surviving workspace collision: {response}"
        );
        assert!(
            external_survived,
            "unrelated external fixture must remain untouched by restart: {response}"
        );
    }

    /// #2454 Slice 11 RED D8: the runtime-present restart SPAWN leaf must not
    /// self-IPC. This source/reachability pin is deterministic and keeps the
    /// real-ingress characterization above free of a flaky PTY seam.
    #[test]
    fn restart_instance_runtime_spawn_is_not_socket_fallback_2454() {
        let source = include_str!("../../mcp/handlers/instance_state/mod.rs");
        let restart_start = source
            .find("pub(super) fn handle_restart_instance_with_runtime(")
            .expect("runtime-aware restart handler");
        let restart_end = source[restart_start..]
            .find("/// #t-777-3")
            .map(|offset| restart_start + offset)
            .expect("restart handler end marker");
        let restart_region = &source[restart_start..restart_end];
        let spawn_start = restart_region
            .find("let spawn_result")
            .expect("restart SPAWN leaf");
        let spawn_region = &restart_region[spawn_start..];
        assert!(
            !spawn_region.lines().any(|line| {
                let trimmed = line.trim();
                !trimmed.starts_with("//") && line.contains("crate::api::call")
            }),
            "D8 runtime-present SPAWN must use the shared runtime service, not crate::api::call"
        );
    }

    /// #2454 Slice 10 RED: drive the real MCP delete_instance ingress with no
    /// API listener. A managed fleet/config entry must be removed in-process
    /// and the supplied notifier must receive InstanceDeleted; the current
    /// adapter still falls back to the lifecycle helper's DELETE loopback.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn delete_instance_real_entry_removes_managed_state_and_notifies_2454() {
        use crate::api::{ApiEvent, ApiNotifier};

        struct RecordingNotifier {
            events: parking_lot::Mutex<Vec<ApiEvent>>,
        }

        impl ApiNotifier for RecordingNotifier {
            fn notify(&self, event: ApiEvent) {
                self.events.lock().push(event);
            }
        }

        let _guard = crate::mcp::handlers::fleet_test_guard();
        let home = std::env::temp_dir().join(format!(
            "agend-s10-mcp-delete-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let id = crate::types::InstanceId::new().full();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!("instances:\n  victim:\n    id: {id}\n    backend: claude\n    created_by: s10-delete-caller-2454\n"),
        )
        .unwrap();
        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);

        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        configs.lock().insert(
            "victim".to_string(),
            crate::daemon::AgentConfig {
                name: "victim".to_string(),
                backend: None,
                backend_command: crate::default_shell().to_string(),
                args: Vec::new(),
                env: None,
                working_dir: None,
                submit_key: "\r".to_string(),
            },
        );
        let externals: crate::agent::ExternalRegistry = Default::default();
        let notifier = std::sync::Arc::new(RecordingNotifier {
            events: parking_lot::Mutex::new(Vec::new()),
        });
        let notifier_trait: std::sync::Arc<dyn ApiNotifier> = notifier.clone();
        let ctx = HandlerCtx {
            registry: &registry,
            configs: &configs,
            externals: &externals,
            notifier: Some(&notifier_trait),
            home: &home,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: crate::api::app_restart::PostFlushSlot::new(),
            shutdown: None,
        };
        let response = handle_mcp_tool(
            &json!({
                "tool": "delete_instance",
                "instance": "s10-delete-caller-2454",
                "arguments": {"instance": "victim"}
            }),
            &ctx,
        );
        let fleet_has_victim =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
                .ok()
                .and_then(|fleet| fleet.instances.get("victim").map(|_| ()))
                .is_some();
        let config_present = configs.lock().contains_key("victim");
        let events = std::mem::take(&mut *notifier.events.lock());

        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(
            response["ok"], true,
            "MCP delete ingress must return a response: {response}"
        );
        assert_eq!(
            response["result"].get("error"),
            None,
            "managed delete must complete without residual error: {response}"
        );
        assert!(
            !fleet_has_victim
                && !config_present
                && matches!(events.as_slice(), [ApiEvent::InstanceDeleted { name }] if name == "victim"),
            "managed delete must remove fleet/config state and emit InstanceDeleted; got \
             fleet_has_victim={fleet_has_victim}, config_present={config_present}, \
             events={events:?}, response={response}"
        );
    }

    /// #2454 Slice 9: drive the real MCP restart_daemon ingress with a Daemon
    /// capability and an injected shutdown flag.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn restart_daemon_real_entry_shutdown_flag_2454() {
        use std::ffi::OsString;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let _guard = crate::mcp::handlers::fleet_test_guard();
        let home = std::env::temp_dir().join(format!(
            "agend-s9-mcp-restart-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  operator:\n    role_kind: orchestrator\n",
        )
        .unwrap();

        let previous_home = std::env::var_os("AGEND_HOME");
        let previous_handoff = std::env::var_os("AGEND_RESTART_HANDOFF");
        let previous_supervised = std::env::var_os("AGEND_SUPERVISED");
        std::env::set_var("AGEND_HOME", &home);
        std::env::set_var("AGEND_RESTART_HANDOFF", OsString::from("0"));
        std::env::set_var("AGEND_SUPERVISED", OsString::from("1"));

        let previous_restart_pending = crate::daemon::RESTART_PENDING.load(Ordering::Acquire);
        crate::daemon::RESTART_PENDING.store(false, Ordering::Release);
        let shutdown = Arc::new(AtomicBool::new(false));
        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();
        let ctx = HandlerCtx {
            registry: &registry,
            configs: &configs,
            externals: &externals,
            notifier: None,
            home: &home,
            capability: crate::api::RestartCapability::Daemon,
            app_restart: None,
            post_flush: crate::api::app_restart::PostFlushSlot::new(),
            shutdown: Some(shutdown.clone()),
        };
        let response = handle_mcp_tool(
            &json!({
                "tool": "restart_daemon",
                "instance": "operator",
                "arguments": {}
            }),
            &ctx,
        );
        let response_ok = response["ok"] == true && response["result"]["ok"] == true;
        let shutdown_set = shutdown.load(Ordering::Acquire);

        crate::daemon::RESTART_PENDING.store(previous_restart_pending, Ordering::Release);
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }
        match previous_handoff {
            Some(value) => std::env::set_var("AGEND_RESTART_HANDOFF", value),
            None => std::env::remove_var("AGEND_RESTART_HANDOFF"),
        }
        match previous_supervised {
            Some(value) => std::env::set_var("AGEND_SUPERVISED", value),
            None => std::env::remove_var("AGEND_SUPERVISED"),
        }
        std::fs::remove_dir_all(&home).ok();

        assert!(
            response_ok,
            "legacy restart_daemon response must remain ok: {response}"
        );
        assert!(
            shutdown_set,
            "production restart_daemon must set the injected shutdown flag: {response}"
        );
    }

    /// #2454 Slice 9: a supervised Daemon request without an injected shutdown
    /// authority must fail closed before touching restart state.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn restart_daemon_real_entry_without_shutdown_fails_closed_2454() {
        use std::ffi::OsString;
        use std::sync::atomic::Ordering;

        let _guard = crate::mcp::handlers::fleet_test_guard();
        let home = std::env::temp_dir().join(format!(
            "agend-s9-mcp-restart-no-shutdown-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  operator:\n    role_kind: orchestrator\n",
        )
        .unwrap();

        let previous_home = std::env::var_os("AGEND_HOME");
        let previous_handoff = std::env::var_os("AGEND_RESTART_HANDOFF");
        let previous_supervised = std::env::var_os("AGEND_SUPERVISED");
        std::env::set_var("AGEND_HOME", &home);
        std::env::set_var("AGEND_RESTART_HANDOFF", OsString::from("0"));
        std::env::set_var("AGEND_SUPERVISED", OsString::from("1"));

        let previous_restart_pending = crate::daemon::RESTART_PENDING.load(Ordering::Acquire);
        crate::daemon::RESTART_PENDING.store(false, Ordering::Release);
        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();
        let ctx = HandlerCtx {
            registry: &registry,
            configs: &configs,
            externals: &externals,
            notifier: None,
            home: &home,
            capability: crate::api::RestartCapability::Daemon,
            app_restart: None,
            post_flush: crate::api::app_restart::PostFlushSlot::new(),
            shutdown: None,
        };
        let response = handle_mcp_tool(
            &json!({
                "tool": "restart_daemon",
                "instance": "operator",
                "arguments": {}
            }),
            &ctx,
        );
        let pending = crate::daemon::RESTART_PENDING.load(Ordering::Acquire);
        let marker_exists = home.join("restart-requested").exists();

        crate::daemon::RESTART_PENDING.store(previous_restart_pending, Ordering::Release);
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }
        match previous_handoff {
            Some(value) => std::env::set_var("AGEND_RESTART_HANDOFF", value),
            None => std::env::remove_var("AGEND_RESTART_HANDOFF"),
        }
        match previous_supervised {
            Some(value) => std::env::set_var("AGEND_SUPERVISED", value),
            None => std::env::remove_var("AGEND_SUPERVISED"),
        }
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(
            response["ok"], true,
            "MCP ingress must return a response: {response}"
        );
        assert_eq!(
            response["result"]["ok"], false,
            "missing authority must fail closed: {response}"
        );
        assert!(
            !pending,
            "missing authority must not set RESTART_PENDING: {response}"
        );
        assert!(
            !marker_exists,
            "missing authority must not write restart-requested"
        );
    }

    // ── #2454 Slice 12: SEND runtime contract RED tests ─────────────

    /// #2454 Slice 12 RED (1/3): structural budget + reachability guard.
    /// (a) cumulative handler api::call == 5, (b) dispatch_send not adapter!,
    /// (c) agent_ops::send_to zero api::call, (d) SEND region bridge == 1.
    #[test]
    fn send_structural_budget_and_reachability_2454() {
        let needle_call = concat!("crate::", "api::", "call");
        let needle_at = concat!("api::", "call_at");
        let test_mod_marker = "#[cfg(test)]\nmod ";

        // (a) cumulative budget: 8 → 5
        // Three handler-local sites to eliminate: SEND (comms.rs),
        // REPORT (comms.rs), DELEGATE (comms_delegate/mod.rs).
        let files: &[&str] = &[
            include_str!("../../mcp/handlers/comms.rs"),
            include_str!("../../mcp/handlers/comms_delegate/mod.rs"),
            include_str!("../../mcp/handlers/task.rs"),
            include_str!("../../mcp/handlers/restart.rs"),
            include_str!("../../mcp/handlers/instance_state/mod.rs"),
            include_str!("../../mcp/handlers/instance_state/spawn.rs"),
            include_str!("../../mcp/handlers/instance_state/lifecycle.rs"),
            include_str!("../../mcp/handlers/instance_metadata.rs"),
        ];
        let mut handler_count = 0;
        for src in files {
            let boundary = src.rfind(test_mod_marker).unwrap_or(src.len());
            let production = &src[..boundary];
            for line in production.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") || trimmed.starts_with("///") {
                    continue;
                }
                if line.contains(needle_call) && !line.contains(needle_at) {
                    handler_count += 1;
                }
            }
        }
        assert_eq!(
            handler_count, 5,
            "Slice-12 cumulative api::call budget must be 5; got {handler_count}"
        );

        // (b) dispatch_send must NOT be adapter! — custom fn threads RuntimeContext
        let dispatch_src = include_str!("../../mcp/handlers/dispatch.rs");
        let dispatch_boundary = dispatch_src
            .rfind(test_mod_marker)
            .unwrap_or(dispatch_src.len());
        let dispatch_prod = &dispatch_src[..dispatch_boundary];
        let adapter_send = concat!("adapter!(dispatch_send");
        assert!(
            !dispatch_prod.contains(adapter_send),
            "dispatch_send must be a custom fn threading RuntimeContext, not adapter!"
        );

        // (c) agent_ops::send_to must have zero api::call (MCP bypasses it)
        let ops_src = include_str!("../../agent_ops.rs");
        let fn_start = ops_src
            .find("pub fn send_to(")
            .expect("agent_ops::send_to must exist");
        let fn_region = &ops_src[fn_start..];
        let fn_end = fn_region
            .find("\npub ")
            .or_else(|| fn_region.find("\n// -----"))
            .unwrap_or(fn_region.len());
        let fn_body = &fn_region[..fn_end];
        let ops_count = fn_body
            .lines()
            .filter(|line| {
                let t = line.trim();
                !t.starts_with("//") && !t.starts_with("///") && line.contains(needle_call)
            })
            .count();
        assert_eq!(
            ops_count, 0,
            "agent_ops::send_to must have zero api::call after migration; got {ops_count}"
        );

        // (d) SEND production region (comms + comms_delegate + agent_ops) must
        // have exactly 1 remaining api::call bridge (the compatibility fallback).
        let send_region_files: &[&str] = &[
            include_str!("../../mcp/handlers/comms.rs"),
            include_str!("../../mcp/handlers/comms_delegate/mod.rs"),
            ops_src,
        ];
        let mut send_region_count = 0;
        for src in send_region_files {
            let boundary = src.rfind(test_mod_marker).unwrap_or(src.len());
            let production = &src[..boundary];
            for line in production.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") || trimmed.starts_with("///") {
                    continue;
                }
                if line.contains(needle_call) && !line.contains(needle_at) {
                    send_region_count += 1;
                }
            }
        }
        assert_eq!(
            send_region_count, 1,
            "SEND production region must have exactly 1 api::call bridge; got {send_region_count}"
        );
    }

    /// #2454 Slice 12 RED (2/3): real MCP ingress — a send with
    /// RuntimeContext=Some must deliver via the neutral in-process service,
    /// not the socket fallback. Currently api::call fails in test (no daemon)
    /// → fallback_deliver → delivery_mode="inbox_fallback".
    #[test]
    #[allow(clippy::unwrap_used)]
    fn send_real_entry_runtime_delivery_not_fallback_2454() {
        let _guard = crate::mcp::handlers::fleet_test_guard();
        let tag = format!(
            "send-rt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let home = std::env::temp_dir().join(format!("agend-s12-{tag}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  send-target:\n    backend: claude\n",
        )
        .unwrap();
        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();

        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);
        let response = invoke_runtime_mcp_tool(
            &home,
            &registry,
            &configs,
            &externals,
            "send",
            "test-sender",
            json!({
                "instance": "send-target",
                "message": "hello from Slice 12 RED",
            }),
        );
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(
            response["ok"], true,
            "MCP send ingress must return a response: {response}"
        );
        let dm = response["result"]["delivery_mode"]
            .as_str()
            .unwrap_or_default();
        assert_ne!(
            dm, "inbox_fallback",
            "SEND with RuntimeContext must deliver via the neutral service, \
             not the socket fallback (delivery_mode={dm}): {response}"
        );
    }

    /// #2454 Slice 12 RED (3/3): real MCP ingress — the neutral service's
    /// team-isolation gate must reject a cross-team send with no delivery
    /// side-effect. Currently api::call fails → fallback_deliver bypasses
    /// the API's policy gates → message silently delivered.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn send_real_entry_runtime_team_isolation_gate_2454() {
        let _guard = crate::mcp::handlers::fleet_test_guard();
        let tag = format!(
            "send-xteam-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let home = std::env::temp_dir().join(format!("agend-s12-{tag}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  \
               alpha-1:\n    backend: claude\n  \
               beta-1:\n    backend: claude\n\
             teams:\n  \
               alpha:\n    members: [alpha-1]\n  \
               beta:\n    members: [beta-1]\n",
        )
        .unwrap();
        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();

        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);
        let response = invoke_runtime_mcp_tool(
            &home,
            &registry,
            &configs,
            &externals,
            "send",
            "alpha-1",
            json!({
                "instance": "beta-1",
                "message": "cross-team probe",
            }),
        );
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }

        // Verify no delivery side-effect: inbox must be empty for the target
        let inbox_dir = home.join("inbox").join("beta-1");
        let inbox_has_message = inbox_dir.exists()
            && std::fs::read_dir(&inbox_dir)
                .map(|d| d.count() > 0)
                .unwrap_or(false);
        std::fs::remove_dir_all(&home).ok();

        let result_str = response.to_string();
        assert!(
            result_str.contains("cross") && result_str.contains("team"),
            "cross-team send must be rejected by the neutral service's team-isolation \
             gate, not silently delivered via the socket fallback: {response}"
        );
        assert!(
            !inbox_has_message,
            "rejected cross-team send must not produce a delivery side-effect in the target inbox"
        );
    }

    // ── #2454 Slice 12 supplemental RED ───────────────────────────────

    /// #2454 Slice 12 supplemental RED (1/3): RuntimeContext=None (standalone
    /// mode) with daemon unreachable must NOT silently succeed via inbox
    /// fallback. The _with_runtime_or_legacy bridge must surface the failure.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn send_runtime_none_no_silent_fallback_2454() {
        let _guard = crate::mcp::handlers::fleet_test_guard();
        let tag = format!(
            "send-none-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let home = std::env::temp_dir().join(format!("agend-s12-{tag}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  none-target:\n    backend: claude\n",
        )
        .unwrap();

        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);
        let response = crate::mcp::handlers::handle_tool_with_runtime(
            "send",
            &json!({
                "instance": "none-target",
                "message": "hello from runtime=None test",
            }),
            "test-sender",
            None,
        );
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }
        std::fs::remove_dir_all(&home).ok();

        let dm = response["delivery_mode"].as_str().unwrap_or_default();
        assert_ne!(
            dm, "inbox_fallback",
            "runtime=None send must NOT silently succeed via inbox_fallback — \
             daemon-unreachable must be an honest failure: {response}"
        );
    }

    /// #2454 Slice 12 supplemental RED (2/3): a report (request_kind=report)
    /// via MCP ingress with RuntimeContext=Some must deliver through the
    /// neutral service, not the socket fallback. Proves the report path's
    /// API-level logic (authorize_report, process_verdicts, track_dispatch)
    /// runs through the in-process path.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn send_report_through_service_not_fallback_2454() {
        let _guard = crate::mcp::handlers::fleet_test_guard();
        let tag = format!(
            "send-rpt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let home = std::env::temp_dir().join(format!("agend-s12-{tag}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  rpt-target:\n    backend: claude\n",
        )
        .unwrap();
        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();

        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);
        let response = invoke_runtime_mcp_tool(
            &home,
            &registry,
            &configs,
            &externals,
            "send",
            "rpt-sender",
            json!({
                "instance": "rpt-target",
                "request_kind": "report",
                "message": "report summary text",
                "correlation_id": "t-test-rpt-corr",
            }),
        );
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }
        std::fs::remove_dir_all(&home).ok();

        let dm = response["result"]["delivery_mode"]
            .as_str()
            .or_else(|| response["result"]["note"].as_str())
            .unwrap_or_default();
        assert!(
            !dm.contains("inbox_fallback") && !dm.contains("API unavailable"),
            "report via MCP must deliver through the neutral service, \
             not the socket fallback: {response}"
        );
    }

    /// #2454 Slice 12 supplemental RED (3/3): a delegate (request_kind=task)
    /// via MCP ingress with RuntimeContext=Some must deliver through the
    /// neutral service. Proves dispatch tracking, auto-bind, and task
    /// correlation fire through the in-process path.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn send_delegate_through_service_not_fallback_2454() {
        let _guard = crate::mcp::handlers::fleet_test_guard();
        let tag = format!(
            "send-del-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let home = std::env::temp_dir().join(format!("agend-s12-{tag}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  del-target:\n    backend: claude\n",
        )
        .unwrap();
        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();

        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);
        let response = invoke_runtime_mcp_tool(
            &home,
            &registry,
            &configs,
            &externals,
            "send",
            "del-sender",
            json!({
                "instance": "del-target",
                "request_kind": "task",
                "message": "test task delegation",
                "task_id": "t-test-delegate-001",
            }),
        );
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }
        std::fs::remove_dir_all(&home).ok();

        let result_str = response["result"].to_string();
        assert!(
            !result_str.contains("inbox_fallback") && !result_str.contains("API unavailable"),
            "delegate via MCP must deliver through the neutral service, \
             not the socket fallback: {response}"
        );
    }

    // ── #2454 Slice 12 supplemental RED batch 2 (d-20260719053726713862-4) ──
    //
    // Three invariant families:
    //   1. Directive integrity (broadcast + query)
    //   2. runtime=None zero side effects
    //   3. Post-success exactly-once (report + delegate + source-owner guard)
    //
    // Post-success effect ownership (reference for tests 9-11):
    //   SERVICE level (handle_send in messaging.rs):
    //     settle_parent_after_successful_send, inject_provenance,
    //     checkout_branch_if_requested, process_verdicts, track_dispatch
    //   MCP DECORATOR level (comms.rs handlers):
    //     dispatch_tracking::mark_completed, ack_by_correlation,
    //     record_triaged_if_present, UxEvent emission

    /// d-...-4 invariant 1a: broadcast must preserve the full advertised
    /// directive set in EVERY per-target delivered InboxMessage. Currently
    /// handle_broadcast → send_to only passes from/target/text/kind/
    /// broadcast_context, silently dropping all dispatch directives.
    /// Checks BOTH targets and all legal directives that InboxMessage carries.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn send_broadcast_directive_integrity_2454() {
        let _guard = crate::mcp::handlers::fleet_test_guard();
        let tag = format!(
            "bcast-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let home = std::env::temp_dir().join(format!("agend-s12b2-{tag}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  bc-sender:\n    backend: claude\n  \
             bc-target-a:\n    backend: claude\n  \
             bc-target-b:\n    backend: claude\n",
        )
        .unwrap();
        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();

        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);
        let _response = invoke_runtime_mcp_tool(
            &home,
            &registry,
            &configs,
            &externals,
            "send",
            "bc-sender",
            json!({
                "instances": ["bc-target-a", "bc-target-b"],
                "message": "broadcast with directives",
                "thread_id": "thread-bc-001",
                "parent_id": "parent-bc-001",
                "correlation_id": "corr-bc-001",
                "eta_minutes": 30,
                "reporting_cadence": "per-pr",
                "worktree_binding_required": true,
                "terminal": true,
            }),
        );
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }

        let msgs_a = crate::inbox::drain(&home, "bc-target-a");
        let msgs_b = crate::inbox::drain(&home, "bc-target-b");
        std::fs::remove_dir_all(&home).ok();

        // Both targets must receive a message.
        assert!(!msgs_a.is_empty(), "broadcast must deliver to bc-target-a");
        assert!(!msgs_b.is_empty(), "broadcast must deliver to bc-target-b");

        // Check full directive set on BOTH targets.
        for (label, msg) in [("target-a", &msgs_a[0]), ("target-b", &msgs_b[0])] {
            assert_eq!(
                msg.thread_id.as_deref(), Some("thread-bc-001"),
                "{label}: broadcast must preserve thread_id; got {:?}", msg.thread_id
            );
            assert_eq!(
                msg.parent_id.as_deref(), Some("parent-bc-001"),
                "{label}: broadcast must preserve parent_id; got {:?}", msg.parent_id
            );
            assert_eq!(
                msg.correlation_id.as_deref(), Some("corr-bc-001"),
                "{label}: broadcast must preserve correlation_id; got {:?}", msg.correlation_id
            );
            assert_eq!(
                msg.eta_minutes, Some(30),
                "{label}: broadcast must preserve eta_minutes; got {:?}", msg.eta_minutes
            );
            assert_eq!(
                msg.reporting_cadence.as_deref(), Some("per-pr"),
                "{label}: broadcast must preserve reporting_cadence; got {:?}", msg.reporting_cadence
            );
            assert_eq!(
                msg.worktree_binding_required, Some(true),
                "{label}: broadcast must preserve worktree_binding_required; got {:?}",
                msg.worktree_binding_required
            );
            assert_eq!(
                msg.terminal, Some(true),
                "{label}: broadcast must preserve terminal; got {:?}", msg.terminal
            );
        }
    }

    /// d-...-4 invariant 1b: query (request_kind=query) must preserve
    /// directive set in the delivered message. Currently
    /// handle_request_information → send_to drops all directives.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn send_query_directive_integrity_2454() {
        let _guard = crate::mcp::handlers::fleet_test_guard();
        let tag = format!(
            "query-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let home = std::env::temp_dir().join(format!("agend-s12b2-{tag}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  q-sender:\n    backend: claude\n  \
             q-target:\n    backend: claude\n",
        )
        .unwrap();
        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();

        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);
        let _response = invoke_runtime_mcp_tool(
            &home,
            &registry,
            &configs,
            &externals,
            "send",
            "q-sender",
            json!({
                "instance": "q-target",
                "request_kind": "query",
                "message": "what is the status?",
                "thread_id": "thread-q-001",
                "parent_id": "parent-q-001",
                "correlation_id": "corr-q-001",
                "eta_minutes": 15,
                "reporting_cadence": "both",
                "worktree_binding_required": true,
                "terminal": true,
            }),
        );
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }

        let msgs = crate::inbox::drain(&home, "q-target");
        std::fs::remove_dir_all(&home).ok();

        assert!(!msgs.is_empty(), "query must deliver to q-target");
        let msg = &msgs[0];
        assert_eq!(
            msg.thread_id.as_deref(), Some("thread-q-001"),
            "query must preserve thread_id; got {:?}", msg.thread_id
        );
        assert_eq!(
            msg.parent_id.as_deref(), Some("parent-q-001"),
            "query must preserve parent_id; got {:?}", msg.parent_id
        );
        assert_eq!(
            msg.correlation_id.as_deref(), Some("corr-q-001"),
            "query must preserve correlation_id; got {:?}", msg.correlation_id
        );
        assert_eq!(
            msg.eta_minutes, Some(15),
            "query must preserve eta_minutes; got {:?}", msg.eta_minutes
        );
        assert_eq!(
            msg.reporting_cadence.as_deref(), Some("both"),
            "query must preserve reporting_cadence; got {:?}", msg.reporting_cadence
        );
        assert_eq!(
            msg.worktree_binding_required, Some(true),
            "query must preserve worktree_binding_required; got {:?}",
            msg.worktree_binding_required
        );
        assert_eq!(
            msg.terminal, Some(true),
            "query must preserve terminal; got {:?}", msg.terminal
        );
    }

    /// d-...-4 invariant 2: runtime=None with API unavailable must produce
    /// ZERO side effects across ALL durable stores: no target inbox entry,
    /// no dispatch tracking, no dispatch-idle sidecar, no task auto-creation,
    /// sender's pre-seeded dispatch row remains unsettled, no discharge
    /// ledger entry. Currently the fallback path creates an inbox entry
    /// and fires post-success decorations.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn send_runtime_none_failure_zero_side_effects_2454() {
        let _guard = crate::mcp::handlers::fleet_test_guard();
        let tag = format!(
            "none-fx-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let home = std::env::temp_dir().join(format!("agend-s12b2-{tag}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  fx-sender:\n    backend: claude\n  \
             fx-target:\n    backend: claude\n",
        )
        .unwrap();

        // Pre-seed a dispatch row in sender's inbox (task dispatch from target).
        // After drain it enters "delivering" state. A failed send must NOT
        // settle this row (ack_by_correlation must not fire).
        crate::inbox::enqueue(
            &home,
            "fx-sender",
            crate::inbox::InboxMessage {
                schema_version: 1,
                id: Some("m-fx-parent".into()),
                from: "fx-target".into(),
                text: "[task] pre-seeded dispatch".into(),
                kind: Some("task".into()),
                task_id: Some("t-fx-parent-task".into()),
                timestamp: "2026-07-19T00:00:00Z".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let pre_drain = crate::inbox::drain(&home, "fx-sender");
        assert_eq!(pre_drain.len(), 1, "pre-seeded dispatch drained → delivering");

        // Register UX Recorder — must record 0 events on failure.
        use crate::channel::sink_registry::registry as ux_sink_registry;
        use crate::channel::ux_event::{UxEvent, UxEventSink};
        let rec = {
            struct Rec(parking_lot::Mutex<Vec<UxEvent>>);
            impl UxEventSink for Rec {
                fn emit(&self, event: &UxEvent) { self.0.lock().push(event.clone()); }
            }
            std::sync::Arc::new(Rec(parking_lot::Mutex::new(Vec::new())))
        };
        ux_sink_registry().clear_for_test();
        ux_sink_registry().register(rec.clone() as std::sync::Arc<dyn UxEventSink>);

        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);
        let response = crate::mcp::handlers::handle_tool_with_runtime(
            "send",
            &json!({
                "instance": "fx-target",
                "message": "this should leave no trace",
                "request_kind": "task",
                "task_id": "t-fx-zero-side-effects",
                "correlation_id": "t-fx-parent-task",
                "triaged": {"head": "fx-test-head-sha", "job": "fx-test-job"},
            }),
            "fx-sender",
            None,
        );
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }

        // (a) Response must indicate failure explicitly.
        let resp_str = response.to_string();
        assert!(
            resp_str.contains("error") || resp_str.contains("fail")
                || resp_str.contains("unavailable") || response["ok"] == false,
            "runtime=None must return an explicit failure response; got: {response}"
        );

        // (b) No inbox entry for the target.
        let target_msgs = crate::inbox::drain(&home, "fx-target");
        // (c) No dispatch tracking record for the target.
        let has_tracking =
            crate::dispatch_tracking::has_for_instance(&home, "fx-target");
        // (d) No dispatch-idle sidecar.
        let has_idle =
            crate::daemon::dispatch_idle::has_pending_for_instance(&home, "fx-target");
        // (e) Sender's pre-seeded parent remains unsettled (Delivering, not
        // ReadAt). describe_message is the direct status probe — drain would
        // miss a delivering row because drain only returns unread messages.
        let parent_status = crate::inbox::describe_message(
            &home, "m-fx-parent", "fx-sender",
        );
        // (f) No task auto-created on the board for the sent task_id.
        let board = crate::tasks::handle(&home, "fx-sender", &json!({"action": "list"}));
        let has_auto_task = board["tasks"]
            .as_array()
            .map(|arr| arr.iter().any(|t| t["id"] == "t-fx-zero-side-effects"))
            .unwrap_or(false);
        // (g) No discharge ledger entry.
        let has_discharge = crate::daemon::discharge_ledger::lookup_discharge(
            &home,
            "fx-test-head-sha",
            "fx-test-job",
        )
        .is_some();
        // (h) No PR-state residue.
        let pr_dir = crate::daemon::pr_state::pr_state_dir(&home);
        let has_pr_state = pr_dir.exists()
            && std::fs::read_dir(&pr_dir)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false);
        std::fs::remove_dir_all(&home).ok();

        assert!(
            target_msgs.is_empty(),
            "runtime=None failure must not leave an inbox entry; found {} messages",
            target_msgs.len()
        );
        assert!(
            !has_tracking,
            "runtime=None failure must not create a dispatch tracking record"
        );
        assert!(
            !has_idle,
            "runtime=None failure must not create a dispatch-idle sidecar"
        );
        assert!(
            matches!(parent_status, crate::inbox::MessageStatus::Delivering { .. }),
            "runtime=None failure must not settle the pre-seeded parent — \
             expected Delivering, got {:?}",
            parent_status
        );
        assert!(
            !has_auto_task,
            "runtime=None failure must not auto-create a task on the board"
        );
        assert!(
            !has_discharge,
            "runtime=None failure must not write a discharge ledger entry"
        );
        assert!(
            !has_pr_state,
            "runtime=None failure must not create any PR-state residue"
        );
        // (i) UX: zero events on failure.
        let ux_events = rec.0.lock().clone();
        assert!(
            ux_events.is_empty(),
            "runtime=None failure must not emit any UX events; got {} events",
            ux_events.len()
        );
    }

    /// d-...-4 invariant 3a: a successful report via runtime=Some must
    /// deliver through the neutral service (not fallback) AND fire MCP
    /// post-success decorations (ack_by_correlation settles sender's
    /// dispatch row). Exactly 1 message must appear in the target's inbox.
    /// Currently adapter! strips runtime → delivery via inbox_fallback.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn send_report_service_delivery_with_post_effects_2454() {
        let _guard = crate::mcp::handlers::fleet_test_guard();
        let tag = format!(
            "rpt-fx-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let home = std::env::temp_dir().join(format!("agend-s12b2-{tag}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  rpt-fx-sender:\n    backend: claude\n  \
             rpt-fx-target:\n    backend: claude\n",
        )
        .unwrap();
        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();

        // Pre-seed an inbox dispatch row so ack_by_correlation has something to settle.
        crate::inbox::enqueue(
            &home,
            "rpt-fx-sender",
            crate::inbox::InboxMessage {
                schema_version: 1,
                id: Some("m-dispatch-fx".into()),
                from: "rpt-fx-target".into(),
                text: "[task] do it".into(),
                kind: Some("task".into()),
                task_id: Some("t-rpt-fx-corr".into()),
                timestamp: "2026-07-19T00:00:00Z".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            crate::inbox::drain(&home, "rpt-fx-sender").len(),
            1,
            "pre-seeded dispatch row drained → delivering"
        );

        // Pre-seed a DispatchEntry so mark_completed has something to clear.
        crate::dispatch_tracking::track_dispatch(
            &home,
            crate::dispatch_tracking::DispatchEntry {
                task_id: Some("t-rpt-fx-corr".into()),
                from: "rpt-fx-target".into(),
                to: "rpt-fx-sender".into(),
                from_id: None,
                to_id: None,
                delegated_at: "2026-07-19T00:00:00Z".into(),
                status: "pending".into(),
            },
        );
        assert!(
            crate::dispatch_tracking::has_for_instance(&home, "rpt-fx-sender"),
            "pre-seeded DispatchEntry must exist"
        );

        // Register UX Recorder.
        use crate::channel::sink_registry::registry as ux_sink_registry;
        use crate::channel::ux_event::{UxEvent, UxEventSink};
        let rec = {
            struct Rec(parking_lot::Mutex<Vec<UxEvent>>);
            impl UxEventSink for Rec {
                fn emit(&self, event: &UxEvent) { self.0.lock().push(event.clone()); }
            }
            std::sync::Arc::new(Rec(parking_lot::Mutex::new(Vec::new())))
        };
        ux_sink_registry().clear_for_test();
        ux_sink_registry().register(rec.clone() as std::sync::Arc<dyn UxEventSink>);

        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);
        let response = invoke_runtime_mcp_tool(
            &home,
            &registry,
            &configs,
            &externals,
            "send",
            "rpt-fx-sender",
            json!({
                "instance": "rpt-fx-target",
                "request_kind": "report",
                "message": "report with post-effects",
                "correlation_id": "t-rpt-fx-corr",
                "triaged": {"head": "rpt-fx-head-sha", "job": "rpt-fx-job"},
            }),
        );
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }

        // (a) Delivery must NOT be via inbox_fallback.
        let dm = response["result"]["delivery_mode"]
            .as_str()
            .or_else(|| response["result"]["note"].as_str())
            .unwrap_or_default();
        // (b) Exactly 1 message in target's inbox (not 0, not 2).
        let target_msgs = crate::inbox::drain(&home, "rpt-fx-target");
        // (c) ack_by_correlation must have settled the sender's dispatch row.
        // describe_message is the direct status probe — drain would miss a
        // delivering row. ReadAt = settled.
        let parent_status = crate::inbox::describe_message(
            &home, "m-dispatch-fx", "rpt-fx-sender",
        );
        // (d) mark_completed must have cleared the seeded DispatchEntry.
        let remaining_tracking =
            crate::dispatch_tracking::take_pending_dispatchers_to(&home, "rpt-fx-sender");
        // (e) UX: exactly one total event, and it must be ReportResult.
        let ux_events = rec.0.lock().clone();
        let report_events: Vec<_> = ux_events.iter().filter(|e| {
            matches!(e, UxEvent::Fleet(crate::channel::ux_event::FleetEvent::ReportResult { .. }))
        }).collect();
        // (f) record_triaged_if_present must have written a discharge ledger entry.
        let has_discharge = crate::daemon::discharge_ledger::lookup_discharge(
            &home, "rpt-fx-head-sha", "rpt-fx-job",
        )
        .is_some();
        std::fs::remove_dir_all(&home).ok();

        assert!(
            !dm.contains("inbox_fallback") && !dm.contains("API unavailable"),
            "report via runtime=Some must deliver through the neutral service \
             (handle_send → settle_parent_after_successful_send, process_verdicts, \
             track_dispatch), not the socket fallback (delivery_mode={dm}): {response}"
        );
        assert_eq!(
            target_msgs.len(), 1,
            "report must produce exactly 1 inbox message for the target; got {}",
            target_msgs.len()
        );
        assert!(
            matches!(parent_status, crate::inbox::MessageStatus::ReadAt(..)),
            "ack_by_correlation must settle the sender's dispatch row to ReadAt; \
             got {:?}",
            parent_status
        );
        assert!(
            remaining_tracking.is_empty(),
            "mark_completed must clear the DispatchEntry for the sender; \
             {} rows remain",
            remaining_tracking.len()
        );
        assert_eq!(
            ux_events.len(), 1,
            "report must emit exactly 1 total UX event; got {}",
            ux_events.len()
        );
        assert_eq!(
            report_events.len(), 1,
            "the single UX event must be ReportResult; got {} ReportResult events",
            report_events.len()
        );
        assert!(
            has_discharge,
            "record_triaged_if_present must write a discharge ledger entry \
             for the triaged head/job"
        );
    }

    /// d-...-4 invariant 3b: a successful delegate (request_kind=task) via
    /// runtime=Some must deliver through the neutral service (not fallback),
    /// produce exactly 1 inbox message for the target, and create a
    /// dispatch tracking record. Currently adapter! strips runtime →
    /// delivery via inbox_fallback.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn send_delegate_service_delivery_with_post_effects_2454() {
        let _guard = crate::mcp::handlers::fleet_test_guard();
        let tag = format!(
            "del-fx-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let home = std::env::temp_dir().join(format!("agend-s12b2-{tag}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  del-fx-sender:\n    backend: claude\n  \
             del-fx-target:\n    backend: claude\n",
        )
        .unwrap();
        let registry: crate::agent::AgentRegistry = Default::default();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();

        // Register UX Recorder.
        use crate::channel::sink_registry::registry as ux_sink_registry;
        use crate::channel::ux_event::{UxEvent, UxEventSink};
        let rec = {
            struct Rec(parking_lot::Mutex<Vec<UxEvent>>);
            impl UxEventSink for Rec {
                fn emit(&self, event: &UxEvent) { self.0.lock().push(event.clone()); }
            }
            std::sync::Arc::new(Rec(parking_lot::Mutex::new(Vec::new())))
        };
        ux_sink_registry().clear_for_test();
        ux_sink_registry().register(rec.clone() as std::sync::Arc<dyn UxEventSink>);

        let previous_home = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &home);
        let response = invoke_runtime_mcp_tool(
            &home,
            &registry,
            &configs,
            &externals,
            "send",
            "del-fx-sender",
            json!({
                "instance": "del-fx-target",
                "request_kind": "task",
                "message": "delegate with post-effects",
                "task_id": "t-del-fx-001",
            }),
        );
        match previous_home {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }

        // (a) Delivery must NOT be via inbox_fallback.
        let result_str = response["result"].to_string();
        // (b) Exactly 1 message in target's inbox.
        let target_msgs = crate::inbox::drain(&home, "del-fx-target");
        // (c) Dispatch tracking: extract exact rows (take_pending_dispatchers_to
        // returns AND removes them, so len is the definitive count).
        let tracking_rows =
            crate::dispatch_tracking::take_pending_dispatchers_to(&home, "del-fx-target");
        // (d) UX: exactly one DelegateTask event.
        let ux_events = rec.0.lock().clone();
        let delegate_events: Vec<_> = ux_events.iter().filter(|e| {
            matches!(e, UxEvent::Fleet(crate::channel::ux_event::FleetEvent::DelegateTask { .. }))
        }).collect();
        std::fs::remove_dir_all(&home).ok();

        assert!(
            !result_str.contains("inbox_fallback") && !result_str.contains("API unavailable"),
            "delegate via runtime=Some must deliver through the neutral service \
             (handle_send → track_dispatch), not the socket fallback: {response}"
        );
        assert_eq!(
            target_msgs.len(), 1,
            "delegate must produce exactly 1 inbox message for the target; got {}",
            target_msgs.len()
        );
        assert_eq!(
            tracking_rows.len(), 1,
            "delegate must create exactly 1 dispatch tracking row for the target; got {}",
            tracking_rows.len()
        );
        assert_eq!(
            ux_events.len(), 1,
            "delegate must emit exactly 1 total UX event; got {}",
            ux_events.len()
        );
        assert_eq!(
            delegate_events.len(), 1,
            "the single UX event must be DelegateTask; got {} DelegateTask events",
            delegate_events.len()
        );
    }

    /// d-...-4 invariant 3c: source-owner guard — MCP adapter production
    /// code (comms.rs, comms_delegate/mod.rs) must NOT call API-service-
    /// level post-success functions, and API service (messaging.rs) must
    /// NOT call MCP-decorator functions. Prevents adapter+service duplicate
    /// tracking/settlement/receipt effects when the in-process path runs.
    #[test]
    fn send_post_success_owner_source_guard_2454() {
        let test_mod_marker = "#[cfg(test)]\nmod ";

        // SERVICE-level functions (owned by messaging.rs handle_send):
        // MCP adapters must NOT call these directly.
        let service_fns = [
            concat!("settle_parent_after", "_successful_send"),
            concat!("inject_", "provenance"),
            concat!("checkout_branch_if", "_requested"),
            concat!("process_", "verdicts"),
            concat!("track_", "dispatch"),
        ];

        // MCP DECORATOR functions (owned by comms.rs handlers):
        // API service must NOT call these.
        let decorator_fns = [
            concat!("dispatch_tracking::", "mark_completed"),
            concat!("ack_by_", "correlation"),
            concat!("record_triaged_if", "_present"),
        ];

        // Check: MCP adapters do NOT call service-level functions.
        let adapter_sources: &[(&str, &str)] = &[
            ("comms.rs", include_str!("../../mcp/handlers/comms.rs")),
            ("comms_delegate/mod.rs", include_str!("../../mcp/handlers/comms_delegate/mod.rs")),
        ];
        for (file, src) in adapter_sources {
            let boundary = src.rfind(test_mod_marker).unwrap_or(src.len());
            let production = &src[..boundary];
            for needle in &service_fns {
                for (ln, line) in production.lines().enumerate() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("//") || trimmed.starts_with("///") {
                        continue;
                    }
                    assert!(
                        !line.contains(needle),
                        "MCP adapter {file}:{} calls service-level function `{needle}` — \
                         this belongs to handle_send, not the MCP adapter. \
                         Duplicate effects will fire when the in-process path runs.",
                        ln + 1
                    );
                }
            }
        }

        // Check: API service does NOT call MCP-decorator functions.
        let svc_src = include_str!("messaging.rs");
        let svc_boundary = svc_src.rfind(test_mod_marker).unwrap_or(svc_src.len());
        let svc_production = &svc_src[..svc_boundary];
        for needle in &decorator_fns {
            for (ln, line) in svc_production.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") || trimmed.starts_with("///") {
                    continue;
                }
                assert!(
                    !line.contains(needle),
                    "API service messaging.rs:{} calls MCP-decorator function `{needle}` — \
                     this belongs to the MCP comms adapter, not handle_send. \
                     Duplicate effects will fire when the in-process path runs.",
                    ln + 1
                );
            }
        }
    }

    /// d-...-4 invariant 4: typed shared service boundary guard.
    /// The neutral typed SendRequest/SendOutcome service must live in
    /// src/agent_ops/messaging.rs, with a typed (non-raw-Value) entry point.
    /// Both the thin API SEND adapter (messaging.rs) and the MCP runtime
    /// SEND family must converge on this module. The neutral module must
    /// NOT depend on API-layer types (HandlerCtx, ConfigRegistry,
    /// ExternalRegistry, raw serde_json::Value service entry, crate::api).
    /// RED while the neutral service source is absent.
    #[test]
    fn send_typed_shared_service_boundary_guard_2454() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let neutral_path = std::path::Path::new(manifest_dir)
            .join("src/agent_ops/messaging.rs");

        // Gate: neutral service source must exist.
        assert!(
            neutral_path.exists(),
            "neutral typed service source src/agent_ops/messaging.rs must exist; \
             d-...-4 requires a typed SendRequest/SendOutcome service below \
             both the API and MCP adapters"
        );

        let src = std::fs::read_to_string(&neutral_path).unwrap();
        let test_mod_marker = "#[cfg(test)]\nmod ";
        let boundary = src.rfind(test_mod_marker).unwrap_or(src.len());
        let production = &src[..boundary];

        // Must expose typed request/outcome boundary (not raw Value).
        assert!(
            production.contains("SendRequest"),
            "neutral service must define SendRequest typed boundary"
        );
        assert!(
            production.contains("SendOutcome"),
            "neutral service must define SendOutcome typed boundary"
        );

        // Must have a typed service entry function (not raw Value → Value).
        let has_typed_entry = production.lines().any(|line| {
            let t = line.trim();
            !t.starts_with("//") && !t.starts_with("///")
                && (t.contains("SendRequest") && t.contains("SendOutcome")
                    && (t.contains("fn ") || t.contains("-> ")))
        });
        assert!(
            has_typed_entry,
            "neutral service must have a typed entry fn(SendRequest) -> SendOutcome; \
             a raw-Value send_with_runtime_or_legacy approach must not pass"
        );

        // Forbidden API-layer types — the neutral service must be
        // dependency-free from the API adapter layer.
        let forbidden = [
            "HandlerCtx",
            "ConfigRegistry",
            "ExternalRegistry",
            "crate::api",
        ];
        for needle in &forbidden {
            for (ln, line) in production.lines().enumerate() {
                let t = line.trim();
                if t.starts_with("//") || t.starts_with("///") {
                    continue;
                }
                assert!(
                    !line.contains(needle),
                    "neutral service src/agent_ops/messaging.rs:{} contains \
                     forbidden API-layer dependency `{needle}` — the neutral \
                     service must be below both API and MCP adapters",
                    ln + 1
                );
            }
        }

        // Forbidden: raw serde_json::Value as service entry parameter.
        let has_raw_value_entry = production.lines().any(|line| {
            let t = line.trim();
            !t.starts_with("//") && !t.starts_with("///")
                && t.contains("fn ")
                && (t.contains("Value") || t.contains("&serde_json"))
                && !t.contains("SendRequest") && !t.contains("SendOutcome")
        });
        assert!(
            !has_raw_value_entry,
            "neutral service must not have a raw Value / serde_json service \
             entry — use typed SendRequest/SendOutcome boundary"
        );

        // ── Convergence proof: both adapters reference this neutral owner ──

        let neutral_mod = concat!("crate::agent_ops::", "messaging");

        // (a) agent_ops.rs must register the messaging submodule.
        let ops_src = include_str!("../../agent_ops.rs");
        let ops_boundary = ops_src.rfind(test_mod_marker).unwrap_or(ops_src.len());
        let ops_production = &ops_src[..ops_boundary];
        let has_mod_decl = ops_production.lines().any(|line| {
            let t = line.trim();
            !t.starts_with("//") && !t.starts_with("///")
                && (t == "mod messaging;" || t == "pub mod messaging;"
                    || t == "pub(crate) mod messaging;")
        });
        assert!(
            has_mod_decl,
            "src/agent_ops.rs must register `mod messaging` submodule \
             to expose the neutral typed service"
        );

        // (b) API SEND adapter (src/api/handlers/messaging.rs) must
        // reference the neutral owner.
        let api_svc_src = include_str!("messaging.rs");
        let api_boundary = api_svc_src.rfind(test_mod_marker).unwrap_or(api_svc_src.len());
        let api_production = &api_svc_src[..api_boundary];
        let api_refs_neutral = api_production.lines().any(|line| {
            let t = line.trim();
            !t.starts_with("//") && !t.starts_with("///")
                && line.contains(neutral_mod)
        });
        assert!(
            api_refs_neutral,
            "API SEND adapter (api/handlers/messaging.rs) must reference \
             {neutral_mod} — both adapters must converge on the neutral owner"
        );

        // (c) MCP runtime SEND adapter sources must reference the neutral
        // owner at least once (behavioral REDs already prove report/delegate
        // paths, so this is a structural convergence check only).
        let mcp_adapter_sources: &[(&str, &str)] = &[
            ("comms.rs", include_str!("../../mcp/handlers/comms.rs")),
            ("comms_delegate/mod.rs", include_str!("../../mcp/handlers/comms_delegate/mod.rs")),
            ("dispatch.rs", include_str!("../../mcp/handlers/dispatch.rs")),
        ];
        let mut mcp_refs_neutral = false;
        for (_file, src) in mcp_adapter_sources {
            let b = src.rfind(test_mod_marker).unwrap_or(src.len());
            let prod = &src[..b];
            if prod.lines().any(|line| {
                let t = line.trim();
                !t.starts_with("//") && !t.starts_with("///")
                    && line.contains(neutral_mod)
            }) {
                mcp_refs_neutral = true;
                break;
            }
        }
        assert!(
            mcp_refs_neutral,
            "MCP SEND adapter sources (comms.rs, comms_delegate/mod.rs, dispatch.rs) \
             must reference {neutral_mod} at least once — both adapters must converge \
             on the neutral owner"
        );
    }
}
