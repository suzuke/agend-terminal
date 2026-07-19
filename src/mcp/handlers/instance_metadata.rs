use crate::agent_ops::{save_metadata, save_metadata_batch};
use crate::identity::Sender;
use crate::mcp::handlers::dispatch::RuntimeContext;
use serde_json::{json, Value};
use std::path::Path;

use super::err_needs_identity;

/// #2050 simplify PR-B (⑩): shared body for the display-name / description
/// metadata setters (byte-identical to the former inline code). Invoked via
/// the `set_metadata` action-based tool (#2547: merged from the former
/// standalone `set_display_name` / `set_description` tools) as
/// `action=display_name` / `action=description`.
///
/// #1604 semantics: an empty `value` = explicit CLEAR — store `null` (so the
/// reader's `Option` is `None`, falling back to the default) and return
/// `{"cleared": true}`. Over `max_len` (BYTES — `str::len`, unchanged) →
/// `{"error": "<attr> exceeds <max_len> character limit"}`. Otherwise persist and
/// echo `{"<attr>": value}`. Exactly one `save_metadata` write executes per call.
fn set_string_attr(
    home: &Path,
    instance_name: &str,
    value: &str,
    attr: &str,
    max_len: usize,
) -> Value {
    if value.is_empty() {
        save_metadata(home, instance_name, attr, json!(null));
        return json!({"cleared": true});
    }
    if value.len() > max_len {
        return json!({"error": format!("{attr} exceeds {max_len} character limit")});
    }
    save_metadata(home, instance_name, attr, json!(value));
    json!({ attr: value })
}

pub(super) fn handle_set_display_name(home: &Path, args: &Value, instance_name: &str) -> Value {
    let display_name = args["name"].as_str().unwrap_or("");
    set_string_attr(home, instance_name, display_name, "display_name", 256)
}

pub(super) fn handle_set_description(home: &Path, args: &Value, instance_name: &str) -> Value {
    let desc = args["description"].as_str().unwrap_or("");
    set_string_attr(home, instance_name, desc, "description", 1024)
}

/// #2454 S4: MCP interrupt uses the neutral `agent_ops::inject_input`
/// service in-process via RuntimeContext — no api::call loopback.
pub(super) fn handle_interrupt(
    home: &Path,
    args: &Value,
    runtime: Option<&RuntimeContext>,
) -> Value {
    let target = match super::require_instance(args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(target);
    let Some(runtime) = runtime else {
        return json!({"error": "runtime unavailable: interrupt requires the in-process daemon runtime"});
    };
    match crate::agent_ops::inject_input(
        &runtime.registry,
        &runtime.externals,
        home,
        target,
        b"\x1b",
        true,
    ) {
        Ok(_) => {
            if let Some(reason) = args["reason"].as_str() {
                let header = crate::inbox::format_event_header("interrupt", &[("reason", reason)]);
                crate::inbox::compose_aware_inject(home, target, &header);
            }
            let mut result = json!({"ok": true, "target": target});
            if args["snapshot"].as_bool() == Some(true) {
                if let Some(text) =
                    crate::agent_ops::pane_scrollback(&runtime.registry, home, target, 40)
                {
                    result["snapshot"] = json!(text);
                }
            }
            result
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

pub(super) fn handle_set_waiting_on(
    home: &Path,
    args: &Value,
    instance_name: &str,
    sender: &Option<Sender>,
) -> Value {
    let Some(_) = sender.as_ref() else {
        return err_needs_identity("set_waiting_on");
    };
    let condition = args["condition"].as_str().unwrap_or("");
    if condition.is_empty() {
        crate::daemon::heartbeat_pair::update_with(instance_name, |p| {
            p.waiting_on_since_ms = None;
        });
        save_metadata_batch(
            home,
            instance_name,
            &[
                ("waiting_on", json!(null)),
                ("waiting_on_since", json!(null)),
            ],
        );
        json!({"cleared": true})
    } else {
        let now_ms = crate::daemon::heartbeat_pair::now_ms();
        crate::daemon::heartbeat_pair::update_with(instance_name, |p| {
            p.heartbeat_at_ms = now_ms;
            p.waiting_on_since_ms = Some(now_ms);
        });
        let now = chrono::Utc::now().to_rfc3339();
        save_metadata_batch(
            home,
            instance_name,
            &[
                ("waiting_on", json!(condition)),
                ("waiting_on_since", json!(&now)),
            ],
        );
        json!({"waiting_on": condition, "since": now})
    }
}

pub(super) fn handle_move_pane(
    home: &Path,
    args: &Value,
    runtime: Option<&RuntimeContext>,
) -> Value {
    let Some(runtime) = runtime else {
        return json!({"error": "runtime unavailable: move_pane requires the in-process daemon runtime"});
    };
    let event = match crate::agent_ops::move_pane(
        home,
        args["instance"].as_str(),
        args["target_tab"].as_str(),
        args["split_dir"].as_str(),
    ) {
        Ok(event) => event,
        Err(error) => return json!({"error": error}),
    };
    if let Some(notifier) = runtime.notifier.as_ref() {
        notifier.notify(crate::api::ApiEvent::PaneMoved {
            agent: event.agent.clone(),
            target_tab: event.target_tab.clone(),
            split_dir: match event.split_dir {
                crate::agent_ops::PaneMoveSplit::Horizontal => {
                    crate::api::PaneMoveSplitDir::Horizontal
                }
                crate::agent_ops::PaneMoveSplit::Vertical => crate::api::PaneMoveSplitDir::Vertical,
            },
        });
    }
    json!({
        "ok": true,
        "instance": event.agent,
        "target_tab": event.target_tab,
    })
}

pub(super) fn handle_pane_snapshot(
    home: &Path,
    args: &Value,
    runtime: Option<&RuntimeContext>,
) -> Value {
    let target = match super::require_instance(args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(target);
    let lines_u64 = args["lines"].as_u64().unwrap_or(100);
    // MCP keeps the stricter explicit >10k reject (the API adapter clamps via
    // min(10_000)); M1: explicit bounds check before the u64→usize cast.
    if lines_u64 > 10000 {
        return json!({"error": "lines must be <= 10000 (scrolling_history limit)"});
    }
    let lines = lines_u64 as usize;
    // #2454: in-process read via the forwarded live registry — no api::call
    // loopback. runtime is absent only on the test-only handle_tool entry.
    let Some(runtime) = runtime else {
        return json!({"error": "runtime unavailable: pane_snapshot requires the in-process daemon runtime"});
    };
    match crate::agent_ops::pane_scrollback(&runtime.registry, home, target, lines) {
        Some(full) => {
            // #2478: `to_file` diagnostic-capture mode (unchanged) — write the full
            // snapshot to a capture file and return a compact summary instead of
            // flooding the agent's context.
            if args["to_file"].as_bool().unwrap_or(false) {
                return pane_snapshot_to_file(home, target, &full, args);
            }
            json!({"ok": true, "text": full})
        }
        None => json!({"error": format!("instance '{target}' not found")}),
    }
}

/// #2478: persist a full pane snapshot to `<home>/captures/` and return a compact
/// summary instead of the raw dump. Keeps a heavy diagnostic capture out of the
/// agent's context while preserving the full bytes on disk for opt-in inspection.
fn pane_snapshot_to_file(home: &Path, target: &str, full: &str, args: &Value) -> Value {
    let head_lines = args["head"].as_u64().unwrap_or(40) as usize;
    let dir = home.join("captures");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        // Fall back to returning the text rather than losing the capture.
        return json!({
            "ok": true,
            "text": full,
            "warning": format!("to_file requested but captures dir unavailable: {e}"),
        });
    }
    let epoch_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let safe_target: String = target
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let path = dir.join(format!("pane-{safe_target}-{epoch_ms}.txt"));
    if let Err(e) = std::fs::write(&path, full) {
        return json!({
            "ok": true,
            "text": full,
            "warning": format!("to_file requested but write failed: {e}"),
        });
    }
    let all_lines: Vec<&str> = full.lines().collect();
    let total_lines = all_lines.len();
    let head: String = all_lines
        .iter()
        .take(head_lines)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let tail: String = all_lines
        .iter()
        .rev()
        .take(5)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    json!({
        "ok": true,
        "captured_to": path.display().to_string(),
        "bytes": full.len(),
        "lines": total_lines,
        "head": head,
        "tail": tail,
        "note": "full capture written to file; read it (or hexdump in scratch) only if the summary is insufficient — kept out of context per #2478",
    })
}

pub(super) fn handle_report_health(
    home: &Path,
    args: &Value,
    instance_name: &str,
    sender: &Option<Sender>,
    runtime: Option<&RuntimeContext>,
) -> Value {
    let Some(_) = sender.as_ref() else {
        return err_needs_identity("report_health");
    };
    let reason_str = match args["reason"].as_str() {
        Some(r) => r,
        None => return json!({"error": "missing 'reason'"}),
    };
    // #2454: write IN-PROCESS via the forwarded live registry — no api::call
    // loopback. `runtime` is absent only on the test-only `handle_tool` entry
    // (production is always mcp_proxy → execute_tool_with_runtime(Some)); return
    // an explicit error, never fall back to api::call.
    let Some(runtime) = runtime else {
        return json!({"error": "runtime unavailable: health write requires the in-process daemon runtime"});
    };
    // Parse the kind (+ payload) at the boundary; unknown kind keeps the API's
    // `unknown reason: <kind>` message. #1933: `note` forwarded (empty → none).
    let Some(reason) = crate::health::BlockedReason::parse_kind(reason_str, args) else {
        return json!({"error": format!("unknown reason: {reason_str}")});
    };
    let note = args["note"].as_str();
    match crate::agent_ops::set_blocked_reason(&runtime.registry, home, instance_name, reason, note)
    {
        Some(out) => json!({
            "status": "reason_set",
            "reason": reason_str,
            "current_state": out.current_state
        }),
        None => json!({"error": format!("instance '{instance_name}' not found")}),
    }
}

pub(super) fn handle_clear_blocked_reason(
    home: &Path,
    args: &Value,
    runtime: Option<&RuntimeContext>,
) -> Value {
    let instance = match super::require_instance(args) {
        Ok(n) => n,
        Err(e) => return e,
    };
    // CR-2026-06-14: validate the instance name at the MCP boundary before the
    // write, mirroring the sibling handlers in this file (handle_interrupt /
    // handle_pane_snapshot). Without it a malformed name (`../evil`) was
    // forwarded straight to the daemon.
    crate::validate_name_or_err!(instance);
    // #2454: in-process clear via the forwarded live registry (no `api::call`).
    let Some(runtime) = runtime else {
        return json!({"error": "runtime unavailable: health write requires the in-process daemon runtime"});
    };
    // `filter` is a reason-KIND token; unknown kind is a legal never-match filter.
    let filter = args["reason"].as_str();
    match crate::agent_ops::clear_blocked_reason(&runtime.registry, home, instance, filter) {
        Ok(out) => {
            let was = out
                .was
                .as_ref()
                .map(|r| serde_json::to_value(r).unwrap_or_default());
            json!({"status": "cleared", "instance": instance, "was": was})
        }
        // Match the pre-migration MCP shape: surface only the error string.
        Err(crate::agent_ops::ClearBlockedError::FilterMismatch { .. }) => {
            json!({"error": "reason mismatch"})
        }
        Err(crate::agent_ops::ClearBlockedError::NotFound) => {
            json!({"error": format!("instance '{instance}' not found")})
        }
    }
}

// --- Private helpers (moved from mod.rs) ---

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod snapshot_file_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pane_snapshot_to_file_returns_summary_not_full_text_2478() {
        let home = std::env::temp_dir().join(format!(
            "agend-pane-snap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let full = (0..80)
            .map(|i| format!("line-{i:02}-{}", "x".repeat(40)))
            .collect::<Vec<_>>()
            .join("\n");
        let resp = pane_snapshot_to_file(&home, "dev/1", &full, &json!({"head": 3}));
        assert_eq!(resp["ok"], true);
        let path = resp["captured_to"].as_str().expect("path");
        assert_eq!(std::fs::read_to_string(path).unwrap(), full);
        assert_eq!(resp["lines"], 80);
        assert!(resp["head"].as_str().unwrap().contains("line-00"));
        assert!(!resp["head"].as_str().unwrap().contains("line-10"));
        assert!(
            resp.get("text").is_none(),
            "summary mode must not return full text"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}

/// #2454: deterministic no-daemon tests that the MCP `health` write handlers
/// mutate the live registry via the forwarded `RuntimeContext` (in-process) and
/// return an explicit error — never `api::call` — when it is absent. `unix`-gated
/// (`mk_test_handle` is `cfg(all(test, unix))`).
#[cfg(all(test, unix))]
mod blocked_reason_runtime_2454_tests {
    use super::*;
    use crate::agent::{self, mk_test_handle};
    use crate::health::BlockedReason;
    use crate::mcp::handlers::dispatch::RuntimeContext;
    use crate::types::InstanceId;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// Sole fixture: a temp home + fleet.yaml + a `true`-backed registry handle.
    fn runtime_with_agent(name: &str) -> (RuntimeContext, std::path::PathBuf) {
        let home = std::env::temp_dir().join(format!(
            "agend-blocked-mcp-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&home).expect("create tmp home");
        let id = InstanceId::new();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!("instances:\n  {name}:\n    id: {}\n", id.full()),
        )
        .expect("seed fleet.yaml");
        let mut handle = mk_test_handle(name, id);
        handle.pty_writer = Arc::new(Mutex::new(Box::new(std::io::sink())));
        let rt = RuntimeContext {
            registry: Arc::new(Mutex::new(HashMap::from([(id, handle)]))),
            configs: Default::default(),
            externals: Arc::new(Mutex::new(HashMap::new())),
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: None,
            notifier: None,
            shutdown: None,
        };
        (rt, home)
    }

    fn current_reason(rt: &RuntimeContext, home: &Path, name: &str) -> Option<BlockedReason> {
        let guard = agent::lock_registry(&rt.registry);
        let id = crate::fleet::resolve_uuid(home, name).expect("resolve uuid");
        let handle = guard.get(&id).expect("handle in registry");
        let core = handle.core.lock();
        core.health.current_reason.clone()
    }

    // (a) set then clear via the runtime registry mutate the in-mem handle, no
    // daemon / socket.
    #[test]
    fn set_then_clear_via_runtime_mutate_registry() {
        let (rt, home) = runtime_with_agent("agent-x");
        let sender = Sender::new("agent-x");
        let set = handle_report_health(
            &home,
            &json!({"reason": "hang"}),
            "agent-x",
            &sender,
            Some(&rt),
        );
        assert_eq!(set["status"].as_str(), Some("reason_set"), "{set}");
        assert_eq!(
            current_reason(&rt, &home, "agent-x"),
            Some(BlockedReason::Hang)
        );

        let cleared =
            handle_clear_blocked_reason(&home, &json!({"instance": "agent-x"}), Some(&rt));
        assert_eq!(cleared["status"].as_str(), Some("cleared"), "{cleared}");
        assert_eq!(current_reason(&rt, &home, "agent-x"), None);
    }

    // (b) runtime=None is an explicit error, never an api::call fallback — for
    // both write actions.
    #[test]
    fn runtime_none_never_falls_back() {
        let home = std::env::temp_dir().join("agend-blocked-mcp-none");
        let sender = Sender::new("agent-x");
        let report =
            handle_report_health(&home, &json!({"reason": "hang"}), "agent-x", &sender, None);
        let clear = handle_clear_blocked_reason(&home, &json!({"instance": "agent-x"}), None);
        for (label, result) in [("report", &report), ("clear", &clear)] {
            let err = result["error"].as_str().unwrap_or_default();
            assert!(
                err.contains("runtime unavailable"),
                "{label}: runtime=None must be an explicit error, not an api::call fallback: {result}"
            );
        }
    }

    // (c) an unknown (never-matching) filter kind does NOT clear — pins the
    // unknown-filter compatibility (no silent unconditional clear).
    #[test]
    fn unknown_filter_does_not_clear() {
        let (rt, home) = runtime_with_agent("agent-x");
        let sender = Sender::new("agent-x");
        handle_report_health(
            &home,
            &json!({"reason": "hang"}),
            "agent-x",
            &sender,
            Some(&rt),
        );
        let result = handle_clear_blocked_reason(
            &home,
            &json!({"instance": "agent-x", "reason": "bogus_kind"}),
            Some(&rt),
        );
        assert_eq!(
            result["error"].as_str(),
            Some("reason mismatch"),
            "{result}"
        );
        assert_eq!(
            current_reason(&rt, &home, "agent-x"),
            Some(BlockedReason::Hang)
        );
    }

    // #2454 pane_snapshot family: the standalone tool + the interrupt-chained
    // snapshot read PTY scrollback IN-PROCESS via the forwarded RuntimeContext
    // (no api::call). Reuses the module's `runtime_with_agent` fixture.
    #[test]
    fn pane_snapshot_reads_via_runtime_registry_no_daemon() {
        let (rt, home) = runtime_with_agent("agent-x");
        // In-process read succeeds with NO daemon (an api::call loopback would error).
        let ok = handle_pane_snapshot(&home, &json!({"instance": "agent-x"}), Some(&rt));
        assert_eq!(
            ok["ok"].as_bool(),
            Some(true),
            "runtime read must succeed no-daemon: {ok}"
        );
        assert!(ok["text"].is_string(), "must return scrollback text: {ok}");
        // runtime=None → explicit error, never an api::call fallback.
        let none = handle_pane_snapshot(&home, &json!({"instance": "agent-x"}), None);
        assert!(
            none["error"]
                .as_str()
                .unwrap_or_default()
                .contains("runtime unavailable"),
            "runtime=None must be an explicit error: {none}"
        );
    }

    #[test]
    fn interrupt_snapshot_runs_in_process_and_runtime_none_errors() {
        let (rt, home) = runtime_with_agent("agent-x");
        let with_snap = handle_interrupt(
            &home,
            &json!({"instance": "agent-x", "snapshot": true}),
            Some(&rt),
        );
        assert_eq!(with_snap["ok"].as_bool(), Some(true), "{with_snap}");
        assert!(
            with_snap["snapshot"].is_string(),
            "snapshot must be populated in-process: {with_snap}"
        );
        let no_rt = handle_interrupt(
            &home,
            &json!({"instance": "agent-x", "snapshot": true}),
            None,
        );
        assert!(
            no_rt["error"]
                .as_str()
                .unwrap_or_default()
                .contains("runtime"),
            "runtime=None must be an explicit error: {no_rt}"
        );
        let missing = handle_interrupt(&home, &json!({"instance": "missing"}), Some(&rt));
        assert_eq!(
            missing["error"].as_str(),
            Some("agent 'missing' not found"),
            "unknown target must surface exact domain error: {missing}"
        );
    }

    /// #2454 S4 immutable RED: `handle_interrupt` with a live
    /// RuntimeContext (seeded fleet.yaml/UUID) must succeed without a
    /// daemon. This test is IMMUTABLE — GREEN makes it pass without
    /// editing this assertion.
    #[test]
    fn interrupt_uses_runtime_context_without_api_listener_2454() {
        let (rt, home) = runtime_with_agent("agent-x");
        let result = handle_interrupt(&home, &json!({"instance": "agent-x"}), Some(&rt));
        assert!(
            result.get("error").is_none(),
            "interrupt via runtime must succeed without a daemon; got: {result}"
        );
        assert_eq!(
            result["ok"].as_bool(),
            Some(true),
            "interrupt must return ok:true on success: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2454 S4 RED source invariant: the MCP interrupt region
    /// (handle_interrupt through handle_set_waiting_on) must contain
    /// ZERO `api::call` references.  Scoped to exclude MOVE_PANE.
    #[test]
    fn no_api_call_in_interrupt_region_2454() {
        let source = include_str!("instance_metadata.rs");
        let start_marker = concat!("pub(super) fn handle_", "interrupt(");
        let end_marker = concat!("pub(super) fn handle_set_", "waiting_on(");
        let start = source.find(start_marker).expect("interrupt region start");
        let end = source[start..]
            .find(end_marker)
            .map(|p| p + start)
            .expect("interrupt region end");
        let region = &source[start..end];
        let needle_a = concat!("crate::", "api::", "call");
        let needle_b = concat!("api::", "call(");
        let needle_c = concat!("api::", "call)");
        let violations: Vec<&str> = region
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.starts_with("//") && !trimmed.starts_with("///")
            })
            .filter(|line| {
                line.contains(needle_a) || line.contains(needle_b) || line.contains(needle_c)
            })
            .collect();
        assert!(
            violations.is_empty(),
            "MCP interrupt region must contain zero api::call; found {}: {:?}",
            violations.len(),
            violations
        );
    }

    /// #2454 S4 RED shared-service invariant: after GREEN, both the MCP
    /// interrupt region AND the API handle_inject adapter must reference
    /// `agent_ops::inject_input` — the frozen neutral service name.
    /// Currently neither does.
    #[test]
    fn shared_inject_input_service_referenced_by_both_callers_2454() {
        let service = concat!("agent_ops::", "inject_input");
        let mcp_src = include_str!("instance_metadata.rs");
        let mcp_start = mcp_src
            .find(concat!("pub(super) fn handle_", "interrupt("))
            .expect("MCP interrupt start");
        let mcp_end = mcp_src[mcp_start..]
            .find(concat!("pub(super) fn handle_set_", "waiting_on("))
            .map(|p| p + mcp_start)
            .expect("MCP interrupt end");
        let mcp_region = &mcp_src[mcp_start..mcp_end];
        let mcp_has = mcp_region.contains(service);

        let api_src = include_str!("../../api/handlers/instance.rs");
        let api_start = api_src
            .find(concat!("pub(crate) fn handle_", "inject("))
            .expect("API handle_inject start");
        let api_end_marker = api_src[api_start..]
            .find("\npub")
            .map(|p| p + api_start)
            .unwrap_or(api_src.len());
        let api_region = &api_src[api_start..api_end_marker];
        let api_has = api_region.contains(service);

        assert!(
            mcp_has && api_has,
            "both MCP interrupt and API handle_inject must reference the neutral \
             agent_ops::inject_input service; MCP={mcp_has}, API={api_has}"
        );
    }
}
