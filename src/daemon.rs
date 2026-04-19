//! Daemon: manages agent registry, TUI sockets, auto-respawn, fleet lifecycle,
//! schedule checking, health monitoring, Telegram notifications.

use crate::agent::{self, AgentRegistry};
use crate::framing::{self, TAG_DATA, TAG_RESIZE};
use teloxide::payloads::SendMessageSetters;
use teloxide::prelude::Requester;

use portable_pty::PtySize;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Agent spawn config — stored for auto-respawn.
#[derive(Clone)]
pub struct AgentConfig {
    pub name: String,
    pub backend_command: String,
    pub args: Vec<String>,
    pub env: Option<HashMap<String, String>>,
    pub working_dir: Option<PathBuf>,
    /// Original repo root (before worktree redirect).
    pub worktree_source: Option<PathBuf>,
    pub submit_key: String,
}

/// Start the TUI socket server for an agent (blocks the calling thread).
///
/// Binds a TCP loopback port, publishes it to `{run_dir}/{name}.port`, then
/// accepts connections. Removes the port file when the listener exits.
pub fn serve_agent_tui(name: &str, run_dir: &Path, registry: &AgentRegistry) {
    let listener = match crate::ipc::bind_loopback() {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(agent = name, error = %e, "failed to bind TUI socket");
            return;
        }
    };
    let port = crate::ipc::local_port(&listener);
    if let Err(e) = crate::ipc::write_port(run_dir, name, port) {
        tracing::warn!(agent = name, error = %e, "failed to publish TUI port");
        return;
    }
    tracing::info!(agent = name, port, "TUI socket ready");

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = stream.set_nodelay(true);
        tracing::info!(agent = name, "TUI client connected");

        // Protocol version handshake: send version byte before any framed data
        if stream.write_all(&[framing::PROTOCOL_VERSION]).is_err() {
            continue;
        }
        if stream.flush().is_err() {
            continue;
        }

        let (rx, pty_writer, pty_master, core) = {
            let reg = agent::lock_registry(registry);
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
        if let Err(e) = std::thread::Builder::new()
            .name(format!("{n}_tui_out"))
            .spawn(move || {
                while let Ok(data) = rx.recv() {
                    if framing::write_frame(&mut write_stream, &data).is_err() {
                        break;
                    }
                }
            })
        {
            tracing::warn!(agent = %n, error = %e, "failed to spawn TUI output thread");
        }

        let read_stream = stream;
        let n = name.to_string();
        let n_err = n.clone();
        if let Err(e) = std::thread::Builder::new()
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
                            let _ = pty_master.lock().unwrap_or_else(|e| e.into_inner()).resize(
                                PtySize {
                                    rows,
                                    cols,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                },
                            );
                            if let Ok(mut c) = core.lock() {
                                c.vterm.resize(cols, rows);
                            }
                        }
                        _ => break,
                    }
                }
                tracing::info!(agent = %n, "TUI client disconnected");
            })
        {
            tracing::warn!(agent = %n_err, error = %e, "failed to spawn TUI input thread");
        }
    }
}

/// Get the PID-isolated run directory for the current daemon.
pub fn run_dir(home: &Path) -> PathBuf {
    home.join("run").join(std::process::id().to_string())
}

/// Find any active run directory (for CLI commands connecting to daemon).
/// Verifies identity via .daemon file (PID + start timestamp) to prevent PID reuse false positives.
pub fn find_active_run_dir(home: &Path) -> Option<PathBuf> {
    let run = home.join("run");
    if !run.exists() {
        return None;
    }
    for entry in std::fs::read_dir(&run).ok()?.flatten() {
        let pid_str = entry.file_name().to_string_lossy().to_string();
        if let Ok(pid) = pid_str.parse::<u32>() {
            // Check if PID is alive
            let alive = crate::process::is_pid_alive(pid);
            if !alive {
                tracing::info!(path = %entry.path().display(), "cleaning stale run dir");
                let _ = std::fs::remove_dir_all(entry.path());
                continue;
            }
            // Verify identity: read .daemon file with start timestamp
            let daemon_file = entry.path().join(".daemon");
            if let Ok(content) = std::fs::read_to_string(&daemon_file) {
                // Format: "pid:start_time"
                if let Some((file_pid, _start_time)) = content.trim().split_once(':') {
                    if file_pid == pid_str {
                        return Some(entry.path());
                    }
                    // PID alive but .daemon file has different PID → PID was reused
                    tracing::info!(pid, old_pid = file_pid, "PID reused, cleaning");
                    let _ = std::fs::remove_dir_all(entry.path());
                    continue;
                }
            }
            // No .daemon file but PID alive → legacy or corrupted, accept it
            return Some(entry.path());
        }
    }
    None
}

/// Write daemon identity file for PID reuse detection.
fn write_daemon_id(run_dir: &Path) {
    let pid = std::process::id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::write(run_dir.join(".daemon"), format!("{pid}:{now}"));
}

/// Agent definition tuple for daemon startup.
pub type AgentDef = (
    String,
    String,
    Vec<String>,
    Option<HashMap<String, String>>,
    Option<PathBuf>,
    String,
);

/// Start daemon: spawn agents, handle crashes with auto-respawn.
pub fn run(home: &Path, agents: Vec<AgentDef>) -> anyhow::Result<()> {
    // Acquire exclusive daemon lock (prevents TOCTOU race)
    std::fs::create_dir_all(home)?;
    let lock_path = home.join(".daemon.lock");
    let lock_file = std::fs::File::create(&lock_path)?;
    use fs2::FileExt;
    lock_file
        .try_lock_exclusive()
        .map_err(|e| anyhow::anyhow!("Another daemon is already running (lock held): {e}"))?;

    // Check for existing daemon (secondary check after lock acquired)
    if let Some(existing) = find_active_run_dir(home) {
        anyhow::bail!("Another daemon is already running ({})", existing.display());
    }

    // Create PID-isolated run directory with identity file
    let run = run_dir(home);
    std::fs::create_dir_all(&run)?;
    write_daemon_id(&run);
    tracing::info!(path = %run.display(), "run dir");

    // Check for previous snapshot if fleet.yaml doesn't exist
    if !home.join("fleet.yaml").exists() {
        if let Some(snapshot) = crate::snapshot::load(home) {
            tracing::info!(
                count = snapshot.agents.len(),
                timestamp = %snapshot.timestamp,
                "previous snapshot found"
            );
        }
    }

    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));

    // External agents registry (connected via `agend-terminal connect`)
    let externals: crate::agent::ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));

    // Crash channel for auto-respawn
    let (crash_tx, crash_rx) = crossbeam::channel::unbounded::<String>();

    // Store configs for respawn
    let configs: Arc<Mutex<HashMap<String, AgentConfig>>> = Arc::new(Mutex::new(HashMap::new()));

    // Shutdown flag — shared with agent reapers to distinguish crash from shutdown
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    tracing::info!(count = agents.len(), "starting agents");

    for (name, command, args, env, working_dir, submit_key) in &agents {
        let worktree_source = working_dir.as_ref().and_then(|wd| {
            wd.display().to_string().contains(".worktrees/").then(|| {
                wd.parent()
                    .and_then(|p| p.parent())
                    .map(|p| p.to_path_buf())
            })?
        });
        configs.lock().unwrap_or_else(|e| e.into_inner()).insert(
            name.clone(),
            AgentConfig {
                name: name.clone(),
                backend_command: command.clone(),
                args: args.clone(),
                env: env.clone(),
                working_dir: working_dir.clone(),
                worktree_source,
                submit_key: submit_key.clone(),
            },
        );

        let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
        agent::spawn_agent(
            &agent::SpawnConfig {
                name,
                backend_command: command,
                args,
                cols,
                rows,
                env: env.as_ref(),
                working_dir: working_dir.as_deref(),
                submit_key,
                home: Some(home),
                crash_tx: Some(crash_tx.clone()),
                shutdown: Some(Arc::clone(&shutdown)),
            },
            &registry,
        )?;

        let rdir = run_dir(home);
        let reg = Arc::clone(&registry);
        let n = name.clone();
        std::thread::Builder::new()
            .name(format!("{n}_tui_server"))
            .spawn(move || serve_agent_tui(&n, &rdir, &reg))?;
        if agents.len() > 1 {
            let stagger_ms: u64 = std::env::var("AGEND_SPAWN_STAGGER_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(500);
            std::thread::sleep(std::time::Duration::from_millis(stagger_ms));
        }
    }

    // API socket server
    let api_reg = Arc::clone(&registry);
    let api_home = home.to_path_buf();
    let api_shutdown = Arc::clone(&shutdown);
    let api_configs = Arc::clone(&configs);
    let api_externals = Arc::clone(&externals);
    std::thread::Builder::new()
        .name("api_server".into())
        .spawn(move || {
            crate::api::serve(
                &api_home,
                api_reg,
                api_shutdown,
                api_configs,
                api_externals,
                None,
            )
        })?;

    // Shutdown wake channel — ctrlc handler sends on this so the main loop's
    // select! wakes immediately instead of waiting up to 10s for the next tick.
    let (shutdown_tx, shutdown_rx) = crossbeam::channel::bounded::<()>(1);
    let shutdown2 = Arc::clone(&shutdown);
    // Windows: re-enable CTRL+C delivery in case something (inherited
    // parent state, a dependency's init) has set the per-process "ignore
    // CTRL+C" flag. Without this, `SetConsoleCtrlHandler` routines are
    // skipped entirely for CTRL_C_EVENT and the daemon appears unresponsive
    // to Ctrl+C (while `agend-terminal stop` still works). CTRL_BREAK_EVENT
    // is unaffected by the flag — that's how the bug was isolated.
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        SetConsoleCtrlHandler(None, 0);
    }
    match ctrlc::set_handler(move || {
        // Sentinel: confirms the handler fired (observable even when the
        // daemon's stdout console is invisible or has already been torn
        // down). Used for Windows Ctrl+C diagnostics.
        if let Ok(path) = std::env::var("AGEND_CTRLC_SENTINEL") {
            let _ = std::fs::write(
                &path,
                format!("fired at {:?}\n", std::time::SystemTime::now()),
            );
        }
        tracing::info!("shutting down...");
        shutdown2.store(true, std::sync::atomic::Ordering::Relaxed);
        // try_send: if already signaled, the flag is enough.
        let _ = shutdown_tx.try_send(());
    }) {
        Ok(()) => {}
        Err(e) => tracing::warn!(error = %e, "Ctrl+C handler failed, use `stop`"),
    }

    crate::event_log::log(
        home,
        "daemon_start",
        "",
        &format!("{} agents", agents.len()),
    );
    tracing::info!("running, Ctrl+C or `agend-terminal stop` to stop");

    let mut last_snapshot_json = String::new();

    // Periodic tick channel (every 10s for health/schedule/session maintenance)
    let tick_rx = {
        let (tx, rx) = crossbeam::channel::bounded(1);
        std::thread::Builder::new()
            .name("daemon_tick".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(10));
                if tx.send(()).is_err() {
                    break;
                }
            })
            .ok();
        rx
    };

    // Main loop: event-driven via select on crash channel + periodic tick
    loop {
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }

        // Block until a crash event, periodic tick, or shutdown signal.
        let crashed_name: Option<String>;
        crossbeam::select! {
            recv(crash_rx) -> msg => {
                crashed_name = msg.ok();
            }
            recv(tick_rx) -> _ => {
                crashed_name = None;
            }
            recv(shutdown_rx) -> _ => {
                // Re-check flag at top of loop and break.
                continue;
            }
        }

        // Periodic maintenance (runs on every wake, whether crash or tick)
        {
            let reg = agent::lock_registry(&registry);
            for (name, handle) in reg.iter() {
                if let Ok(mut core) = handle.core.lock() {
                    core.health.maybe_decay();
                    let agent_state = core.state.current;
                    let silent = core.state.last_output.elapsed();
                    if core.health.check_hang(agent_state, silent) {
                        tracing::warn!(
                            agent = %name,
                            state = agent_state.display_name(),
                            silent = ?silent,
                            "hang detected"
                        );
                    }
                }
            }
        }

        // Liveness check for external agents
        {
            let mut ext = crate::agent::lock_external(&externals);
            ext.retain(|name, handle| {
                let alive = crate::process::is_pid_alive(handle.pid);
                if !alive {
                    tracing::info!(agent = %name, pid = handle.pid, "external agent gone, deregistering");
                    crate::event_log::log(home, "disconnect", name, "external agent PID gone");
                }
                alive
            });
        }

        // Periodic snapshot: save fleet state (only if changed)
        {
            let reg = agent::lock_registry(&registry);
            let cfgs = configs.lock().unwrap_or_else(|e| e.into_inner());
            let snapshots: Vec<_> = reg
                .iter()
                .map(|(name, handle)| {
                    let (agent_state, health_state) = handle
                        .core
                        .lock()
                        .map(|c| {
                            (
                                c.state.get_state().display_name().to_string(),
                                c.health.state.display_name().to_string(),
                            )
                        })
                        .unwrap_or_else(|_| ("unknown".into(), "unknown".into()));
                    let cfg = cfgs.get(name);
                    crate::snapshot::AgentSnapshot {
                        name: name.clone(),
                        backend_command: handle.backend_command.clone(),
                        args: cfg.map(|c| c.args.clone()).unwrap_or_default(),
                        working_dir: cfg
                            .and_then(|c| c.working_dir.as_ref())
                            .map(|p| p.display().to_string()),
                        submit_key: handle.submit_key.clone(),
                        health_state,
                        agent_state,
                    }
                })
                .collect();
            drop(cfgs);
            drop(reg);
            // Only write if snapshot content changed
            let new_json = serde_json::to_string(&snapshots).unwrap_or_default();
            if last_snapshot_json != new_json {
                crate::snapshot::save(home, &snapshots);
                last_snapshot_json = new_json;
            }
        }

        check_schedules(home, &registry);
        check_ci_watches(home, &registry);

        {
            let cfgs = configs.lock().unwrap_or_else(|e| e.into_inner());
            for (name, config) in cfgs.iter() {
                if let Some(ref dir) = config.working_dir {
                    if let Some(sid) = crate::backend::read_session_id(dir) {
                        crate::backend::save_session_id(home, name, &sid);
                    }
                }
            }
        }

        // Handle crash event (if any)
        let crashed_name = match crashed_name {
            Some(n) => n,
            None => continue, // Tick only — no crash to handle
        };

        // Ignore crash events during shutdown — agents are being killed intentionally.
        // This prevents spurious respawns and "Agent restarted" system messages.
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::info!(agent = %crashed_name, "ignoring crash during shutdown");
            break;
        }

        tracing::warn!(agent = %crashed_name, "crashed");
        crate::event_log::log(home, "crash", &crashed_name, "agent crashed");

        // Clear stale session ID — if agent crashed with --resume, the session
        // may no longer exist. Removing the .sid file ensures the next respawn
        // starts fresh instead of crash-looping on "No conversation found".
        let sid_file = home.join("sessions").join(format!("{crashed_name}.sid"));
        if sid_file.exists() {
            tracing::info!(agent = %crashed_name, "clearing stale session ID");
            let _ = std::fs::remove_file(&sid_file);
        }

        // Get config for respawn
        let config = configs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&crashed_name)
            .cloned();
        let config = match config {
            Some(c) => c,
            None => {
                tracing::debug!(agent = %crashed_name, "no config for respawn (likely deleted)");
                continue;
            }
        };

        // Record crash in health tracker (unified in AgentCore)
        let (should_respawn, delay, should_notify) = {
            let reg = agent::lock_registry(&registry);
            match reg.get(&crashed_name) {
                Some(handle) => {
                    let mut core = handle.core.lock().unwrap_or_else(|e| e.into_inner());
                    core.health.record_crash()
                }
                None => {
                    tracing::warn!(agent = %crashed_name, "not in registry, skipping");
                    continue;
                }
            }
        };

        if should_notify {
            let state = {
                let reg = agent::lock_registry(&registry);
                reg.get(&crashed_name)
                    .and_then(|h| h.core.lock().ok().map(|c| c.health.state.display_name()))
                    .unwrap_or("unknown")
            };
            tracing::warn!(agent = %crashed_name, %state, "notifying");
            let msg = format!("[health] {crashed_name}: {state}");
            notify_telegram(home, &crashed_name, &msg);
        }

        if should_respawn {
            tracing::info!(agent = %crashed_name, ?delay, "respawning");
            let reg = Arc::clone(&registry);
            let home = home.to_path_buf();
            let tx = crash_tx.clone();
            let shutdown_for_respawn = Arc::clone(&shutdown);

            // Save health tracker from old handle before respawn replaces it
            let saved_health = {
                let r = registry.lock().unwrap_or_else(|e| e.into_inner());
                r.get(&crashed_name)
                    .and_then(|h| h.core.lock().ok().map(|c| c.health.clone()))
            };

            if let Err(e) = std::thread::Builder::new()
                        .name(format!("{crashed_name}_respawn"))
                        .spawn(move || {
                            std::thread::sleep(delay);
                            // Check shutdown flag after backoff — don't respawn during shutdown
                            if shutdown_for_respawn.load(std::sync::atomic::Ordering::Relaxed) {
                                tracing::info!(agent = %config.name, "shutdown during respawn backoff, aborting");
                                return;
                            }
                            let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
                            // After a crash, start fresh — use backend's fresh_args to avoid
                            // crash loops from stale resume (--continue, --resume <id>, etc.)
                            let mut respawn_args: Vec<String> = if let Some(b) = crate::backend::Backend::from_command(&config.backend_command) {
                                let p = b.preset();
                                p.fresh_args.unwrap_or(p.args)
                                    .iter()
                                    .map(|s| s.to_string())
                                    .collect()
                            } else {
                                // Unknown backend: strip common resume flags as best effort
                                strip_resume_args(&config.args)
                            };
                            // Preserve non-resume flags from original args (--mcp-config, --settings, etc.)
                            let preserve = ["--mcp-config", "--settings"];
                            let mut i = 0;
                            while i < config.args.len() {
                                if preserve.contains(&config.args[i].as_str())
                                    && i + 1 < config.args.len()
                                {
                                    respawn_args.push(config.args[i].clone());
                                    respawn_args.push(config.args[i + 1].clone());
                                    i += 2;
                                    continue;
                                }
                                i += 1;
                            }
                            match agent::spawn_agent(
                                &agent::SpawnConfig {
                                    name: &config.name, backend_command: &config.backend_command, args: &respawn_args,
                                    cols, rows,
                                    env: config.env.as_ref(), working_dir: config.working_dir.as_deref(),
                                    submit_key: &config.submit_key, home: Some(&home), crash_tx: Some(tx),
                                    shutdown: Some(Arc::clone(&shutdown_for_respawn)),
                                },
                                &reg,
                            ) {
                                Ok(()) => {
                                    tracing::info!(agent = %config.name, "respawned");
                                    crate::event_log::log(&home, "respawn", &config.name, "agent respawned");

                                    // Restore health tracker from old handle + mark respawn OK
                                    {
                                        let r = reg.lock().unwrap_or_else(|e| e.into_inner());
                                        if let Some(handle) = r.get(&config.name) {
                                            let mut core = handle.core.lock().unwrap_or_else(|e| e.into_inner());
                                            if let Some(ref old_health) = saved_health {
                                                core.health = old_health.clone();
                                            }
                                            core.health.respawn_ok();
                                        }
                                    }

                                    // Inject system message
                                    std::thread::sleep(std::time::Duration::from_secs(2));
                                    {
                                        let r = reg.lock().unwrap_or_else(|e| e.into_inner());
                                        if let Some(handle) = r.get(&config.name) {
                                            let reason = handle.core.lock().ok()
                                                .map(|c| c.health.crash_reason().to_string())
                                                .unwrap_or_else(|| "crash".into());
                                            let msg = format!(
                                                "[system] Agent restarted due to {reason}. Previous context was lost.\r"
                                            );
                                            let _ = agent::write_to_agent(handle, msg.as_bytes());
                                        }
                                    }

                                    // Start TUI socket for respawned agent
                                    let rdir = run_dir(&home);
                                    let n = config.name.clone();
                                    let n_err = n.clone();
                                    let reg2 = Arc::clone(&reg);
                                    if let Err(e) = std::thread::Builder::new()
                                        .name(format!("{n}_tui_server"))
                                        .spawn(move || serve_agent_tui(&n, &rdir, &reg2))
                                    {
                                        tracing::warn!(agent = %n_err, error = %e, "failed to spawn TUI server");
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(agent = %config.name, error = %e, "respawn failed");
                                }
                            }
                        })
                    {
                        tracing::warn!(agent = %crashed_name, error = %e, "failed to spawn respawn thread");
                    }
        } else {
            tracing::warn!(agent = %crashed_name, "max retries exceeded, not respawning");
        }
    }

    // Shutdown: print residual worktrees
    {
        let cfgs = configs.lock().unwrap_or_else(|e| e.into_inner());
        let mut seen = std::collections::HashSet::new();
        for config in cfgs.values() {
            // Use worktree_source (original repo) if available, otherwise working_dir
            let repo = config
                .worktree_source
                .as_ref()
                .or(config.working_dir.as_ref());
            if let Some(dir) = repo {
                if seen.insert(dir.clone()) {
                    let residual = crate::worktree::list_residual(dir);
                    if !residual.is_empty() {
                        tracing::info!(
                            repo = %dir.display(),
                            residual = ?residual,
                            "residual worktrees found (use `git worktree remove` to clean)"
                        );
                    }
                }
            }
        }
    }

    crate::event_log::log(home, "daemon_stop", "", "shutdown");
    tracing::info!("cleaning up...");
    // Drain registry FIRST, then kill. PTY close handlers check the
    // registry — if the agent is gone, they return silently instead of
    // sending crash events. This eliminates all shutdown race conditions.
    let agents_to_kill: Vec<_> = {
        let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        let agents: Vec<_> = reg
            .drain()
            .map(|(name, handle)| (name, handle.child))
            .collect();
        agents
    };
    for (name, child) in &agents_to_kill {
        let mut c = child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = c.kill();
        tracing::info!(agent = %name, "killed");
    }
    // Remove entire run dir (port files + .daemon identity + PID isolation)
    let _ = std::fs::remove_dir_all(run_dir(home));

    // Give threads time to flush logs and close connections
    std::thread::sleep(std::time::Duration::from_secs(1));
    tracing::info!("exiting");
    Ok(())
}

/// Send a notification to Telegram (instance topic or general).
fn notify_telegram(home: &Path, instance_name: &str, text: &str) {
    let config = match crate::fleet::FleetConfig::load(&home.join("fleet.yaml")) {
        Ok(c) => c,
        Err(_) => return,
    };
    let (token, group_id, topic_id) = match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram {
            bot_token_env,
            group_id,
            ..
        }) => match std::env::var(bot_token_env) {
            Ok(t) => (
                t,
                *group_id,
                config.instances.get(instance_name).and_then(|i| i.topic_id),
            ),
            Err(_) => return,
        },
        None => return,
    };

    let text = text.to_string();
    std::thread::Builder::new()
        .name("tg_notify".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            if let Err(_e) = rt.block_on(async {
                let bot = teloxide::Bot::new(&token);
                let chat_id = teloxide::types::ChatId(group_id);
                match topic_id {
                    Some(tid) if tid != 1 => {
                        bot.send_message(chat_id, &text)
                            .message_thread_id(teloxide::types::ThreadId(
                                teloxide::types::MessageId(tid),
                            ))
                            .await?;
                    }
                    _ => {
                        bot.send_message(chat_id, &text).await?;
                    }
                }
                Ok::<(), anyhow::Error>(())
            }) {
                tracing::warn!(error = %_e, "telegram notify failed");
            }
        })
        .ok();
}

/// Check cron schedules and inject messages for due ones.
fn check_schedules(home: &Path, registry: &AgentRegistry) {
    use cron::Schedule;
    use std::str::FromStr;

    let store: serde_json::Value = match std::fs::read_to_string(home.join("schedules.json"))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
    {
        Some(v) => v,
        None => return,
    };
    let schedules = match store["schedules"].as_array() {
        Some(s) => s,
        None => return,
    };

    let now = chrono::Utc::now();
    let last_check_path = home.join(".schedule_last_check");
    let last_check = std::fs::read_to_string(&last_check_path)
        .ok()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| now - chrono::Duration::seconds(10));

    let mut any_triggered = false;
    for sched in schedules {
        if !sched["enabled"].as_bool().unwrap_or(true) {
            continue;
        }
        let cron_expr = match sched["cron"].as_str() {
            Some(c) => c,
            None => continue,
        };
        let full_expr = if cron_expr.split_whitespace().count() == 5 {
            format!("0 {cron_expr}")
        } else {
            cron_expr.to_string()
        };

        let schedule = match Schedule::from_str(&full_expr) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(cron = cron_expr, error = %e, "invalid cron");
                continue;
            }
        };
        if !schedule.after(&last_check).take(1).any(|next| next <= now) {
            continue;
        }

        let (sched_id, target) = (
            sched["id"].as_str().unwrap_or(""),
            sched["target"].as_str().unwrap_or(""),
        );
        let (message, label) = (
            sched["message"].as_str().unwrap_or(""),
            sched["label"].as_str().unwrap_or("(unnamed)"),
        );

        tracing::info!(label, target, message, "schedule triggered");
        crate::event_log::log(
            home,
            "schedule_trigger",
            target,
            &format!("{label}: {message}"),
        );

        let reg = agent::lock_registry(registry);
        let status = if let Some(handle) = reg.get(target) {
            match agent::inject_to_agent(handle, message.as_bytes()) {
                Ok(()) => "ok",
                Err(e) => {
                    tracing::warn!(error = %e, "schedule inject failed");
                    "inject_failed"
                }
            }
        } else {
            let _ = crate::inbox::enqueue(
                home,
                target,
                crate::inbox::InboxMessage {
                    from: "system:schedule".to_string(),
                    text: message.to_string(),
                    kind: Some("schedule".to_string()),
                    timestamp: now.to_rfc3339(),
                },
            );
            "ok_inbox"
        };
        drop(reg);
        if !sched_id.is_empty() {
            crate::schedules::record_run(home, sched_id, status);
        }
        any_triggered = true;
    }

    if any_triggered || now.signed_duration_since(last_check).num_seconds() >= 10 {
        let _ = std::fs::write(&last_check_path, now.to_rfc3339());
    }
}

/// Check CI watch configs and inject failure logs to agents when CI fails.
fn check_ci_watches(home: &Path, registry: &AgentRegistry) {
    let entries = match std::fs::read_dir(home.join("ci-watches")) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let watch: serde_json::Value = match std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
        {
            Some(v) => v,
            None => continue,
        };
        let (repo, instance) = match (watch["repo"].as_str(), watch["instance"].as_str()) {
            (Some(r), Some(i)) => (r.to_string(), i.to_string()),
            _ => continue,
        };
        let branch = watch["branch"].as_str().unwrap_or("main").to_string();
        let interval = watch["interval_secs"].as_u64().unwrap_or(60);
        let last_run_id = watch["last_run_id"].as_u64();

        // Throttle via mtime
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map(|age| age.as_secs() < interval)
                .unwrap_or(false)
            {
                continue;
            }
        }
        let _ = std::fs::write(
            &path,
            serde_json::to_string_pretty(&watch).unwrap_or_default(),
        );

        let home = home.to_path_buf();
        let watch_path = path.clone();
        let registry = Arc::clone(registry);
        std::thread::Builder::new()
            .name("ci_check".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                if let Err(e) = rt.block_on(ci_check_repo(
                    &home,
                    &watch_path,
                    &repo,
                    &branch,
                    &instance,
                    last_run_id,
                    &registry,
                )) {
                    tracing::debug!(repo = %repo, error = %e, "CI check failed");
                }
            })
            .ok();
    }
}

/// Fetch latest GitHub Actions run and inject failure info if new failure detected.
async fn ci_check_repo(
    home: &Path,
    watch_path: &Path,
    repo: &str,
    branch: &str,
    instance: &str,
    last_run_id: Option<u64>,
    registry: &AgentRegistry,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let gh_get = |url: &str| {
        let mut req = client
            .get(url)
            .header("User-Agent", "agend-terminal")
            .header("Accept", "application/vnd.github+json");
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req
    };

    let resp: serde_json::Value = gh_get(&format!(
        "https://api.github.com/repos/{repo}/actions/runs?branch={branch}&per_page=1"
    ))
    .send()
    .await?
    .json()
    .await?;
    let run = match resp["workflow_runs"].as_array().and_then(|a| a.first()) {
        Some(r) => r,
        None => return Ok(()),
    };
    let run_id = run["id"].as_u64().unwrap_or(0);
    if run["conclusion"].as_str() != Some("failure") || Some(run_id) == last_run_id {
        return Ok(());
    }

    let jobs_resp: serde_json::Value = gh_get(&format!(
        "https://api.github.com/repos/{repo}/actions/runs/{run_id}/jobs"
    ))
    .send()
    .await?
    .json()
    .await?;
    let failure_summary = jobs_resp["jobs"]
        .as_array()
        .and_then(|jobs| {
            jobs.iter().find_map(|job| {
                job["steps"].as_array().and_then(|steps| {
                    steps.iter().find_map(|step| {
                        (step["conclusion"].as_str() == Some("failure")).then(|| {
                            format!(
                                "{} / {}",
                                job["name"].as_str().unwrap_or("?"),
                                step["name"].as_str().unwrap_or("?")
                            )
                        })
                    })
                })
            })
        })
        .unwrap_or_else(|| "unknown step".to_string());

    let msg = format!("[ci-fail] {repo} branch {branch}: {failure_summary}\r");
    let reg = agent::lock_registry(registry);
    if let Some(handle) = reg.get(instance) {
        let _ = agent::inject_to_agent(handle, msg.as_bytes());
    } else {
        drop(reg);
        let _ = crate::inbox::enqueue(
            home,
            instance,
            crate::inbox::InboxMessage {
                from: "system:ci".to_string(),
                text: msg,
                kind: Some("ci-fail".to_string()),
                timestamp: chrono::Utc::now().to_rfc3339(),
            },
        );
    }

    // Update last_run_id
    if let Ok(content) = std::fs::read_to_string(watch_path) {
        if let Ok(mut watch) = serde_json::from_str::<serde_json::Value>(&content) {
            watch["last_run_id"] = serde_json::json!(run_id);
            let _ = std::fs::write(
                watch_path,
                serde_json::to_string_pretty(&watch).unwrap_or_default(),
            );
        }
    }
    Ok(())
}

/// Strip resume-related arguments for fresh respawn.
///
/// Handles all backend resume patterns:
/// - `--resume <session-id>` (Claude SavedSession)
/// - `--resume latest` (Gemini Fixed)
/// - `--continue` (OpenCode ContinueInCwd)
/// - `resume --last` (Codex positional subcommand in preset args)
fn strip_resume_args(args: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    let mut skip_next = false;
    let mut skip_codex_resume = false;

    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }

        // --resume <value> or --continue (flag + next arg)
        if arg == "--resume" || arg == "--continue" {
            skip_next = true;
            continue;
        }

        // Codex: "resume" subcommand followed by "--last"
        if arg == "resume" && !arg.starts_with('-') {
            skip_codex_resume = true;
            continue;
        }
        if skip_codex_resume && arg == "--last" {
            skip_codex_resume = false;
            continue;
        }
        skip_codex_resume = false;

        result.push(arg.clone());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-daemon-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn run_dir_contains_pid() {
        let home = tmp_home("run_dir");
        let dir = run_dir(&home);
        let pid = std::process::id().to_string();
        assert!(dir.display().to_string().contains(&pid));
        assert!(dir.ends_with(&pid));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn run_dir_under_home() {
        let home = tmp_home("run_dir_home");
        let dir = run_dir(&home);
        assert!(dir.starts_with(&home));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn find_active_run_dir_no_run_dir() {
        let home = tmp_home("no_run");
        assert!(find_active_run_dir(&home).is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn find_active_run_dir_empty_run_dir() {
        let home = tmp_home("empty_run");
        std::fs::create_dir_all(home.join("run")).ok();
        assert!(find_active_run_dir(&home).is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn find_active_run_dir_stale_pid_cleaned() {
        let home = tmp_home("stale_pid");
        // Use PID 999999 which is very unlikely to be alive
        let stale = home.join("run").join("999999");
        std::fs::create_dir_all(&stale).ok();
        std::fs::write(stale.join(".daemon"), "999999:0").ok();
        assert!(find_active_run_dir(&home).is_none());
        // Stale dir should be cleaned up
        assert!(!stale.exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn find_active_run_dir_current_pid() {
        let home = tmp_home("current_pid");
        let pid = std::process::id();
        let run = home.join("run").join(pid.to_string());
        std::fs::create_dir_all(&run).ok();
        write_daemon_id(&run);
        let found = find_active_run_dir(&home);
        assert!(found.is_some());
        assert_eq!(found.as_deref(), Some(run.as_path()));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn write_daemon_id_format() {
        let home = tmp_home("daemon_id");
        let run = home.join("run").join("test");
        std::fs::create_dir_all(&run).ok();
        write_daemon_id(&run);
        let content = std::fs::read_to_string(run.join(".daemon")).expect("read .daemon");
        let parts: Vec<&str> = content.split(':').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], std::process::id().to_string());
        // Timestamp should be a positive number
        let ts: u64 = parts[1].parse().expect("parse timestamp");
        assert!(ts > 0);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn find_active_run_dir_pid_reuse_detected() {
        let home = tmp_home("pid_reuse");
        let pid = std::process::id();
        let run = home.join("run").join(pid.to_string());
        std::fs::create_dir_all(&run).ok();
        // Write a .daemon file with a DIFFERENT PID (simulates PID reuse)
        std::fs::write(run.join(".daemon"), "12345:0").ok();
        // Should detect PID reuse and clean up
        let found = find_active_run_dir(&home);
        assert!(found.is_none());
        assert!(!run.exists());
        std::fs::remove_dir_all(&home).ok();
    }

    // --- strip_resume_args ---

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn strip_resume_claude() {
        let args = s(&["--dangerously-skip-permissions", "--resume", "abc-123"]);
        let stripped = strip_resume_args(&args);
        assert_eq!(stripped, s(&["--dangerously-skip-permissions"]));
    }

    #[test]
    fn strip_resume_opencode_continue() {
        let args = s(&["--continue"]);
        let stripped = strip_resume_args(&args);
        // --continue + its next arg (if any) stripped; here there's nothing after it
        assert!(stripped.is_empty() || !stripped.contains(&"--continue".to_string()));
    }

    #[test]
    fn strip_resume_gemini() {
        let args = s(&["--yolo", "--resume", "latest"]);
        let stripped = strip_resume_args(&args);
        assert_eq!(stripped, s(&["--yolo"]));
    }

    #[test]
    fn strip_resume_codex() {
        let args = s(&[
            "resume",
            "--last",
            "--dangerously-bypass-approvals-and-sandbox",
        ]);
        let stripped = strip_resume_args(&args);
        assert_eq!(stripped, s(&["--dangerously-bypass-approvals-and-sandbox"]));
    }

    #[test]
    fn strip_resume_no_resume_args() {
        let args = s(&["--dangerously-skip-permissions"]);
        let stripped = strip_resume_args(&args);
        assert_eq!(stripped, args);
    }

    // --- fresh_args ---

    #[test]
    fn codex_fresh_args_drops_resume() {
        let p = crate::backend::Backend::Codex.preset();
        let fresh = p.fresh_args.expect("codex has fresh_args");
        assert!(!fresh.contains(&"resume"));
        assert!(!fresh.contains(&"--last"));
        assert!(fresh.contains(&"--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn claude_fresh_args_same_as_preset() {
        let p = crate::backend::Backend::ClaudeCode.preset();
        assert!(p.fresh_args.is_none());
    }

    #[test]
    fn opencode_fresh_args_same_as_preset() {
        let p = crate::backend::Backend::OpenCode.preset();
        assert!(p.fresh_args.is_none());
    }
}
