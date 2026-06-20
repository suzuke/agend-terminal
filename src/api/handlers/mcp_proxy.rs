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

/// Fast read-only / atomic-flip tools (~5s). Every name is a registered MCP
/// tool — `tool_timeout_keys_are_registered_tools` pins this against
/// `registry::all()` so a consolidation/rename can't silently leave a stale
/// (never-matching) entry here again.
const FAST_TOOLS: &[&str] = &[
    "inbox",
    "list_instances",
    "set_waiting_on",
    "set_display_name",
    "set_description",
    "health",
];

/// Tools whose slowest action spawns a process / does network I/O (~60s).
///
/// #2050 W1.3①: `deployment` / `ci` / `repo` are the action-consolidated
/// successors of the old `deploy_template` / `watch_ci` / `checkout_repo`
/// names. Those stale names stopped matching after the consolidation, so these
/// genuinely-long operations (deploy, CI watch, repo checkout/merge) were
/// silently falling back to the 30s default — a false-timeout risk this fix
/// closes. #1814: `restart_daemon` spawns a successor + runs a ≤30s Phase-1
/// gate. `team` (create) spawns agents like `create_instance` (and mirrors the
/// `method_wait_timeout` CREATE_TEAM 60s band).
const SLOW_TOOLS: &[&str] = &[
    "create_instance",
    "replace_instance",
    "restart_daemon",
    "deployment",
    "ci",
    "repo",
    "team",
];

pub(crate) fn tool_timeout(tool: &str) -> Duration {
    if FAST_TOOLS.contains(&tool) {
        FAST_TOOL_TIMEOUT
    } else if SLOW_TOOLS.contains(&tool) {
        SLOW_TOOL_TIMEOUT
    } else {
        DEFAULT_TOOL_TIMEOUT
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
    // never hit, so every agent saw all 36 tools.
    let inst = params
        .get("instance")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let role_kind = match inst {
        // No instance (old bridge / non-agent caller) → default-all-open.
        None => None,
        Some(inst) => {
            // #2344 (r6 #2367 reject + lead nuance): distinguish a MISSING fleet.yaml
            // from a present-but-malformed one. The old `FleetConfig::load(..).ok()`
            // collapsed BOTH to `None` → the full 36-tool surface, which let a typo'd
            // `role_kind: supervisor` FAIL strict parse yet still advertise every tool
            // — the D2 strict-deny hole r6 found.
            let fleet_path = crate::fleet::fleet_yaml_path(ctx.home);
            if !fleet_path.exists() {
                // (1) No fleet.yaml at all — a common setup (dev / fresh / local
                // shell). Stay ALL-OPEN; a blanket fail-closed would regress every
                // no-fleet daemon.
                None
            } else {
                match crate::fleet::FleetConfig::load(&fleet_path) {
                    // (2) Present but FAILS to load (parse error / malformed
                    // role_kind) → FAIL CLOSED. Surface the error loudly (operator
                    // fixes the config); never silently advertise the full surface.
                    Err(e) => {
                        return json!({
                            "ok": false,
                            "error": format!(
                                "tools/list: fleet.yaml is present but failed to load \
                                 (refusing to advertise the full tool surface — fix the \
                                 config): {e:#}"
                            ),
                        });
                    }
                    // (3) Loaded OK → the instance's role_kind. An unknown instance OR
                    // an absent role_kind → all-open (the unchanged opt-in contract).
                    Ok(fleet) => fleet.instances.get(inst).and_then(|i| i.role_kind),
                }
            }
        }
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

    /// #2050 W1.3① coverage invariant: every tool the timeout map classifies
    /// MUST be a registered MCP tool. This is the closure that prevents the
    /// stale-name drift this PR fixed (`deploy_template`/`watch_ci`/
    /// `checkout_repo`/`describe_*`/`list_*`/`react`/`report_health` had all
    /// gone stale after consolidation, silently degrading their tools to the
    /// 30s default). A future rename/removal now fails CI here instead.
    #[test]
    fn tool_timeout_keys_are_registered_tools() {
        use std::collections::HashSet;
        let registered: HashSet<&str> =
            crate::mcp::registry::all().iter().map(|t| t.name).collect();
        for &t in FAST_TOOLS.iter().chain(SLOW_TOOLS.iter()) {
            assert!(
                registered.contains(t),
                "tool_timeout classifies '{t}' but it is not a registered MCP tool \
                 (registry::all) — stale after a tool consolidation/rename? Update the \
                 name (or add a documented retired-allowlist). #2050 W1.3①"
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

    /// #2344 e2e: `handle_mcp_tools_list` subsets the surface for a fleet instance
    /// whose `role_kind` is a read/report role — the live path the MCP bridge hits.
    /// A reviewer sees fewer than the full 36 tools; no instance → all-open.
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

    /// #2344 D2 STRICT on the live bridge path (r6 #2367 reject): a fleet.yaml that
    /// is PRESENT but malformed (e.g. an unknown `role_kind`) must FAIL CLOSED in
    /// `handle_mcp_tools_list` — NOT silently fall back to the full 36-tool surface
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
}
