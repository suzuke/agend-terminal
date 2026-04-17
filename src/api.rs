//! Daemon JSON control API over Unix socket.
//!
//! Protocol: NDJSON (one JSON request per line, one JSON response per line).
//! Socket: {home}/api.sock

use crate::agent::{self, AgentRegistry, ExternalRegistry};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

pub type ConfigRegistry = Arc<Mutex<HashMap<String, crate::daemon::AgentConfig>>>;

/// API method name constants — single source of truth for the NDJSON protocol.
pub mod method {
    pub const LIST: &str = "list";
    pub const INJECT: &str = "inject";
    pub const KILL: &str = "kill";
    pub const DELETE: &str = "delete";
    pub const SPAWN: &str = "spawn";
    pub const SEND: &str = "send";
    pub const STATUS: &str = "status";
    pub const REGISTER_EXTERNAL: &str = "register_external";
    pub const DEREGISTER_EXTERNAL: &str = "deregister_external";
    pub const CREATE_TEAM: &str = "create_team";
    pub const SHUTDOWN: &str = "shutdown";
}

/// Start API socket server (blocks calling thread).
///
/// `tui_tx`: when running inside the TUI app, `Some(sender)` to notify the
/// event loop about instance/team creation and deletion. Daemon mode passes
/// `None` and events are silently dropped.
pub fn serve(
    home: &Path,
    registry: AgentRegistry,
    shutdown: Arc<AtomicBool>,
    configs: ConfigRegistry,
    externals: ExternalRegistry,
    tui_tx: Option<crate::app::TuiEventSender>,
) {
    let sock = api_socket_path(home);
    let _ = std::fs::remove_file(&sock);

    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(path = %sock, error = %e, "failed to bind API socket");
            return;
        }
    };
    tracing::info!(path = %sock, "API listening");

    for stream in listener.incoming().flatten() {
        let reg = Arc::clone(&registry);
        let home = home.to_path_buf();
        let shutdown = Arc::clone(&shutdown);
        let cfgs = Arc::clone(&configs);
        let ext = Arc::clone(&externals);
        let tui = tui_tx.clone();
        std::thread::Builder::new()
            .name("api_handler".into())
            .spawn(move || {
                handle_session(stream, &reg, &home, &shutdown, &cfgs, &ext, tui.as_ref())
            })
            .ok();
    }
}

pub fn api_socket_path(home: &Path) -> String {
    crate::daemon::run_dir(home)
        .join("api.sock")
        .display()
        .to_string()
}

/// Find API socket from any active daemon.
pub fn find_api_socket(home: &Path) -> Option<String> {
    let run = crate::daemon::find_active_run_dir(home)?;
    let sock = run.join("api.sock");
    if sock.exists() {
        Some(sock.display().to_string())
    } else {
        None
    }
}

fn handle_session(
    stream: UnixStream,
    registry: &AgentRegistry,
    home: &Path,
    shutdown: &Arc<AtomicBool>,
    configs: &ConfigRegistry,
    externals: &ExternalRegistry,
    tui_tx: Option<&crate::app::TuiEventSender>,
) {
    let cloned = match stream.try_clone() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "API stream clone failed");
            return;
        }
    };
    let mut reader = BufReader::new(cloned);
    let mut writer = stream;

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(
                    writer,
                    "{}",
                    json!({"ok": false, "error": format!("parse: {e}")})
                );
                continue;
            }
        };

        let method = req["method"].as_str().unwrap_or("");
        let params = &req["params"];

        let response = match method {
            method::LIST => {
                let reg = agent::lock_registry(registry);
                let mut agents: Vec<Value> = reg.iter()
                    .map(|(name, handle)| {
                        let (agent_state, health_state) = handle.core.lock()
                            .map(|c| (c.state.get_state().display_name().to_string(), c.health.state.display_name().to_string()))
                            .unwrap_or_else(|_| ("unknown".into(), "unknown".into()));
                        json!({"name": name, "backend": handle.backend_command, "submit_key": handle.submit_key,
                               "inject_prefix": handle.inject_prefix, "agent_state": agent_state, "health_state": health_state,
                               "kind": "managed"})
                    }).collect();
                drop(reg);
                let ext = agent::lock_external(externals);
                for (name, handle) in ext.iter() {
                    agents.push(json!({
                        "name": name, "backend": handle.backend_command,
                        "agent_state": "external", "health_state": "connected",
                        "kind": "external", "pid": handle.pid
                    }));
                }
                json!({"ok": true, "result": {"protocol_version": crate::framing::PROTOCOL_VERSION, "agents": agents}})
            }
            method::INJECT => {
                let name = params["name"].as_str().unwrap_or("");
                if let Err(e) = agent::validate_name(name) {
                    let _ = writeln!(writer, "{}", json!({"ok": false, "error": e}));
                    continue;
                }
                let data = params["data"].as_str().unwrap_or("");
                // "raw" flag: send bytes as-is (for attach-like paths)
                let raw = params["raw"].as_bool().unwrap_or(false);
                let reg = agent::lock_registry(registry);
                match reg.get(name) {
                    Some(handle) => {
                        // Check if agent is restarting
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
                        let ext = agent::lock_external(externals);
                        if ext.contains_key(name) {
                            json!({"ok": false, "error": format!("agent '{name}' is external — use send instead of inject")})
                        } else {
                            json!({"ok": false, "error": format!("agent '{name}' not found")})
                        }
                    }
                }
            }
            method::KILL => {
                let name = params["name"].as_str().unwrap_or("");
                if let Err(e) = agent::validate_name(name) {
                    let _ = writeln!(writer, "{}", json!({"ok": false, "error": e}));
                    continue;
                }
                let reg = agent::lock_registry(registry);
                match reg.get(name) {
                    Some(handle) => {
                        if let Ok(mut core) = handle.core.lock() {
                            core.state.set_restarting();
                        }
                        let mut child = handle.child.lock().unwrap_or_else(|e| e.into_inner());
                        let _ = child.kill();
                        drop(child);
                        drop(reg);
                        crate::event_log::log(home, "kill", name, "killed via API");
                        json!({"ok": true})
                    }
                    None => {
                        // Try external registry
                        drop(reg);
                        let mut ext = agent::lock_external(externals);
                        if ext.remove(name).is_some() {
                            crate::event_log::log(home, "kill", name, "external agent removed");
                            json!({"ok": true})
                        } else {
                            json!({"ok": false, "error": format!("agent '{name}' not found")})
                        }
                    }
                }
            }
            method::DELETE => {
                // Kill + remove from registry + configs
                let name = params["name"].as_str().unwrap_or("");
                if let Err(e) = agent::validate_name(name) {
                    let _ = writeln!(writer, "{}", json!({"ok": false, "error": e}));
                    continue;
                }
                // Check external registry first
                {
                    let mut ext = agent::lock_external(externals);
                    if ext.remove(name).is_some() {
                        crate::event_log::log(home, "delete", name, "external agent deleted");
                        let _ = writeln!(writer, "{}", json!({"ok": true}));
                        let _ = writer.flush();
                        continue;
                    }
                }
                // Kill and remove from registry first — this prevents the PTY close
                // handler from sending a crash event (agent not in registry = no crash).
                let mut reg = agent::lock_registry(registry);
                if let Some(handle) = reg.get(name) {
                    let mut child = handle.child.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = child.kill();
                    drop(child);
                }
                reg.remove(name);
                drop(reg);
                // Then remove config (no race: agent already gone from registry)
                configs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(name);
                // Cleanup socket
                let sock = crate::daemon::agent_socket_path(home, name);
                let _ = std::fs::remove_file(&sock);
                crate::event_log::log(home, "delete", name, "deleted via API");
                // Notify TUI to close the corresponding pane
                if let Some(tx) = tui_tx {
                    if let Err(e) = tx.try_send(crate::app::TuiEvent::InstanceDeleted {
                        name: name.to_string(),
                    }) {
                        tracing::warn!("TUI event send failed (delete {}): {e}", name);
                    }
                }
                json!({"ok": true})
            }
            method::SPAWN => {
                let name = match params["name"].as_str() {
                    Some(n) => n,
                    None => {
                        let _ =
                            writeln!(writer, "{}", json!({"ok": false, "error": "missing name"}));
                        continue;
                    }
                };
                if let Err(e) = agent::validate_name(name) {
                    let _ = writeln!(writer, "{}", json!({"ok": false, "error": e}));
                    continue;
                }
                // Dedup: reject spawn for an already-registered name. spawn_agent
                // silently overwrites the registry entry, which would orphan the
                // previous agent's PTY and leave panes reading from a stale
                // subscription. The respawn-after-crash path in daemon.rs goes
                // through a different code path and is not affected.
                if agent::lock_registry(registry).contains_key(name) {
                    let _ = writeln!(
                        writer,
                        "{}",
                        json!({"ok": false, "error": format!("agent '{name}' already exists")})
                    );
                    continue;
                }
                let command = params["backend"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| {
                        crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))
                            .ok()
                            .and_then(|f| {
                                f.defaults.backend.map(|b| b.preset().command.to_string())
                            })
                            .unwrap_or_else(|| "claude".to_string())
                    });
                let command = command.as_str();
                let args: Vec<String> = params["args"]
                    .as_str()
                    .map(|s| s.split_whitespace().map(String::from).collect())
                    .unwrap_or_default();
                let work_dir = params["working_directory"]
                    .as_str()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| home.join("workspace").join(name));
                let size = crossterm::terminal::size().unwrap_or((120, 40));

                match spawn_one(home, registry, name, command, &args, &work_dir, size) {
                    Ok(()) => {
                        if let Some(tx) = tui_tx {
                            let layout_hint = crate::app::LayoutHint::from_str(
                                params["layout"].as_str().unwrap_or("tab"),
                            );
                            let spawner = params["spawner"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .map(String::from);
                            tracing::info!(
                                agent = name,
                                layout = ?layout_hint,
                                spawner = ?spawner,
                                channel_len = tx.len(),
                                "SPAWN emitting InstanceCreated"
                            );
                            if let Err(e) = tx.try_send(crate::app::TuiEvent::InstanceCreated {
                                name: name.to_string(),
                                layout: layout_hint,
                                spawner,
                            }) {
                                tracing::warn!(agent = name, error = %e, "InstanceCreated try_send failed");
                            }
                        }
                        json!({"ok": true, "result": {"name": name}})
                    }
                    Err(e) => json!({"ok": false, "error": format!("{e}")}),
                }
            }
            method::SEND => {
                let (from, target, text) = (
                    params["from"].as_str().unwrap_or("unknown"),
                    params["target"].as_str().unwrap_or(""),
                    params["text"].as_str().unwrap_or(""),
                );
                if let Err(e) = agent::validate_name(target) {
                    let _ = writeln!(writer, "{}", json!({"ok": false, "error": e}));
                    continue;
                }
                if from == target {
                    let _ = writeln!(
                        writer,
                        "{}",
                        json!({"ok": false, "error": "cannot send to self"})
                    );
                    continue;
                }
                let _ = crate::inbox::enqueue(
                    home,
                    target,
                    crate::inbox::InboxMessage {
                        from: format!("from:{from}"),
                        text: text.to_string(),
                        kind: params
                            .get("kind")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        timestamp: chrono::Utc::now().to_rfc3339(),
                    },
                );
                let display_text = if text.chars().count() > 200 {
                    format!(
                        "{}... (use inbox tool)",
                        text.chars().take(200).collect::<String>()
                    )
                } else {
                    text.to_string()
                };
                let inject_msg = format!("[from:{from}] {display_text} (Reply using send_to_instance MCP tool, NOT direct text)");

                // Try managed agent first
                let reg = agent::lock_registry(registry);
                if let Some(handle) = reg.get(target) {
                    let _ = agent::inject_to_agent(handle, inject_msg.as_bytes());
                }
                // External agents receive messages via inbox only (no PTY injection)
                json!({"ok": true})
            }
            method::STATUS => match crate::snapshot::load(home) {
                Some(snapshot) => {
                    json!({"ok": true, "result": {
                        "protocol_version": crate::framing::PROTOCOL_VERSION,
                        "timestamp": snapshot.timestamp,
                        "agents": snapshot.agents.iter().map(|a| {
                            json!({
                                "name": a.name,
                                "backend": a.backend_command,
                                "args": a.args,
                                "working_dir": a.working_dir,
                                "submit_key": a.submit_key,
                                "health_state": a.health_state,
                                "agent_state": a.agent_state,
                            })
                        }).collect::<Vec<_>>()
                    }})
                }
                None => json!({"ok": true, "result": {"agents": [], "timestamp": null}}),
            },
            method::REGISTER_EXTERNAL => {
                let name = match params["name"].as_str() {
                    Some(n) => n,
                    None => {
                        let _ =
                            writeln!(writer, "{}", json!({"ok": false, "error": "missing name"}));
                        continue;
                    }
                };
                if let Err(e) = agent::validate_name(name) {
                    let _ = writeln!(writer, "{}", json!({"ok": false, "error": e}));
                    continue;
                }
                // Atomic check-and-insert: lock order is always registry → external
                let reg = agent::lock_registry(registry);
                if reg.contains_key(name) {
                    let _ = writeln!(
                        writer,
                        "{}",
                        json!({"ok": false, "error": format!("agent '{name}' already exists (managed)")})
                    );
                    continue;
                }
                let mut ext = agent::lock_external(externals);
                if ext.contains_key(name) {
                    let _ = writeln!(
                        writer,
                        "{}",
                        json!({"ok": false, "error": format!("agent '{name}' already exists (external)")})
                    );
                    continue;
                }
                let backend = params["backend"].as_str().unwrap_or("unknown");
                let pid = params["pid"].as_u64().unwrap_or(0) as u32;
                ext.insert(
                    name.to_string(),
                    agent::ExternalAgentHandle {
                        backend_command: backend.to_string(),
                        pid,
                    },
                );
                drop(reg);
                drop(ext);
                crate::event_log::log(
                    home,
                    "connect",
                    name,
                    &format!("external agent registered (pid={pid}, backend={backend})"),
                );
                tracing::info!(agent = name, pid, backend, "external agent registered");
                json!({"ok": true})
            }
            method::DEREGISTER_EXTERNAL => {
                let name = params["name"].as_str().unwrap_or("");
                if let Err(e) = agent::validate_name(name) {
                    let _ = writeln!(writer, "{}", json!({"ok": false, "error": e}));
                    continue;
                }
                let mut ext = agent::lock_external(externals);
                if ext.remove(name).is_some() {
                    drop(ext);
                    crate::event_log::log(home, "disconnect", name, "external agent deregistered");
                    tracing::info!(agent = name, "external agent deregistered");
                    json!({"ok": true})
                } else {
                    json!({"ok": false, "error": format!("external agent '{name}' not found")})
                }
            }
            method::CREATE_TEAM => {
                let team_name = match params["name"].as_str() {
                    Some(n) => n,
                    None => {
                        let _ =
                            writeln!(writer, "{}", json!({"ok": false, "error": "missing name"}));
                        continue;
                    }
                };
                let count = params["count"].as_u64().unwrap_or(0) as usize;
                let backend = params["backend"].as_str().unwrap_or("claude");
                tracing::info!(team = team_name, count, backend, "CREATE_TEAM begin");

                let mut spawned: Vec<String> = Vec::new();
                let mut failed: Vec<String> = Vec::new();
                let size = crossterm::terminal::size().unwrap_or((120, 40));
                for i in 1..=count {
                    let inst_name = format!("{team_name}-{i}");
                    // Dedup: see SPAWN handler note. Re-creating a team with an
                    // existing name would otherwise overwrite the registry entry
                    // and orphan the previous tab's PTY subscription.
                    if agent::lock_registry(registry).contains_key(&inst_name) {
                        tracing::warn!(team = team_name, member = %inst_name, "CREATE_TEAM skip: name already exists");
                        failed.push(format!("{inst_name}: agent already exists"));
                        continue;
                    }
                    let work_dir = home.join("workspace").join(&inst_name);
                    match spawn_one(home, registry, &inst_name, backend, &[], &work_dir, size) {
                        Ok(()) => {
                            tracing::info!(team = team_name, member = %inst_name, "CREATE_TEAM spawn ok");
                            spawned.push(inst_name);
                        }
                        Err(e) => {
                            tracing::warn!(team = team_name, member = %inst_name, error = %e, "CREATE_TEAM spawn failed");
                            failed.push(format!("{inst_name}: {e}"));
                        }
                    }
                }
                tracing::info!(team = team_name, spawned = spawned.len(), failed = failed.len(), "CREATE_TEAM spawn phase done");
                if count > 0 && spawned.is_empty() {
                    let _ = writeln!(
                        writer,
                        "{}",
                        json!({"ok": false, "error": format!("all {} spawns failed: {}", count, failed.join("; "))})
                    );
                    continue;
                }

                let existing: Vec<String> = params["members"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let all_members: Vec<String> = existing
                    .into_iter()
                    .chain(spawned.iter().cloned())
                    .collect();

                if !spawned.is_empty() {
                    let entries: Vec<(String, crate::fleet::InstanceYamlEntry)> = spawned
                        .iter()
                        .map(|name| {
                            (
                                name.clone(),
                                crate::fleet::InstanceYamlEntry {
                                    backend: Some(backend.to_string()),
                                    working_directory: Some(
                                        home.join("workspace").join(name).display().to_string(),
                                    ),
                                    role: None,
                                },
                            )
                        })
                        .collect();
                    let refs: Vec<(&str, &crate::fleet::InstanceYamlEntry)> =
                        entries.iter().map(|(n, e)| (n.as_str(), e)).collect();
                    if let Err(e) = crate::fleet::add_instances_to_yaml(home, &refs) {
                        tracing::warn!(error = %e, "failed to persist team to fleet.yaml");
                    }
                }

                let team_params = json!({"name": team_name, "members": all_members, "description": params["description"]});
                let result = crate::teams::create(home, &team_params);

                if let Some(tx) = tui_tx {
                    if !spawned.is_empty() {
                        let members_for_event = spawned.clone();
                        tracing::info!(
                            team = team_name,
                            members = ?members_for_event,
                            channel_len = tx.len(),
                            channel_cap = ?tx.capacity(),
                            "CREATE_TEAM emitting TeamCreated"
                        );
                        if let Err(e) = tx.try_send(crate::app::TuiEvent::TeamCreated {
                            name: team_name.to_string(),
                            members: members_for_event,
                        }) {
                            tracing::warn!(team = team_name, error = %e, "TeamCreated try_send failed");
                        }
                    } else {
                        tracing::warn!(team = team_name, "CREATE_TEAM not emitting (spawned empty)");
                    }
                } else {
                    tracing::warn!(team = team_name, "CREATE_TEAM no tui_tx, event dropped");
                }
                let mut resp = json!({"ok": true, "result": result, "spawned": &spawned});
                if !failed.is_empty() {
                    resp["failed"] = json!(failed);
                }
                resp
            }
            method::SHUTDOWN => {
                tracing::info!("API shutdown requested");
                shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
                json!({"ok": true})
            }
            _ => json!({"ok": false, "error": format!("unknown method: {method}")}),
        };

        let _ = writeln!(writer, "{}", response);
        let _ = writer.flush();
    }
}

/// Spawn a single agent, register it, and start its TUI socket thread.
/// Shared by the SPAWN and CREATE_TEAM API handlers.
fn spawn_one(
    home: &Path,
    registry: &AgentRegistry,
    name: &str,
    backend: &str,
    args: &[String],
    work_dir: &Path,
    size: (u16, u16),
) -> anyhow::Result<()> {
    std::fs::create_dir_all(work_dir).ok();
    agent::spawn_agent(
        &agent::SpawnConfig {
            name,
            backend_command: backend,
            args,
            cols: size.0,
            rows: size.1,
            env: None,
            working_dir: Some(work_dir),
            submit_key: "\r",
            home: Some(home),
            crash_tx: None,
            shutdown: None,
        },
        registry,
    )?;
    let sock = crate::daemon::agent_socket_path(home, name);
    let reg = Arc::clone(registry);
    let n = name.to_string();
    std::thread::Builder::new()
        .name(format!("{n}_tui"))
        .spawn(move || crate::daemon::serve_agent_tui(&n, &sock, &reg))
        .ok();
    Ok(())
}

/// Send a request to the API socket and get response.
pub fn call(home: &Path, request: &Value) -> anyhow::Result<Value> {
    let sock = find_api_socket(home).unwrap_or_else(|| api_socket_path(home));
    let mut stream = UnixStream::connect(&sock)?;
    writeln!(stream, "{}", request)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let resp: Value = serde_json::from_str(line.trim())?;
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-api-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn api_socket_path_ends_with_api_sock() {
        let home = tmp_home("sock_path");
        let path = api_socket_path(&home);
        assert!(path.ends_with("api.sock"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn api_socket_path_under_run_dir() {
        let home = tmp_home("sock_path_run");
        let path = api_socket_path(&home);
        assert!(path.contains("run"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn find_api_socket_no_daemon() {
        let home = tmp_home("no_daemon");
        assert!(find_api_socket(&home).is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn find_api_socket_with_sock_file() {
        let home = tmp_home("with_sock");
        let pid = std::process::id();
        let run = home.join("run").join(pid.to_string());
        std::fs::create_dir_all(&run).ok();
        // Write daemon ID so find_active_run_dir finds it
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        std::fs::write(run.join(".daemon"), format!("{pid}:{now}")).ok();
        // Create fake api.sock
        std::fs::write(run.join("api.sock"), "").ok();
        let found = find_api_socket(&home);
        assert!(found.is_some());
        assert!(found
            .as_ref()
            .map(|s| s.ends_with("api.sock"))
            .unwrap_or(false));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn find_api_socket_run_dir_without_sock() {
        let home = tmp_home("no_sock");
        let pid = std::process::id();
        let run = home.join("run").join(pid.to_string());
        std::fs::create_dir_all(&run).ok();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        std::fs::write(run.join(".daemon"), format!("{pid}:{now}")).ok();
        // No api.sock file
        let found = find_api_socket(&home);
        assert!(found.is_none());
        std::fs::remove_dir_all(&home).ok();
    }
}
