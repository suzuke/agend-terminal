//! Daemon: manages agent registry, TUI sockets, and fleet lifecycle.

use crate::agent::{self, AgentRegistry};
use crate::framing::{self, TAG_DATA, TAG_RESIZE};
#[allow(unused_imports)]
use std::io::Read;
use portable_pty::PtySize;
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Start the TUI socket server for an agent (blocks the calling thread).
pub fn serve_agent_tui(name: &str, socket_path: &str, registry: &AgentRegistry) {
    let _ = std::fs::remove_file(socket_path);
    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[{name}] failed to bind TUI socket {socket_path}: {e}");
            return;
        }
    };
    eprintln!("[{name}] TUI socket on {socket_path}");

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        eprintln!("[{name}] TUI client connected");

        // Atomic subscribe + screen dump (under core lock — no output gap)
        let (rx, pty_writer, pty_master, core) = {
            let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
            let agent = match reg.get(name) {
                Some(a) => a,
                None => continue,
            };
            let (rx, dump) = agent::subscribe_with_dump(agent);
            // Send screen dump to client
            if framing::write_frame(&mut stream, &dump).is_err() {
                continue;
            }
            (
                rx,
                Arc::clone(&agent.pty_writer),
                Arc::clone(&agent.pty_master),
                Arc::clone(&agent.core),
            )
        };

        // Output thread: forward broadcast to this client
        let mut write_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let n = name.to_string();
        std::thread::Builder::new()
            .name(format!("{n}_tui_out"))
            .spawn(move || {
                loop {
                    match rx.recv() {
                        Ok(data) => {
                            if framing::write_frame(&mut write_stream, &data).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .ok();

        // Input thread: forward client input to PTY, handle resize
        let read_stream = stream;
        let n = name.to_string();
        std::thread::Builder::new()
            .name(format!("{n}_tui_in"))
            .spawn(move || {
                let mut reader = read_stream;
                loop {
                    match framing::read_tagged_frame(&mut reader) {
                        Ok((TAG_DATA, data)) => {
                            if pty_writer
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .write_all(&data)
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok((TAG_RESIZE, data)) if data.len() == 4 => {
                            let cols = u16::from_be_bytes([data[0], data[1]]);
                            let rows = u16::from_be_bytes([data[2], data[3]]);
                            let _ = pty_master
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .resize(PtySize {
                                    rows,
                                    cols,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                });
                            if let Ok(mut c) = core.lock() {
                                c.vterm.resize(cols, rows);
                            }
                        }
                        _ => break,
                    }
                }
                eprintln!("[{n}] TUI client disconnected");
            })
            .ok();
    }
}

/// Socket path for an agent's TUI connection.
pub fn agent_socket_path(home: &Path, name: &str) -> String {
    home.join(format!("{name}.sock")).display().to_string()
}

/// Start daemon: spawn agents from config, serve TUI sockets.
pub fn run(
    home: &Path,
    agents: Vec<(String, String, Vec<String>, Option<HashMap<String, String>>, Option<PathBuf>, String)>,
) -> anyhow::Result<()> {
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));

    eprintln!("[daemon] starting {} agent(s)", agents.len());

    for (name, command, args, env, working_dir, submit_key) in &agents {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
        agent::spawn_agent(
            name,
            command,
            args,
            cols,
            rows,
            env.as_ref(),
            working_dir.as_deref(),
            submit_key,
            &registry,
        )?;

        // Start TUI socket server for this agent in a new thread
        let sock = agent_socket_path(home, name);
        let reg = Arc::clone(&registry);
        let n = name.clone();
        std::thread::Builder::new()
            .name(format!("{n}_tui_server"))
            .spawn(move || serve_agent_tui(&n, &sock, &reg))?;

        // Staggered startup
        if agents.len() > 1 {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    // Graceful shutdown
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown2 = Arc::clone(&shutdown);
    ctrlc::set_handler(move || {
        eprintln!("\n[daemon] shutting down...");
        shutdown2.store(true, std::sync::atomic::Ordering::Relaxed);
    })
    .ok();

    eprintln!("[daemon] running. Ctrl+C to stop.");

    // Wait for shutdown signal
    while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Cleanup sockets
    eprintln!("[daemon] cleaning up...");
    for (name, _, _, _, _, _) in &agents {
        let sock = agent_socket_path(home, name);
        let _ = std::fs::remove_file(&sock);
    }

    Ok(())
}
