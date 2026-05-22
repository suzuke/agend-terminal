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
                // Record for ServerRateLimit auto-retry.
                if result.is_ok() && !data.is_empty() {
                    drop(reg);
                    crate::daemon::heartbeat_pair::update_with(name, |p| {
                        p.last_input_text = Some(data.to_string());
                    });
                    return match result {
                        Ok(()) => json!({"ok": true, "result": {"bytes": data.len()}}),
                        Err(e) => json!({"ok": false, "error": format!("{e}")}),
                    };
                }
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
    // H3: clean up poll_reminder dedup state for deleted agent
    crate::daemon::poll_reminder::remove_agent(name);
    if let Some(n) = ctx.notifier {
        tracing::info!(agent = name, "DELETE emitting InstanceDeleted");
        n.notify(ApiEvent::InstanceDeleted {
            name: name.to_string(),
        });
    }
    json!({"ok": true})
}

/// Parse the SPAWN-RPC `env` field into a `HashMap` of process env vars.
/// Non-string values are dropped (the SPAWN schema accepts string-string
/// only — this matches `agent::build_command`'s `cmd.env(k, v)` shape).
/// `None` here is "no override" (caller will fall back to fleet); the
/// caller distinguishes "no env field" from "explicit empty map" by
/// checking the original `Value` shape if that semantic ever matters.
fn parse_env_object(value: Option<&Value>) -> Option<std::collections::HashMap<String, String>> {
    let obj = value?.as_object()?;
    Some(
        obj.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
    )
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
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(ctx.home))
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
        .unwrap_or_else(|| crate::paths::workspace_dir(ctx.home).join(name));
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

    // #900 hybrid (b)+(c): env precedence is params.env > fleet.yaml
    // resolved env > none. `params.env` lets the SPAWN caller pass an
    // explicit override (MCP start_instance forwards the already-resolved
    // env from its own fleet load; future explicit-env spawners do too);
    // the fleet fallback covers deploy_template Phase 3 + operator
    // hand-edited fleet.yaml entries where the wire payload omits env.
    let env_from_params = parse_env_object(params.get("env"));
    let env_from_fleet = if env_from_params.is_none() {
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(ctx.home))
            .ok()
            .and_then(|f| f.resolve_instance(name).map(|r| r.env))
    } else {
        None
    };
    let env_for_spawn = env_from_params.as_ref().or(env_from_fleet.as_ref());

    match crate::api::spawn_one(
        ctx.home,
        ctx.registry,
        name,
        command,
        &args,
        spawn_mode,
        &work_dir,
        size,
        env_for_spawn,
    ) {
        Ok(_spawn_mode) => {
            // #991: skip topic creation when caller opts out.
            let topic_binding = params["topic_binding"].as_str().unwrap_or("auto");
            let topic_id = if matches!(topic_binding, "skip" | "deferred") {
                None
            } else {
                match crate::channel::ensure_topic_for(name) {
                    crate::channel::TopicOutcome::Created(tid) => Some(tid),
                    crate::channel::TopicOutcome::NoChannel => None,
                    crate::channel::TopicOutcome::Failed(err) => {
                        tracing::warn!(
                            agent = name,
                            error = %err,
                            "SPAWN: channel exists but create_topic failed; \
                             instance created without topic"
                        );
                        None
                    }
                }
            };
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

    /// §3.5.10 persistence-replay: spawn clears stale metadata for same name.
    #[test]
    fn spawn_clears_stale_metadata_for_same_name() {
        let (ctx, home) = test_ctx_with_agent("meta-stale");
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Kill the agent so we can re-spawn with the same name
        cleanup_agent(&ctx, "meta-stale");
        {
            let mut reg = agent::lock_registry(ctx.registry);
            reg.remove("meta-stale");
        }

        // Pre-seed stale metadata
        let meta_dir = home.join("metadata");
        std::fs::create_dir_all(&meta_dir).ok();
        let meta_path = meta_dir.join("meta-stale.json");
        std::fs::write(
            &meta_path,
            r#"{"last_heartbeat":"2026-01-01T00:00:00Z","pending_pickup_ids":["m-stale-1"]}"#,
        )
        .expect("write stale metadata");
        assert!(meta_path.exists(), "stale metadata must exist before spawn");

        // Re-spawn with same name — should clear stale metadata
        let result = handle_spawn(
            &json!({"name": "meta-stale", "backend": crate::default_shell()}),
            &ctx,
        );
        assert_eq!(result["ok"], true, "spawn must succeed: {result}");

        // Assert stale metadata is gone
        if meta_path.exists() {
            let content = std::fs::read_to_string(&meta_path).unwrap_or_default();
            assert!(
                !content.contains("2026-01-01"),
                "stale last_heartbeat must be cleared, got: {content}"
            );
            assert!(
                !content.contains("m-stale-1"),
                "stale pending_pickup_ids must be cleared, got: {content}"
            );
        }
        // Either file is absent (deleted) or has fresh content — both OK

        cleanup_agent(&ctx, "meta-stale");
    }

    #[test]
    fn spawn_with_no_prior_metadata_does_not_panic() {
        let (ctx, home) = test_ctx_with_agent("meta-fresh");
        std::thread::sleep(std::time::Duration::from_millis(500));
        cleanup_agent(&ctx, "meta-fresh");
        {
            let mut reg = agent::lock_registry(ctx.registry);
            reg.remove("meta-fresh");
        }

        // Ensure no metadata file exists
        let meta_path = home.join("metadata").join("meta-fresh.json");
        let _ = std::fs::remove_file(&meta_path);

        // Spawn should work fine without prior metadata
        let result = handle_spawn(
            &json!({"name": "meta-fresh", "backend": crate::default_shell()}),
            &ctx,
        );
        assert_eq!(
            result["ok"], true,
            "spawn without prior metadata must succeed: {result}"
        );

        cleanup_agent(&ctx, "meta-fresh");
    }

    /// §3.5.10: spawn_one (team-spawn path) also clears stale metadata.
    #[test]
    fn spawn_one_clears_stale_metadata_for_team_path() {
        let (ctx, home) = test_ctx_with_agent("team-meta");
        std::thread::sleep(std::time::Duration::from_millis(500));
        cleanup_agent(&ctx, "team-meta");
        {
            let mut reg = agent::lock_registry(ctx.registry);
            reg.remove("team-meta");
        }

        // Pre-seed stale metadata
        let meta_dir = home.join("metadata");
        std::fs::create_dir_all(&meta_dir).ok();
        let meta_path = meta_dir.join("team-meta.json");
        std::fs::write(
            &meta_path,
            r#"{"last_heartbeat":"2025-01-01T00:00:00Z","stale":true}"#,
        )
        .expect("write stale metadata");

        // Call spawn_one directly (team-spawn path bypasses handle_spawn)
        let size = (80u16, 24u16);
        let work_dir = crate::paths::workspace_dir(&home).join("team-meta");
        std::fs::create_dir_all(&work_dir).ok();
        let result = crate::api::spawn_one(
            ctx.home,
            ctx.registry,
            "team-meta",
            crate::default_shell(),
            &[],
            crate::backend::SpawnMode::Fresh,
            &work_dir,
            size,
            None,
        );
        assert!(result.is_ok(), "spawn_one must succeed: {result:?}");

        // Assert stale metadata is gone
        if meta_path.exists() {
            let content = std::fs::read_to_string(&meta_path).unwrap_or_default();
            assert!(
                !content.contains("2025-01-01"),
                "stale metadata must be cleared via spawn_one, got: {content}"
            );
        }

        cleanup_agent(&ctx, "team-meta");
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn topic_id_persists_to_fleet_yaml_via_update_instance_field() {
        // Helper-level test for update_instance_field (#415). Post-#994
        // production uses topics.json, but this function still works.
        let home = std::env::temp_dir().join(format!("agend-topic-persist-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  agent1:\n    backend: claude\n",
        )
        .unwrap();
        crate::fleet::update_instance_field(
            &home,
            "agent1",
            "topic_id",
            serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(42)),
        )
        .unwrap();
        let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        assert_eq!(
            cfg.instances.get("agent1").and_then(|i| i.topic_id),
            Some(42),
            "topic_id must be persisted to fleet.yaml"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ----- #900: env propagation through SPAWN RPC + fleet.yaml -----

    /// Build a test `HandlerCtx` with an isolated home dir + empty
    /// registries. Unlike `test_ctx_with_agent`, no pre-existing agent
    /// is spawned — these env tests spawn the agent under test directly
    /// so the registry is clean.
    #[cfg(unix)]
    fn env_test_ctx(test_name: &str) -> (HandlerCtx<'static>, std::path::PathBuf) {
        let home =
            std::env::temp_dir().join(format!("agend-900-{}-{}", test_name, std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create home");

        // #949 bonus: pre-issue api.cookie so the background TUI bridge
        // thread that spawn_one fires (fire-and-forget at
        // src/api/mod.rs:597-600 via `crate::daemon::serve_agent_tui`)
        // can read it without the noisy "TUI listener prep failed;
        // api.cookie unavailable" WARN. The TUI bridge isn't load-
        // bearing for env-propagation tests — this is cosmetic log
        // hygiene that ALSO eliminates a misleading red-herring trail
        // that confused the original #949 RCA hypothesis (operator
        // suspected api.cookie race; the real flake was `await_sentinel`
        // reading from `printf > sentinel`'s empty open-truncate window).
        let rdir = crate::daemon::run_dir(&home);
        std::fs::create_dir_all(&rdir).ok();
        let _ = crate::auth_cookie::issue(&rdir);

        let registry: &'static agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let home_ref: &'static std::path::Path = Box::leak(home.clone().into_boxed_path());

        let ctx = HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home: home_ref,
        };
        (ctx, home)
    }

    /// Write a tiny shell script that captures `$VAR_NAME` to `sentinel_path`
    /// then sleeps so the agent stays alive long enough for cleanup_agent
    /// to reap it. Returns the script path to pass as the agent's args.
    #[cfg(unix)]
    fn write_env_capture_script(
        home: &std::path::Path,
        var_name: &str,
        sentinel_path: &std::path::Path,
    ) -> std::path::PathBuf {
        let script = home.join("env-capture.sh");
        let body = format!(
            "#!/bin/sh\nprintf '%s' \"${{{var}:-__UNSET__}}\" > '{sentinel}'\nsleep 30\n",
            var = var_name,
            sentinel = sentinel_path.display()
        );
        std::fs::write(&script, body).expect("write script");
        script
    }

    /// Poll for the sentinel file to appear with NON-EMPTY content,
    /// then return its contents. Returns `None` on timeout.
    ///
    /// #949: pre-#949 name was `await_sentinel`; the early-return on
    /// any successful `read_to_string` (including `Ok("")`) raced
    /// `printf > sentinel`'s open-truncate-then-write sequence and
    /// returned `Some("")` from the empty intermediate state under
    /// CI scheduler contention. The rename makes the non-empty
    /// contract explicit at call sites; the body waits until content
    /// actually commits (§3.20 SOP 1 — poll-with-deadline against
    /// the real post-condition). Same idiom as #905/#909's
    /// `agent::tests::wait_for_nonempty_file` for the sweep_child_tree
    /// pid_file race.
    ///
    /// Sentinel always materializes once the shell launches (script
    /// writes unconditionally), so a `None` here means the agent itself
    /// never ran OR the post-trim content stayed empty (the script's
    /// `${VAR:-__UNSET__}` default ensures non-empty content even when
    /// the env var is unset).
    #[cfg(unix)]
    fn await_sentinel_nonempty(sentinel_path: &std::path::Path) -> Option<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(c) = std::fs::read_to_string(sentinel_path) {
                // #949: wait for content commit. `printf > sentinel`
                // creates the file empty (open-truncate) before writing,
                // so a bare `exists() + read` polled during that window
                // would return `Some("")` from the empty intermediate
                // state. Continue polling until non-empty content is
                // observed.
                if !c.is_empty() {
                    return Some(c);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        None
    }

    /// #949 RED: synthetic reproduction of the `await_sentinel` write-vs-read race.
    ///
    /// A writer thread (mimics the shell script's `printf > sentinel`):
    ///   Phase 1: creates the sentinel file EMPTY (open-truncate).
    ///   Phase 2: sleeps to simulate the CI-contention write-flush gap.
    ///   Phase 3: writes the actual content.
    ///
    /// Under the pre-#949 `await_sentinel` logic, the poll observes
    /// `exists() + read_to_string == Ok("")` during Phase 1, takes the
    /// `return Some(c)` early-return, and yields `Some("")` BEFORE
    /// content commits. The fix (#949 GREEN) polls until non-empty.
    ///
    /// This test FAILS against the pre-fix logic deterministically
    /// (Phase 1's empty window is engineered to overlap the 50ms poll
    /// cadence). Post-fix it PASSES.
    #[cfg(unix)]
    #[test]
    fn await_sentinel_waits_for_nonempty_content_949() {
        let sentinel = std::env::temp_dir().join(format!(
            "agend-949-await-test-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&sentinel);

        let writer_path = sentinel.clone();
        let writer = std::thread::spawn(move || {
            // Phase 1: create empty (printf's open-truncate stage).
            std::fs::write(&writer_path, "").expect("create empty");
            // Phase 2: simulate CI-contention write-flush gap. 200ms
            // is 4× the poll cadence (50ms) — guarantees the reader
            // polls into the empty window at least once.
            std::thread::sleep(std::time::Duration::from_millis(200));
            // Phase 3: commit content.
            std::fs::write(&writer_path, "value-from-params").expect("commit");
        });

        let result = await_sentinel_nonempty(&sentinel);
        writer.join().expect("writer joined");

        assert_eq!(
            result.as_deref(),
            Some("value-from-params"),
            "await_sentinel must wait for non-empty content commit, not \
             early-return on the open-truncate empty window. Got {result:?}"
        );

        let _ = std::fs::remove_file(&sentinel);
    }

    /// #900 ingress 1 — `handle_spawn` MUST extract `env` from `params`
    /// and propagate it down `spawn_one` → `spawn_agent` → `build_command`
    /// so the child process inherits the requested env vars. Pre-fix
    /// `spawn_one` hard-codes `env: None` so any caller-supplied env
    /// is silently dropped; this is the SPAWN-RPC half of the bug.
    #[cfg(unix)]
    #[test]
    fn handle_spawn_propagates_params_env_to_spawned_process() {
        let (ctx, home) = env_test_ctx("handle-spawn-params");
        let sentinel = home.join("sentinel.txt");
        let script = write_env_capture_script(&home, "MY_SPIKE_900_PARAMS_VAR", &sentinel);

        let result = handle_spawn(
            &json!({
                "name": "env-params-test",
                "backend": "/bin/sh",
                "args": script.display().to_string(),
                "env": {"MY_SPIKE_900_PARAMS_VAR": "value-from-params"},
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true, "spawn must succeed: {result}");

        let actual = await_sentinel_nonempty(&sentinel);
        cleanup_agent(&ctx, "env-params-test");
        let _ = std::fs::remove_dir_all(&home);

        assert_eq!(
            actual.as_deref(),
            Some("value-from-params"),
            "params.env MUST propagate to the child process; the legacy \
             behavior silently dropped it, leaving the var unset"
        );
    }

    /// #900 ingress 2 — when SPAWN params omit `env`, `handle_spawn`
    /// MUST fall back to `fleet.yaml`'s resolved env for the named
    /// instance. This is the path that `deploy_template` (Phase 3
    /// writes fleet entry, then issues SPAWN without env in the wire
    /// payload) and operator-typed fleet.yaml entries both rely on.
    #[cfg(unix)]
    #[test]
    fn handle_spawn_falls_back_to_fleet_yaml_env_when_params_missing() {
        let (ctx, home) = env_test_ctx("handle-spawn-fleet-fallback");
        let sentinel = home.join("sentinel.txt");
        let script = write_env_capture_script(&home, "MY_SPIKE_900_FLEET_VAR", &sentinel);

        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  env-fleet-test:\n    backend: shell\n    env:\n      MY_SPIKE_900_FLEET_VAR: value-from-fleet\n",
        )
        .expect("write fleet.yaml");

        let result = handle_spawn(
            &json!({
                "name": "env-fleet-test",
                "backend": "/bin/sh",
                "args": script.display().to_string(),
                // No "env" field — handler must resolve from fleet.yaml.
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true, "spawn must succeed: {result}");

        let actual = await_sentinel_nonempty(&sentinel);
        cleanup_agent(&ctx, "env-fleet-test");
        let _ = std::fs::remove_dir_all(&home);

        assert_eq!(
            actual.as_deref(),
            Some("value-from-fleet"),
            "fleet.yaml env MUST propagate when SPAWN params omit env"
        );
    }

    /// #900 precedence — params.env wins over fleet.yaml env. Operators
    /// that pass an explicit `env` in their SPAWN call must always see
    /// it honoured over whatever's in fleet.yaml for the same key.
    #[cfg(unix)]
    #[test]
    fn handle_spawn_params_env_overrides_fleet_yaml_env() {
        let (ctx, home) = env_test_ctx("handle-spawn-precedence");
        let sentinel = home.join("sentinel.txt");
        let script = write_env_capture_script(&home, "MY_SPIKE_900_PREC_VAR", &sentinel);

        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  env-precedence-test:\n    backend: shell\n    env:\n      MY_SPIKE_900_PREC_VAR: from-fleet\n",
        )
        .expect("write fleet.yaml");

        let result = handle_spawn(
            &json!({
                "name": "env-precedence-test",
                "backend": "/bin/sh",
                "args": script.display().to_string(),
                "env": {"MY_SPIKE_900_PREC_VAR": "from-params-wins"},
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true, "spawn must succeed: {result}");

        let actual = await_sentinel_nonempty(&sentinel);
        cleanup_agent(&ctx, "env-precedence-test");
        let _ = std::fs::remove_dir_all(&home);

        assert_eq!(
            actual.as_deref(),
            Some("from-params-wins"),
            "params.env MUST take precedence over fleet.yaml env for the same key"
        );
    }
}
