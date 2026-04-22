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
            let is_restarting = handle
                .core
                .lock()
                .map(|c| c.state.current.is_unavailable())
                .unwrap_or(false);
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
            if let Ok(mut core) = handle.core.lock() {
                core.state.set_restarting();
            }
            let mut child = crate::sync::lock_poisoned(&handle.child, "api_child");
            let _ = child.kill();
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
    let mut reg = agent::lock_registry(ctx.registry);
    if let Some(handle) = reg.get(name) {
        let mut child = crate::sync::lock_poisoned(&handle.child, "api_child");
        let _ = child.kill();
        drop(child);
    }
    reg.remove(name);
    drop(reg);
    crate::sync::lock_poisoned(ctx.configs, "api_configs").remove(name);
    crate::ipc::remove_port(&crate::daemon::run_dir(ctx.home), name);
    crate::event_log::log(ctx.home, "delete", name, "deleted via API");
    if let Some(n) = ctx.notifier {
        tracing::info!(agent = name, "DELETE emitting InstanceDeleted");
        n.notify(ApiEvent::InstanceDeleted {
            name: name.to_string(),
        });
    }
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
        Ok(()) => {
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
            json!({"ok": true, "result": {"name": name}})
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
