//! Instance lifecycle handlers: INJECT, KILL, DELETE, SPAWN.

use super::HandlerCtx;
use crate::agent;
use crate::api::{ApiEvent, LayoutHint, PaneMoveSplitDir};
use serde_json::{json, Value};

pub(crate) fn handle_inject(params: &Value, ctx: &HandlerCtx) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    if let Err(e) = agent::validate_name(name) {
        return json!({"ok": false, "error": e});
    }
    let data = params["data"].as_str().unwrap_or("");
    let raw = params["raw"].as_bool().unwrap_or(false);
    let reg = agent::lock_registry(ctx.registry);
    match reg.get(name) {
        Some(handle) => {
            let is_restarting = handle.core.lock().state.current.is_unavailable();
            if is_restarting {
                json!({"ok": false, "error": format!("agent '{name}' is restarting, retry later")})
            } else {
                let result = if raw {
                    agent::write_to_agent(handle, data.as_bytes())
                } else {
                    agent::inject_to_agent(handle, data.as_bytes())
                };
                match result {
                    Ok(()) => json!({"ok": true, "result": {"bytes": data.len()}}),
                    Err(e) => json!({"ok": false, "error": format!("{e}")}),
                }
            }
        }
        None => {
            let ext = agent::lock_external(ctx.externals);
            if ext.contains_key(name) {
                json!({"ok": false, "error": format!("agent '{name}' is external — use send instead of inject")})
            } else {
                json!({"ok": false, "error": format!("agent '{name}' not found")})
            }
        }
    }
}

pub(crate) fn handle_kill(params: &Value, ctx: &HandlerCtx) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    if let Err(e) = agent::validate_name(name) {
        return json!({"ok": false, "error": e});
    }
    let reg = agent::lock_registry(ctx.registry);
    match reg.get(name) {
        Some(handle) => {
            {
                let mut core = handle.core.lock();
                core.state.set_restarting();
            }
            let mut child = handle.child.lock();
            // Kill the process group (not just the leader) to propagate to
            // child subprocesses (kiro-cli spawns bun/mcp/acp children).
            if let Some(pid) = child.process_id() {
                crate::process::kill_process_tree(pid);
            }
            let _ = child.kill(); // also kill via PTY handle as fallback
            drop(child);
            drop(reg);
            crate::event_log::log(ctx.home, "kill", name, "killed via API");
            json!({"ok": true})
        }
        None => {
            drop(reg);
            let mut ext = agent::lock_external(ctx.externals);
            if ext.remove(name).is_some() {
                crate::event_log::log(ctx.home, "kill", name, "external agent removed");
                json!({"ok": true})
            } else {
                json!({"ok": false, "error": format!("agent '{name}' not found")})
            }
        }
    }
}

pub(crate) fn handle_delete(params: &Value, ctx: &HandlerCtx) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    if let Err(e) = agent::validate_name(name) {
        return json!({"ok": false, "error": e});
    }
    // Check external registry first
    {
        let mut ext = agent::lock_external(ctx.externals);
        if ext.remove(name).is_some() {
            crate::event_log::log(ctx.home, "delete", name, "external agent deleted");
            return json!({"ok": true});
        }
    }
    // delete_transaction kills the process tree, waits up to CHILD_EXIT_TIMEOUT
    // for actual exit, then removes registry / drops Telegram binding /
    // removes configs / removes IPC port / emits event log. Sprint 20 F2 fix:
    // the previous implementation removed the registry entry before the OS
    // had reaped the PID, exposing PID re-use + concurrent-spawn collision
    // races.
    crate::daemon::lifecycle::delete_transaction(ctx.home, name, ctx.registry, Some(ctx.configs));
    if let Some(n) = ctx.notifier {
        tracing::info!(agent = name, "DELETE emitting InstanceDeleted");
        n.notify(ApiEvent::InstanceDeleted {
            name: name.to_string(),
        });
    }
    // Announce the removal to every survivor. Must run AFTER the target
    // is removed from `ctx.registry` above, so compute_targets doesn't
    // try to inject the marker back into the dying agent's PTY.
    crate::fleet_broadcast::broadcast(
        ctx.home,
        ctx.registry,
        &crate::fleet_broadcast::FleetUpdate::InstanceDeleted {
            name: name.to_string(),
        },
    );
    json!({"ok": true})
}

#[allow(clippy::too_many_lines)]
pub(crate) fn handle_spawn(params: &Value, ctx: &HandlerCtx) -> Value {
    let name = match params["name"].as_str() {
        Some(n) => n,
        None => return json!({"ok": false, "error": "missing name"}),
    };
    if let Err(e) = agent::validate_name(name) {
        return json!({"ok": false, "error": e});
    }
    if agent::lock_registry(ctx.registry).contains_key(name) {
        return json!({"ok": false, "error": format!("agent '{name}' already exists")});
    }
    let command = params["backend"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| {
            crate::fleet::FleetConfig::load(&ctx.home.join("fleet.yaml"))
                .ok()
                .and_then(|f| f.defaults.backend.map(|b| b.preset().command.to_string()))
                .unwrap_or_else(|| "claude".to_string())
        });
    let command = command.as_str();
    let args: Vec<String> = params["args"]
        .as_str()
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default();
    let requested_work_dir = params["working_directory"]
        .as_str()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ctx.home.join("workspace").join(name));
    let work_dir = match crate::api::validate_working_directory(&requested_work_dir, ctx.home) {
        Ok(p) => p,
        Err(e) => return json!({"ok": false, "error": format!("{e}")}),
    };
    let size = crossterm::terminal::size().unwrap_or((120, 40));
    let spawn_mode = match params["mode"].as_str() {
        Some("resume") => crate::backend::SpawnMode::Resume,
        _ => crate::backend::SpawnMode::Fresh,
    };

    // Generate instructions before spawn — see api::handlers::prepare_instructions
    // for why ordering matters (backend flag-build time file presence check).
    let explicit_role = params
        .get("role")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    super::prepare_instructions(ctx.home, name, command, &work_dir, explicit_role);

    match crate::api::spawn_one(
        ctx.home,
        ctx.registry,
        name,
        command,
        &args,
        spawn_mode,
        &work_dir,
        size,
    ) {
        Ok(spawn_mode) => {
            // Every API-level spawn gets a channel topic (no-op when
            // no channel is configured). Routes through the Channel trait
            // so this handler is channel-agnostic.
            let topic_id = crate::channel::active_channel()
                .and_then(|ch| ch.create_topic(name).ok())
                .map(|t| t.id);
            if let Some(n) = ctx.notifier {
                let layout_hint = LayoutHint::parse(params["layout"].as_str().unwrap_or("tab"));
                let spawner = params["spawner"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                let target_pane = params["target_pane"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                tracing::info!(
                    agent = name,
                    layout = ?layout_hint,
                    spawner = ?spawner,
                    target_pane = ?target_pane,
                    "SPAWN emitting InstanceCreated"
                );
                n.notify(ApiEvent::InstanceCreated {
                    name: name.to_string(),
                    layout: layout_hint,
                    spawner,
                    target_pane,
                });
            }
            // Tell every other running agent about the new member, unless
            // this was a Resume spawn (returning agent, not a brand-new
            // fleet joiner — peers already know about it from their own
            // agend.md snapshots, so broadcasting would just generate
            // noise on every daemon restart).
            if matches!(spawn_mode, crate::backend::SpawnMode::Fresh) {
                let role_owned = explicit_role.map(str::to_string).or_else(|| {
                    crate::fleet::FleetConfig::load(&ctx.home.join("fleet.yaml"))
                        .ok()
                        .and_then(|f| f.instances.get(name).and_then(|c| c.role.clone()))
                });
                crate::fleet_broadcast::broadcast(
                    ctx.home,
                    ctx.registry,
                    &crate::fleet_broadcast::FleetUpdate::InstanceCreated {
                        name: name.to_string(),
                        backend: command.to_string(),
                        role: role_owned,
                    },
                );
            }
            let mut result = json!({"name": name});
            if let Some(tid) = topic_id {
                result["topic_id"] = json!(tid);
            }
            json!({"ok": true, "result": result})
        }
        Err(e) => json!({"ok": false, "error": format!("{e}")}),
    }
}

/// Relocate the pane currently hosting `agent` into `target_tab`.
///
/// If the target tab exists, the moved pane splits the target tab's focused
/// pane along `split_dir` (default: horizontal). If the target tab does not
/// exist, a new tab named `target_tab` is created with the moved pane as its
/// root. `split_dir` is ignored in the new-tab case.
///
/// The actual layout mutation happens in the TUI event loop — this handler
/// only validates inputs and emits `ApiEvent::PaneMoved`. Daemon mode (no
/// notifier) is a no-op and still returns `{"ok": true}`, matching the
/// semantics of other layout-affecting MCP methods.
pub(crate) fn handle_move_pane(params: &Value, ctx: &HandlerCtx) -> Value {
    let agent_name = match params["agent"].as_str() {
        Some(n) => n,
        None => return json!({"ok": false, "error": "missing agent"}),
    };
    if let Err(e) = agent::validate_name(agent_name) {
        return json!({"ok": false, "error": e});
    }
    let target_tab = match params["target_tab"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => return json!({"ok": false, "error": "missing target_tab"}),
    };
    let split_dir = PaneMoveSplitDir::parse(params["split_dir"].as_str().unwrap_or("horizontal"));

    if let Some(n) = ctx.notifier {
        n.notify(ApiEvent::PaneMoved {
            agent: agent_name.to_string(),
            target_tab: target_tab.to_string(),
            split_dir,
        });
    }
    crate::event_log::log(
        ctx.home,
        "move_pane",
        agent_name,
        &format!("target_tab={target_tab} split={split_dir:?}"),
    );
    json!({"ok": true})
}

pub(crate) fn handle_set_blocked_reason(params: &Value, ctx: &HandlerCtx) -> Value {
    let name = match params["name"].as_str() {
        Some(n) => n,
        None => return json!({"ok": false, "error": "missing 'name'"}),
    };
    let reason_str = match params["reason"].as_str() {
        Some(r) => r,
        None => return json!({"ok": false, "error": "missing 'reason'"}),
    };
    let reason = match reason_str {
        "rate_limit" => crate::health::BlockedReason::RateLimit {
            retry_after_secs: params["retry_after_secs"].as_u64(),
        },
        "quota_exceeded" => crate::health::BlockedReason::QuotaExceeded,
        "awaiting_operator" => crate::health::BlockedReason::AwaitingOperator,
        "permission_prompt" => crate::health::BlockedReason::PermissionPrompt,
        "hang" => crate::health::BlockedReason::Hang,
        "crash" => crate::health::BlockedReason::Crash,
        _ => return json!({"ok": false, "error": format!("unknown reason: {reason_str}")}),
    };
    let reg = agent::lock_registry(ctx.registry);
    match reg.get(name) {
        Some(handle) => {
            let mut core = handle.core.lock();
            let state = core.state.get_state().display_name().to_string();
            core.health.set_blocked_reason(reason);
            json!({"ok": true, "status": "reason_set", "reason": reason_str, "current_state": state})
        }
        None => json!({"ok": false, "error": format!("instance '{name}' not found")}),
    }
}

pub(crate) fn handle_clear_blocked_reason(params: &Value, ctx: &HandlerCtx) -> Value {
    let name = match params["name"].as_str() {
        Some(n) => n,
        None => return json!({"ok": false, "error": "missing 'name'"}),
    };
    let filter_reason = params["reason"].as_str();
    let reg = agent::lock_registry(ctx.registry);
    match reg.get(name) {
        Some(handle) => {
            let mut core = handle.core.lock();
            let was = core
                .health
                .current_reason
                .as_ref()
                .map(|r| serde_json::to_value(r).unwrap_or_default());
            // If a reason filter is specified, only clear if it matches
            if let Some(filter) = filter_reason {
                let matches = core.health.current_reason.as_ref().is_some_and(|r| {
                    let kind = match r {
                        crate::health::BlockedReason::RateLimit { .. } => "rate_limit",
                        crate::health::BlockedReason::QuotaExceeded => "quota_exceeded",
                        crate::health::BlockedReason::AwaitingOperator => "awaiting_operator",
                        crate::health::BlockedReason::PermissionPrompt => "permission_prompt",
                        crate::health::BlockedReason::Hang => "hang",
                        crate::health::BlockedReason::Crash => "crash",
                    };
                    kind == filter
                });
                if !matches {
                    return json!({"ok": false, "error": "reason mismatch", "current": was});
                }
            }
            core.health.clear_blocked_reason();
            json!({"ok": true, "status": "cleared", "instance": name, "was": was})
        }
        None => json!({"ok": false, "error": format!("instance '{name}' not found")}),
    }
}

pub(crate) fn handle_pane_snapshot(params: &Value, ctx: &HandlerCtx) -> Value {
    let name = match params["name"].as_str() {
        Some(n) => n,
        None => return json!({"ok": false, "error": "missing 'name'"}),
    };
    let lines = params["lines"].as_u64().unwrap_or(100) as usize;
    let reg = agent::lock_registry(ctx.registry);
    let handle = match reg.get(name) {
        Some(h) => h,
        None => return json!({"ok": false, "error": format!("instance '{name}' not found")}),
    };
    let core = handle.core.lock();
    let text = core.vterm.read_scrollback(lines);
    drop(core);
    drop(reg);
    json!({"ok": true, "text": text})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent;
    use parking_lot::Mutex;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn test_ctx_with_agent(name: &str) -> (HandlerCtx<'static>, Box<std::path::PathBuf>) {
        let home = Box::new(std::env::temp_dir().join(format!(
            "agend-api-inst-test-{}-{}",
            name,
            std::process::id()
        )));
        std::fs::create_dir_all(home.as_ref()).ok();

        // Leak the registries so they live for 'static — acceptable in tests.
        let registry: &'static agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));

        // Spawn a real shell agent so the registry has an entry with a HealthTracker.
        let spawn_cfg = agent::SpawnConfig {
            name,
            backend_command: crate::default_shell(),
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        agent::spawn_agent(&spawn_cfg, registry).expect("spawn test agent");

        let home_ref: &'static std::path::Path = Box::leak(home.clone());
        let ctx = HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home: home_ref,
        };
        (ctx, home)
    }

    fn cleanup_agent(ctx: &HandlerCtx, name: &str) {
        let reg = agent::lock_registry(ctx.registry);
        if let Some(h) = reg.get(name) {
            let _ = h.child.lock().kill();
        }
    }

    #[test]
    fn test_report_health_sets_reason_on_caller() {
        let (ctx, _home) = test_ctx_with_agent("health-set");
        std::thread::sleep(std::time::Duration::from_millis(500));

        let result = handle_set_blocked_reason(
            &json!({"name": "health-set", "reason": "rate_limit", "retry_after_secs": 60}),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(result["status"], "reason_set");
        assert_eq!(result["reason"], "rate_limit");

        // Verify the reason is actually set on the HealthTracker
        let reg = agent::lock_registry(ctx.registry);
        let handle = reg.get("health-set").expect("agent exists");
        let core = handle.core.lock();
        assert!(core.health.current_reason.is_some());
        match &core.health.current_reason {
            Some(crate::health::BlockedReason::RateLimit { retry_after_secs }) => {
                assert_eq!(*retry_after_secs, Some(60));
            }
            other => panic!("expected RateLimit, got {:?}", other),
        }
        drop(core);
        drop(reg);

        cleanup_agent(&ctx, "health-set");
    }

    #[test]
    fn test_clear_blocked_reason_by_operator() {
        let (ctx, _home) = test_ctx_with_agent("health-clear");
        std::thread::sleep(std::time::Duration::from_millis(500));

        // First set a reason
        let set_result = handle_set_blocked_reason(
            &json!({"name": "health-clear", "reason": "quota_exceeded"}),
            &ctx,
        );
        assert_eq!(set_result["ok"], true);

        // Clear it
        let clear_result = handle_clear_blocked_reason(&json!({"name": "health-clear"}), &ctx);
        assert_eq!(clear_result["ok"], true);
        assert_eq!(clear_result["status"], "cleared");
        assert_eq!(clear_result["instance"], "health-clear");
        // "was" should contain the previous reason
        assert!(clear_result["was"].is_object());

        // Verify it's actually cleared
        let reg = agent::lock_registry(ctx.registry);
        let handle = reg.get("health-clear").expect("agent exists");
        let core = handle.core.lock();
        assert!(core.health.current_reason.is_none());
        drop(core);
        drop(reg);

        cleanup_agent(&ctx, "health-clear");
    }

    /// §3.5.10 wire-format: pane_snapshot success-path response shape
    /// with deterministic PTY content. Pins ok=true, text=string, content
    /// ordering, and empty-PTY shape.
    #[test]
    fn pane_snapshot_success_shape_with_deterministic_content() {
        let (ctx, _home) = test_ctx_with_agent("snap-shape");
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Feed deterministic content into the agent's VTerm
        {
            let reg = agent::lock_registry(ctx.registry);
            let handle = reg.get("snap-shape").expect("agent exists");
            let mut core = handle.core.lock();
            for i in 1..=5 {
                core.vterm.process(format!("TESTLINE{i}\r\n").as_bytes());
            }
        }

        let result = handle_pane_snapshot(&json!({"name": "snap-shape", "lines": 100}), &ctx);

        // Pin response shape: ok=true, text=string
        assert_eq!(result["ok"], true, "success must have ok=true: {result}");
        let text = result["text"].as_str().expect("text must be a string");
        assert!(
            text.contains("TESTLINE1"),
            "must contain TESTLINE1, got: {text}"
        );
        assert!(
            text.contains("TESTLINE5"),
            "must contain TESTLINE5, got: {text}"
        );

        // Verify line ordering
        let pos1 = text.find("TESTLINE1").expect("TESTLINE1 present");
        let pos5 = text.find("TESTLINE5").expect("TESTLINE5 present");
        assert!(pos1 < pos5, "lines must be in order");

        cleanup_agent(&ctx, "snap-shape");
    }
}
