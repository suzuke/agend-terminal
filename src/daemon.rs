//! Daemon: manages agent registry, TUI sockets, auto-respawn, and fleet lifecycle.

use crate::agent::{self, AgentRegistry};
use crate::framing::{self, TAG_DATA, TAG_RESIZE};
use crate::health::HealthTracker;

use portable_pty::PtySize;
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Agent spawn config — stored for auto-respawn.
#[derive(Clone)]
struct AgentConfig {
    name: String,
    command: String,
    args: Vec<String>,
    env: Option<HashMap<String, String>>,
    working_dir: Option<PathBuf>,
    submit_key: String,
}

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

        let (rx, pty_writer, pty_master, core) = {
            let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
            let agent = match reg.get(name) {
                Some(a) => a,
                None => continue,
            };
            let (rx, dump) = agent::subscribe_with_dump(agent);
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

pub fn agent_socket_path(home: &Path, name: &str) -> String {
    home.join(format!("{name}.sock")).display().to_string()
}

/// Start daemon: spawn agents, handle crashes with auto-respawn.
pub fn run(
    home: &Path,
    agents: Vec<(String, String, Vec<String>, Option<HashMap<String, String>>, Option<PathBuf>, String)>,
) -> anyhow::Result<()> {
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));

    // Crash channel for auto-respawn
    let (crash_tx, crash_rx) = crossbeam::channel::unbounded::<String>();

    // Store configs for respawn + per-agent health trackers
    let configs: Arc<Mutex<HashMap<String, AgentConfig>>> = Arc::new(Mutex::new(HashMap::new()));
    let health_trackers: Arc<Mutex<HashMap<String, HealthTracker>>> = Arc::new(Mutex::new(HashMap::new()));

    eprintln!("[daemon] starting {} agent(s)", agents.len());

    for (name, command, args, env, working_dir, submit_key) in &agents {
        let config = AgentConfig {
            name: name.clone(),
            command: command.clone(),
            args: args.clone(),
            env: env.clone(),
            working_dir: working_dir.clone(),
            submit_key: submit_key.clone(),
        };
        configs.lock().unwrap().insert(name.clone(), config);
        health_trackers.lock().unwrap().insert(name.clone(), HealthTracker::new());

        let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
        agent::spawn_agent(
            name, command, args, cols, rows,
            env.as_ref(), working_dir.as_deref(), submit_key,
            &registry, Some(home), Some(crash_tx.clone()),
        )?;

        let sock = agent_socket_path(home, name);
        let reg = Arc::clone(&registry);
        let n = name.clone();
        std::thread::Builder::new()
            .name(format!("{n}_tui_server"))
            .spawn(move || serve_agent_tui(&n, &sock, &reg))?;

        if agents.len() > 1 {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    // Shutdown flag
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // API socket server
    let api_reg = Arc::clone(&registry);
    let api_home = home.to_path_buf();
    let api_shutdown = Arc::clone(&shutdown);
    std::thread::Builder::new()
        .name("api_server".into())
        .spawn(move || crate::api::serve(&api_home, api_reg, api_shutdown))?;

    let shutdown2 = Arc::clone(&shutdown);
    match ctrlc::set_handler(move || {
        eprintln!("\n[daemon] shutting down...");
        shutdown2.store(true, std::sync::atomic::Ordering::Relaxed);
    }) {
        Ok(()) => {}
        Err(e) => eprintln!("[daemon] warning: Ctrl+C handler failed: {e}. Use `stop`."),
    }

    eprintln!("[daemon] running. Ctrl+C or `agend-terminal stop` to stop.");

    // Main loop: handle crashes + shutdown
    loop {
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }

        // Check for crashes (non-blocking, 200ms timeout)
        match crash_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(crashed_name) => {
                eprintln!("[health] {crashed_name} crashed");

                // Get config for respawn
                let config = configs.lock().unwrap().get(&crashed_name).cloned();
                let config = match config {
                    Some(c) => c,
                    None => {
                        eprintln!("[health] {crashed_name}: no config for respawn");
                        continue;
                    }
                };

                // Record crash in health tracker
                let (should_respawn, delay, should_notify) = {
                    let mut trackers = health_trackers.lock().unwrap();
                    let tracker = trackers.entry(crashed_name.clone()).or_insert_with(HealthTracker::new);
                    tracker.record_crash()
                };

                if should_notify {
                    let state = health_trackers.lock().unwrap()
                        .get(&crashed_name)
                        .map(|h| h.state.display_name())
                        .unwrap_or("unknown");
                    eprintln!("[health] {crashed_name}: {} — notifying", state);
                    // TODO: Telegram notification
                }

                if should_respawn {
                    eprintln!("[health] {crashed_name}: respawning in {:?}", delay);
                    let reg = Arc::clone(&registry);
                    let home = home.to_path_buf();
                    let tx = crash_tx.clone();
                    let trackers = Arc::clone(&health_trackers);

                    std::thread::Builder::new()
                        .name(format!("{crashed_name}_respawn"))
                        .spawn(move || {
                            std::thread::sleep(delay);
                            let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
                            match agent::spawn_agent(
                                &config.name, &config.command, &config.args,
                                cols, rows,
                                config.env.as_ref(), config.working_dir.as_deref(),
                                &config.submit_key, &reg, Some(&home), Some(tx),
                            ) {
                                Ok(()) => {
                                    eprintln!("[health] {}: respawned", config.name);

                                    // Mark respawn OK
                                    if let Ok(mut t) = trackers.lock() {
                                        if let Some(h) = t.get_mut(&config.name) {
                                            h.respawn_ok();
                                        }
                                    }

                                    // Inject system message
                                    std::thread::sleep(std::time::Duration::from_secs(2));
                                    {
                                        let r = reg.lock().unwrap_or_else(|e| e.into_inner());
                                        if let Some(handle) = r.get(&config.name) {
                                            let reason = trackers.lock().ok()
                                                .and_then(|t| t.get(&config.name).map(|h| h.crash_reason().to_string()))
                                                .unwrap_or_else(|| "crash".into());
                                            let msg = format!(
                                                "[system] Agent restarted due to {reason}. Previous context was lost.\r"
                                            );
                                            let _ = agent::write_to_agent(handle, msg.as_bytes());
                                        }
                                    }

                                    // Start TUI socket for respawned agent
                                    let sock = agent_socket_path(&home, &config.name);
                                    let n = config.name.clone();
                                    let reg2 = Arc::clone(&reg);
                                    std::thread::Builder::new()
                                        .name(format!("{n}_tui_server"))
                                        .spawn(move || serve_agent_tui(&n, &sock, &reg2))
                                        .ok();
                                }
                                Err(e) => {
                                    eprintln!("[health] {}: respawn failed: {e}", config.name);
                                }
                            }
                        })
                        .ok();
                } else {
                    eprintln!("[health] {crashed_name}: max retries exceeded, not respawning");
                }
            }
            Err(crossbeam::channel::RecvTimeoutError::Timeout) => {} // Normal
            Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Shutdown cleanup
    eprintln!("[daemon] cleaning up...");
    {
        let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        for (name, agent) in reg.iter() {
            let mut child = agent.child.lock().unwrap_or_else(|e| e.into_inner());
            let _ = child.kill();
            eprintln!("[daemon] killed {name}");
        }
    }
    for (name, _, _, _, _, _) in &agents {
        let sock = agent_socket_path(home, name);
        let _ = std::fs::remove_file(&sock);
    }
    let _ = std::fs::remove_file(crate::api::api_socket_path(home));

    {
        let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        reg.clear();
    }

    std::thread::sleep(std::time::Duration::from_millis(500));
    eprintln!("[daemon] exiting.");
    std::process::exit(0);
}
