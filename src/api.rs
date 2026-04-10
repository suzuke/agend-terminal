//! Daemon JSON control API over Unix socket.
//!
//! Protocol: NDJSON (one JSON request per line, one JSON response per line).
//! Socket: {home}/api.sock

use crate::agent::{self, AgentRegistry};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Start API socket server (blocks calling thread).
pub fn serve(home: &Path, registry: AgentRegistry, shutdown: Arc<AtomicBool>) {
    let sock = api_socket_path(home);
    let _ = std::fs::remove_file(&sock);

    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[api] failed to bind {sock}: {e}");
            return;
        }
    };
    eprintln!("[api] listening on {sock}");

    for stream in listener.incoming().flatten() {
        let reg = Arc::clone(&registry);
        let home = home.to_path_buf();
        let shutdown = Arc::clone(&shutdown);
        std::thread::Builder::new()
            .name("api_handler".into())
            .spawn(move || handle_session(stream, &reg, &home, &shutdown))
            .ok();
    }
}

pub fn api_socket_path(home: &Path) -> String {
    home.join("api.sock").display().to_string()
}

fn handle_session(stream: UnixStream, registry: &AgentRegistry, home: &Path, shutdown: &Arc<AtomicBool>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
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
                let _ = writeln!(writer, "{}", json!({"ok": false, "error": format!("parse: {e}")}));
                continue;
            }
        };

        let method = req["method"].as_str().unwrap_or("");
        let params = &req["params"];

        let response = match method {
            "list" => {
                let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
                let agents: Vec<Value> = reg
                    .iter()
                    .map(|(name, handle)| {
                        let state = handle.core.lock()
                            .map(|mut c| c.state.get_state().display_name().to_string())
                            .unwrap_or_else(|_| "unknown".into());
                        json!({
                            "name": name,
                            "command": handle.command,
                            "submit_key": handle.submit_key,
                            "inject_prefix": handle.inject_prefix,
                            "state": state,
                        })
                    })
                    .collect();
                json!({"ok": true, "result": {"agents": agents}})
            }
            "inject" => {
                let name = params["name"].as_str().unwrap_or("");
                let data = params["data"].as_str().unwrap_or("");
                // "raw" flag: send bytes as-is (for attach-like paths)
                let raw = params["raw"].as_bool().unwrap_or(false);
                let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
                match reg.get(name) {
                    Some(handle) => {
                        let result = if raw {
                            agent::write_to_agent(handle, data.as_bytes())
                        } else {
                            // Smart inject: text only, prefix+submit added by inject_to_agent
                            agent::inject_to_agent(handle, data.as_bytes())
                        };
                        match result {
                            Ok(()) => json!({"ok": true, "result": {"bytes": data.len()}}),
                            Err(e) => json!({"ok": false, "error": format!("{e}")}),
                        }
                    },
                    None => json!({"ok": false, "error": format!("agent '{name}' not found")}),
                }
            }
            "kill" => {
                let name = params["name"].as_str().unwrap_or("");
                let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
                match reg.get(name) {
                    Some(handle) => {
                        let mut child = handle.child.lock().unwrap_or_else(|e| e.into_inner());
                        let _ = child.kill();
                        drop(child);
                        drop(reg);
                        // Socket cleanup happens via session reaper
                        json!({"ok": true})
                    }
                    None => json!({"ok": false, "error": format!("agent '{name}' not found")}),
                }
            }
            "spawn" => {
                let name = match params["name"].as_str() {
                    Some(n) => n,
                    None => {
                        let _ = writeln!(writer, "{}", json!({"ok": false, "error": "missing name"}));
                        continue;
                    }
                };
                let command = params["command"].as_str().unwrap_or("bash");
                let args: Vec<String> = params["args"]
                    .as_str()
                    .map(|s| s.split_whitespace().map(String::from).collect())
                    .unwrap_or_default();

                let work_dir = params["working_directory"]
                    .as_str()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| home.join("workspaces").join(name));
                std::fs::create_dir_all(&work_dir).ok();

                let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));

                match agent::spawn_agent(
                    name, command, &args, cols, rows, None,
                    Some(work_dir.as_path()), "\r", registry, Some(home), None,
                ) {
                    Ok(()) => {
                        // Start TUI socket
                        let sock = crate::daemon::agent_socket_path(home, name);
                        let reg = Arc::clone(registry);
                        let n = name.to_string();
                        std::thread::Builder::new()
                            .name(format!("{n}_tui"))
                            .spawn(move || crate::daemon::serve_agent_tui(&n, &sock, &reg))
                            .ok();

                        json!({"ok": true, "result": {"name": name}})
                    }
                    Err(e) => json!({"ok": false, "error": format!("{e}")}),
                }
            }
            "send" => {
                let from = params["from"].as_str().unwrap_or("unknown");
                let target = params["target"].as_str().unwrap_or("");
                let text = params["text"].as_str().unwrap_or("");

                // Enqueue in inbox
                let msg = crate::inbox::InboxMessage {
                    from: format!("from:{from}"),
                    text: text.to_string(),
                    kind: params.get("kind").and_then(|v| v.as_str()).map(String::from),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                };
                let _ = crate::inbox::enqueue(home, target, msg);

                // Direct write to PTY (daemon has registry — no API loop)
                let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(handle) = reg.get(target) {
                    let display_text = if text.chars().count() > 200 {
                        let truncated: String = text.chars().take(200).collect();
                        format!("{truncated}... (use inbox tool)")
                    } else {
                        text.to_string()
                    };
                    let notification = format!("[from:{from}] {display_text}");
                    let _ = agent::inject_to_agent(handle, notification.as_bytes());
                }
                json!({"ok": true})
            }
            "shutdown" => {
                eprintln!("[api] shutdown requested");
                shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
                json!({"ok": true})
            }
            _ => json!({"ok": false, "error": format!("unknown method: {method}")}),
        };

        let _ = writeln!(writer, "{}", response);
        let _ = writer.flush();
    }
}

/// Send a request to the API socket and get response.
pub fn call(home: &Path, request: &Value) -> anyhow::Result<Value> {
    let sock = api_socket_path(home);
    let mut stream = UnixStream::connect(&sock)?;
    writeln!(stream, "{}", request)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let resp: Value = serde_json::from_str(line.trim())?;
    Ok(resp)
}
