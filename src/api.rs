//! Daemon JSON control API over TCP loopback.
//!
//! Protocol: NDJSON (one JSON request per line, one JSON response per line).
//! Port is published to `{run_dir}/api.port`; see `ipc.rs` for the port
//! registry and loopback-binding rules.

use crate::agent::{self, AgentRegistry, ExternalRegistry};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

pub type ConfigRegistry = Arc<Mutex<HashMap<String, crate::daemon::AgentConfig>>>;

/// Validate a caller-supplied `working_directory` against the AGEND_HOME and
/// (optionally) `AGEND_ALLOWED_WORK_ROOTS` — a platform-native path list
/// (`:`-separated on Unix, `;`-separated on Windows, same rules as `PATH`).
///
/// Rules:
/// - Path must not contain `..` components (blocks relative escape regardless
///   of whether the target exists).
/// - After canonicalising the deepest existing ancestor, the resolved path
///   must start with one of the allowed roots. This catches symlink escape
///   inside an otherwise-legal prefix.
///
/// Returns the resolved `PathBuf` on success. The caller is responsible for
/// creating the directory.
pub fn validate_working_directory(
    path: &std::path::Path,
    home: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    use std::path::{Component, PathBuf};
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        anyhow::bail!("working_directory must not contain '..'");
    }
    // Walk up to the deepest existing ancestor for canonicalisation. A path
    // pointing into a not-yet-created subdirectory is legal as long as its
    // existing prefix is inside an allowed root.
    let mut existing = path.to_path_buf();
    while !existing.as_os_str().is_empty() && !existing.exists() {
        if !existing.pop() {
            break;
        }
    }
    let anchor = if existing.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        std::fs::canonicalize(&existing).unwrap_or(existing.clone())
    };
    let tail = path.strip_prefix(&existing).unwrap_or(path);
    let resolved = anchor.join(tail);

    let mut roots: Vec<PathBuf> = Vec::new();
    roots.push(std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()));
    if let Some(extra) = std::env::var_os("AGEND_ALLOWED_WORK_ROOTS") {
        // `split_paths` uses the OS-native separator — `:` on Unix, `;` on
        // Windows. Raw `split(':')` broke Windows because `C:\...` paths
        // already contain a colon after the drive letter.
        for pb in std::env::split_paths(&extra).filter(|p| !p.as_os_str().is_empty()) {
            roots.push(std::fs::canonicalize(&pb).unwrap_or(pb));
        }
    }
    for root in &roots {
        if resolved.starts_with(root) {
            return Ok(resolved);
        }
    }
    anyhow::bail!(
        "working_directory '{}' escapes allowed roots (set AGEND_ALLOWED_WORK_ROOTS to widen)",
        resolved.display()
    )
}

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
    pub const UPDATE_TEAM: &str = "update_team";
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
    let listener: TcpListener = match crate::ipc::bind_loopback() {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "failed to bind API socket");
            return;
        }
    };
    let port = crate::ipc::local_port(&listener);
    let run_dir = crate::daemon::run_dir(home);
    if let Err(e) = crate::ipc::write_port(&run_dir, crate::ipc::API_NAME, port) {
        tracing::warn!(error = %e, "failed to publish API port");
        return;
    }
    // P1-10: Load the per-daemon auth cookie (already issued by
    // `daemon::run` / `verify::run` before any server thread spawned). If
    // it's missing we fail closed — running without auth would be worse
    // than not serving.
    let cookie = match crate::auth_cookie::read_cookie(&run_dir) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "api.cookie missing; aborting serve");
            return;
        }
    };
    tracing::info!(port, "API listening");

    for stream in listener.incoming().flatten() {
        let _ = stream.set_nodelay(true);
        // Slow-client hardening: set read/write deadlines so a stalled peer
        // cannot pin a session thread indefinitely. 30s is generous for a
        // JSON request line; control-plane calls are never slow on purpose.
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(30)));
        let reg = Arc::clone(&registry);
        let home = home.to_path_buf();
        let shutdown = Arc::clone(&shutdown);
        let cfgs = Arc::clone(&configs);
        let ext = Arc::clone(&externals);
        let tui = tui_tx.clone();
        // Cookie is `[u8; 32]` (Copy), each session gets its own copy so the
        // spawned closure satisfies `'static`.
        let session_cookie = cookie;
        std::thread::Builder::new()
            .name("api_handler".into())
            .spawn(move || {
                handle_session(
                    stream,
                    &reg,
                    &home,
                    &shutdown,
                    &cfgs,
                    &ext,
                    tui.as_ref(),
                    session_cookie,
                )
            })
            .ok();
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_session(
    stream: TcpStream,
    registry: &AgentRegistry,
    home: &Path,
    shutdown: &Arc<AtomicBool>,
    configs: &ConfigRegistry,
    externals: &ExternalRegistry,
    tui_tx: Option<&crate::app::TuiEventSender>,
    cookie: crate::auth_cookie::Cookie,
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

    // P1-10 gate: first NDJSON line must be `{"auth":"<hex>"}`. Read deadline
    // on the stream (set in `serve`) ensures a silent peer closes out in 30s
    // rather than pinning this worker thread.
    if let Err(e) = crate::auth_cookie::server_handshake_ndjson(&mut reader, &mut writer, &cookie) {
        tracing::warn!(error = %e, "API auth rejected");
        return;
    }

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
                        let mut child = crate::sync::lock_poisoned(&handle.child, "api_child");
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
                    let mut child = crate::sync::lock_poisoned(&handle.child, "api_child");
                    let _ = child.kill();
                    drop(child);
                }
                reg.remove(name);
                drop(reg);
                // Then remove config (no race: agent already gone from registry)
                crate::sync::lock_poisoned(configs, "api_configs").remove(name);
                // Cleanup the agent's published port file
                crate::ipc::remove_port(&crate::daemon::run_dir(home), name);
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
                let requested_work_dir = params["working_directory"]
                    .as_str()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| home.join("workspace").join(name));
                let work_dir = match validate_working_directory(&requested_work_dir, home) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ =
                            writeln!(writer, "{}", json!({"ok": false, "error": format!("{e}")}));
                        continue;
                    }
                };
                let size = crossterm::terminal::size().unwrap_or((120, 40));
                let spawn_mode = match params["mode"].as_str() {
                    Some("resume") => crate::backend::SpawnMode::Resume,
                    _ => crate::backend::SpawnMode::Fresh,
                };

                match spawn_one(
                    home, registry, name, command, &args, spawn_mode, &work_dir, size,
                ) {
                    Ok(()) => {
                        if let Some(tx) = tui_tx {
                            let layout_hint = crate::app::LayoutHint::parse_hint(
                                params["layout"].as_str().unwrap_or("tab"),
                            );
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
                                channel_len = tx.len(),
                                "SPAWN emitting InstanceCreated"
                            );
                            if let Err(e) = tx.try_send(crate::app::TuiEvent::InstanceCreated {
                                name: name.to_string(),
                                layout: layout_hint,
                                spawner,
                                target_pane,
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
                // `backends: [..]` — per-member backend (heterogeneous team).
                // Falls back to repeating `backend` `count` times when absent.
                let per_member_backends: Vec<String> =
                    if let Some(arr) = params["backends"].as_array() {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    } else {
                        let count = params["count"].as_u64().unwrap_or(0) as usize;
                        let backend = params["backend"].as_str().unwrap_or("claude").to_string();
                        vec![backend; count]
                    };
                let count = per_member_backends.len();
                tracing::info!(
                    team = team_name,
                    count,
                    backends = ?per_member_backends,
                    "CREATE_TEAM begin"
                );

                let mut spawned: Vec<(String, String)> = Vec::new(); // (name, backend)
                let mut failed: Vec<String> = Vec::new();
                let size = crossterm::terminal::size().unwrap_or((120, 40));
                for (i, backend) in per_member_backends.iter().enumerate() {
                    let inst_name = format!("{team_name}-{}", i + 1);
                    // Dedup: see SPAWN handler note. Re-creating a team with an
                    // existing name would otherwise overwrite the registry entry
                    // and orphan the previous tab's PTY subscription.
                    if agent::lock_registry(registry).contains_key(&inst_name) {
                        tracing::warn!(team = team_name, member = %inst_name, "CREATE_TEAM skip: name already exists");
                        failed.push(format!("{inst_name}: agent already exists"));
                        continue;
                    }
                    let work_dir = home.join("workspace").join(&inst_name);
                    match spawn_one(
                        home,
                        registry,
                        &inst_name,
                        backend,
                        &[],
                        crate::backend::SpawnMode::Fresh,
                        &work_dir,
                        size,
                    ) {
                        Ok(()) => {
                            tracing::info!(team = team_name, member = %inst_name, backend = %backend, "CREATE_TEAM spawn ok");
                            spawned.push((inst_name, backend.clone()));
                        }
                        Err(e) => {
                            tracing::warn!(team = team_name, member = %inst_name, backend = %backend, error = %e, "CREATE_TEAM spawn failed");
                            failed.push(format!("{inst_name}: {e}"));
                        }
                    }
                }
                tracing::info!(
                    team = team_name,
                    spawned = spawned.len(),
                    failed = failed.len(),
                    "CREATE_TEAM spawn phase done"
                );
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
                let spawned_names: Vec<String> = spawned.iter().map(|(n, _)| n.clone()).collect();
                let all_members: Vec<String> = existing
                    .into_iter()
                    .chain(spawned_names.iter().cloned())
                    .collect();

                if !spawned.is_empty() {
                    let entries: Vec<(String, crate::fleet::InstanceYamlEntry)> = spawned
                        .iter()
                        .map(|(name, be)| {
                            (
                                name.clone(),
                                crate::fleet::InstanceYamlEntry {
                                    backend: Some(be.clone()),
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
                        let members_for_event = spawned_names.clone();
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
                        tracing::warn!(
                            team = team_name,
                            "CREATE_TEAM not emitting (spawned empty)"
                        );
                    }
                } else {
                    tracing::warn!(team = team_name, "CREATE_TEAM no tui_tx, event dropped");
                }
                let mut resp = json!({"ok": true, "result": result, "spawned": &spawned_names});
                if !failed.is_empty() {
                    resp["failed"] = json!(failed);
                }
                resp
            }
            method::UPDATE_TEAM => {
                let team_name = match params["name"].as_str() {
                    Some(n) => n.to_string(),
                    None => {
                        let _ =
                            writeln!(writer, "{}", json!({"ok": false, "error": "missing name"}));
                        continue;
                    }
                };
                // Snapshot the pre-mutation roster so the TUI event carries the
                // *effective* diff (noop adds like re-adding an existing member
                // must not trigger a pane move).
                let before = crate::teams::get_members(home, &team_name);
                let result = crate::teams::update(home, params);
                let after = crate::teams::get_members(home, &team_name);
                let before_set: std::collections::HashSet<&String> = before.iter().collect();
                let after_set: std::collections::HashSet<&String> = after.iter().collect();
                let added: Vec<String> = after
                    .iter()
                    .filter(|m| !before_set.contains(m))
                    .cloned()
                    .collect();
                let removed: Vec<String> = before
                    .iter()
                    .filter(|m| !after_set.contains(m))
                    .cloned()
                    .collect();
                if let Some(tx) = tui_tx {
                    if !added.is_empty() || !removed.is_empty() {
                        tracing::info!(
                            team = %team_name,
                            added = ?added,
                            removed = ?removed,
                            "UPDATE_TEAM emitting TeamMembersChanged"
                        );
                        if let Err(e) = tx.try_send(crate::app::TuiEvent::TeamMembersChanged {
                            name: team_name.clone(),
                            added,
                            removed,
                        }) {
                            tracing::warn!(team = %team_name, error = %e, "TeamMembersChanged try_send failed");
                        }
                    }
                }
                json!({"ok": true, "result": result})
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
#[allow(clippy::too_many_arguments)]
fn spawn_one(
    home: &Path,
    registry: &AgentRegistry,
    name: &str,
    backend: &str,
    args: &[String],
    spawn_mode: crate::backend::SpawnMode,
    work_dir: &Path,
    size: (u16, u16),
) -> anyhow::Result<()> {
    std::fs::create_dir_all(work_dir).ok();
    agent::spawn_agent(
        &agent::SpawnConfig {
            name,
            backend_command: backend,
            args,
            spawn_mode,
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
    let rdir = crate::daemon::run_dir(home);
    let reg = Arc::clone(registry);
    let n = name.to_string();
    std::thread::Builder::new()
        .name(format!("{n}_tui"))
        .spawn(move || crate::daemon::serve_agent_tui(&n, &rdir, &reg))
        .ok();
    Ok(())
}

/// Send a request to the daemon API and read one NDJSON response.
///
/// Performs the P1-10 cookie handshake first: reads `api.cookie` from the
/// active daemon's run dir, sends `{"auth":"<hex>"}`, and rejects the call
/// if the server does not reply `{"ok":true}`. The cookie file has mode
/// 0600 so only the daemon's user can read it — this is the peer-UID
/// substitute for TCP loopback (see `auth_cookie.rs`).
pub fn call(home: &Path, request: &Value) -> anyhow::Result<Value> {
    let stream = crate::ipc::connect_api(home)?;
    let run = crate::daemon::find_active_run_dir(home)
        .ok_or_else(|| anyhow::anyhow!("no active daemon (run dir not found)"))?;
    let cookie = crate::auth_cookie::read_cookie(&run)?;

    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    crate::auth_cookie::client_handshake_ndjson(&mut reader, &mut writer, &cookie)?;

    writeln!(writer, "{}", request)?;
    writer.flush()?;

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let resp: Value = serde_json::from_str(line.trim())?;
    Ok(resp)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Serializes tests that mutate `AGEND_ALLOWED_WORK_ROOTS` — env mutation
    /// from parallel tests races otherwise.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        crate::sync::lock_poisoned(LOCK.get_or_init(|| Mutex::new(())), "api_env_guard")
    }

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
    fn validate_work_dir_rejects_parent_dir() {
        let home = tmp_home("validate_parent");
        let bad = home.join("..").join("escape");
        let err = validate_working_directory(&bad, &home).unwrap_err();
        assert!(
            format!("{err}").contains(".."),
            "expected parent-dir rejection, got: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn validate_work_dir_allows_under_home() {
        let home = tmp_home("validate_home");
        let ok = home.join("workspace").join("agent");
        let resolved =
            validate_working_directory(&ok, &home).expect("path under home must validate");
        assert!(resolved.starts_with(std::fs::canonicalize(&home).unwrap()));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn validate_work_dir_rejects_outside_home() {
        let _g = env_guard();
        let home = tmp_home("validate_outside");
        // Pick a path that definitely exists and is not under home.
        let outside = std::path::PathBuf::from("/tmp");
        // Ensure AGEND_ALLOWED_WORK_ROOTS isn't accidentally opening this up.
        let prev = std::env::var("AGEND_ALLOWED_WORK_ROOTS").ok();
        std::env::remove_var("AGEND_ALLOWED_WORK_ROOTS");
        let err = validate_working_directory(&outside, &home).unwrap_err();
        if let Some(v) = prev {
            std::env::set_var("AGEND_ALLOWED_WORK_ROOTS", v);
        }
        assert!(
            format!("{err}").contains("escapes"),
            "expected escape rejection, got: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn validate_work_dir_honors_allowed_roots_env() {
        let _g = env_guard();
        let home = tmp_home("validate_env_root");
        let root = std::env::temp_dir().join(format!(
            "agend-extra-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("mkdir extra root");
        let inside = root.join("agent");
        let prev = std::env::var("AGEND_ALLOWED_WORK_ROOTS").ok();
        std::env::set_var("AGEND_ALLOWED_WORK_ROOTS", root.display().to_string());
        let result = validate_working_directory(&inside, &home);
        match prev {
            Some(v) => std::env::set_var("AGEND_ALLOWED_WORK_ROOTS", v),
            None => std::env::remove_var("AGEND_ALLOWED_WORK_ROOTS"),
        }
        result.expect("path under AGEND_ALLOWED_WORK_ROOTS must validate");
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn validate_work_dir_rejects_symlink_escape() {
        // Stage 4 P1-8 regression guard: a symlink inside an allowed root
        // pointing OUT of all allowed roots must be rejected after canonicalisation.
        let _g = env_guard();
        let home = tmp_home("validate_symlink_escape");
        // Create a symlink at `{home}/escape` → /tmp (outside any allowed root).
        let target = std::path::PathBuf::from("/tmp");
        let link = home.join("escape");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        let prev = std::env::var("AGEND_ALLOWED_WORK_ROOTS").ok();
        std::env::remove_var("AGEND_ALLOWED_WORK_ROOTS");
        // Request a path *under* the symlink. After canonicalisation it should
        // resolve outside `home` and be rejected.
        let requested = link.join("agent");
        let result = validate_working_directory(&requested, &home);
        if let Some(v) = prev {
            std::env::set_var("AGEND_ALLOWED_WORK_ROOTS", v);
        }
        match result {
            Ok(resolved) => panic!(
                "expected symlink escape rejection, but validated as {}",
                resolved.display()
            ),
            Err(e) => assert!(
                format!("{e}").contains("escapes"),
                "expected escape rejection, got: {e}"
            ),
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn call_fails_without_daemon() {
        let home = tmp_home("call_no_daemon");
        let err = call(&home, &json!({"method": "list"})).unwrap_err();
        // No active daemon → either "no active daemon" or a TCP ConnectionRefused
        let msg = format!("{err:#}");
        assert!(
            msg.to_ascii_lowercase().contains("no active daemon")
                || msg.to_ascii_lowercase().contains("refused"),
            "unexpected error: {msg}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
