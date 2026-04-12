//! Daemon: manages agent registry, TUI sockets, auto-respawn, and fleet lifecycle.

use crate::agent::{self, AgentRegistry};
use crate::framing::{self, TAG_DATA, TAG_RESIZE};
use teloxide::prelude::Requester;
use teloxide::payloads::SendMessageSetters;

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
    /// Original repo root (before worktree redirect).
    worktree_source: Option<PathBuf>,
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
        if let Err(e) = std::thread::Builder::new()
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
        {
            eprintln!("[{n}] failed to spawn TUI output thread: {e}");
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
        {
            eprintln!("[{n_err}] failed to spawn TUI input thread: {e}");
        }
    }
}

/// Get the PID-isolated run directory for the current daemon.
pub fn run_dir(home: &Path) -> PathBuf {
    home.join("run").join(std::process::id().to_string())
}

pub fn agent_socket_path(home: &Path, name: &str) -> String {
    let dir = find_active_run_dir(home).unwrap_or_else(|| run_dir(home));
    dir.join(format!("{name}.sock")).display().to_string()
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
            let alive = unsafe { nix::libc::kill(pid as i32, 0) == 0 };
            if !alive {
                eprintln!("[daemon] cleaning stale run dir: {}", entry.path().display());
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
                    eprintln!("[daemon] PID {pid} reused (was {}), cleaning", file_pid);
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

/// Start daemon: spawn agents, handle crashes with auto-respawn.
pub fn run(
    home: &Path,
    agents: Vec<(String, String, Vec<String>, Option<HashMap<String, String>>, Option<PathBuf>, String)>,
) -> anyhow::Result<()> {
    // Check for existing daemon
    if let Some(existing) = find_active_run_dir(home) {
        anyhow::bail!("Another daemon is already running ({})", existing.display());
    }

    // Create PID-isolated run directory with identity file
    let run = run_dir(home);
    std::fs::create_dir_all(&run)?;
    write_daemon_id(&run);
    eprintln!("[daemon] run dir: {}", run.display());

    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));

    // Crash channel for auto-respawn
    let (crash_tx, crash_rx) = crossbeam::channel::unbounded::<String>();

    // Store configs for respawn
    let configs: Arc<Mutex<HashMap<String, AgentConfig>>> = Arc::new(Mutex::new(HashMap::new()));

    // Shutdown flag — shared with agent reapers to distinguish crash from shutdown
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    eprintln!("[daemon] starting {} agent(s)", agents.len());

    for (name, command, args, env, working_dir, submit_key) in &agents {
        // Derive worktree_source: if working_dir contains .worktrees/, the repo root is 2 levels up
        let worktree_source = working_dir.as_ref().and_then(|wd| {
            let wd_str = wd.display().to_string();
            if wd_str.contains(".worktrees/") {
                wd.parent().and_then(|p| p.parent()).map(|p| p.to_path_buf())
            } else {
                None
            }
        });
        let config = AgentConfig {
            name: name.clone(),
            command: command.clone(),
            args: args.clone(),
            env: env.clone(),
            working_dir: working_dir.clone(),
            worktree_source,
            submit_key: submit_key.clone(),
        };
        configs.lock().unwrap_or_else(|e| e.into_inner()).insert(name.clone(), config);

        let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
        agent::spawn_agent(
            name, command, args, cols, rows,
            env.as_ref(), working_dir.as_deref(), submit_key,
            &registry, Some(home), Some(crash_tx.clone()),
            Some(Arc::clone(&shutdown)),
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

    crate::event_log::log(home, "daemon_start", "", &format!("{} agents", agents.len()));
    eprintln!("[daemon] running. Ctrl+C or `agend-terminal stop` to stop.");

    // Periodic tick channel (every 10s for health/schedule/session maintenance)
    let tick_rx = {
        let (tx, rx) = crossbeam::channel::bounded(1);
        std::thread::Builder::new()
            .name("daemon_tick".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(10));
                    if tx.send(()).is_err() {
                        break;
                    }
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

        // Block until either a crash event or periodic tick
        let crashed_name: Option<String>;
        crossbeam::select! {
            recv(crash_rx) -> msg => {
                crashed_name = msg.ok();
            }
            recv(tick_rx) -> _ => {
                crashed_name = None;
            }
        }

        // Periodic maintenance (runs on every wake, whether crash or tick)
        {
            let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
            for (name, handle) in reg.iter() {
                if let Ok(mut core) = handle.core.lock() {
                    core.health.maybe_decay();
                    let agent_state = core.state.current;
                    let last_output = core.state.last_output;
                    if core.health.check_hang(agent_state, last_output) {
                        eprintln!("[health] {name}: hang detected (state={}, silent={:?})",
                            agent_state.display_name(), last_output.elapsed());
                    }
                }
            }
        }

        check_schedules(home, &registry);

        {
            let cfgs = configs.lock().unwrap_or_else(|e| e.into_inner());
            for (name, config) in cfgs.iter() {
                if let Some(ref dir) = config.working_dir {
                    let statusline = dir.join("statusline.json");
                    if statusline.exists() {
                        if let Ok(content) = std::fs::read_to_string(&statusline) {
                            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) {
                                if let Some(sid) = data.get("session_id").and_then(|v| v.as_str()) {
                                    crate::backend::save_session_id(home, name, sid);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Handle crash event (if any)
        let crashed_name = match crashed_name {
            Some(n) => n,
            None => continue, // Tick only — no crash to handle
        };

        eprintln!("[health] {crashed_name} crashed");
        crate::event_log::log(home, "crash", &crashed_name, "agent crashed");

                // Get config for respawn
                let config = configs.lock().unwrap_or_else(|e| e.into_inner()).get(&crashed_name).cloned();
                let config = match config {
                    Some(c) => c,
                    None => {
                        eprintln!("[health] {crashed_name}: no config for respawn");
                        continue;
                    }
                };

                // Record crash in health tracker (unified in AgentCore)
                let (should_respawn, delay, should_notify) = {
                    let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
                    match reg.get(&crashed_name) {
                        Some(handle) => {
                            let mut core = handle.core.lock().unwrap_or_else(|e| e.into_inner());
                            core.health.record_crash()
                        }
                        None => {
                            eprintln!("[health] {crashed_name}: not in registry, skipping");
                            continue;
                        }
                    }
                };

                if should_notify {
                    let state = {
                        let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
                        reg.get(&crashed_name)
                            .and_then(|h| h.core.lock().ok().map(|c| c.health.state.display_name()))
                            .unwrap_or("unknown")
                    };
                    eprintln!("[health] {crashed_name}: {state} — notifying");
                    let msg = format!("[health] {crashed_name}: {state}");
                    notify_telegram(home, &crashed_name, &msg);
                }

                if should_respawn {
                    eprintln!("[health] {crashed_name}: respawning in {:?}", delay);
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
                                eprintln!("[health] {}: shutdown during respawn backoff, aborting", config.name);
                                return;
                            }
                            let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
                            match agent::spawn_agent(
                                &config.name, &config.command, &config.args,
                                cols, rows,
                                config.env.as_ref(), config.working_dir.as_deref(),
                                &config.submit_key, &reg, Some(&home), Some(tx),
                                Some(Arc::clone(&shutdown_for_respawn)),
                            ) {
                                Ok(()) => {
                                    eprintln!("[health] {}: respawned", config.name);
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
                                    let sock = agent_socket_path(&home, &config.name);
                                    let n = config.name.clone();
                                    let n_err = n.clone();
                                    let reg2 = Arc::clone(&reg);
                                    if let Err(e) = std::thread::Builder::new()
                                        .name(format!("{n}_tui_server"))
                                        .spawn(move || serve_agent_tui(&n, &sock, &reg2))
                                    {
                                        eprintln!("[{n_err}] failed to spawn TUI server: {e}");
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[health] {}: respawn failed: {e}", config.name);
                                }
                            }
                        })
                    {
                        eprintln!("[health] {crashed_name}: failed to spawn respawn thread: {e}");
                    }
                } else {
                    eprintln!("[health] {crashed_name}: max retries exceeded, not respawning");
                }
    }

    // Shutdown: print residual worktrees
    {
        let cfgs = configs.lock().unwrap_or_else(|e| e.into_inner());
        let mut seen = std::collections::HashSet::new();
        for config in cfgs.values() {
            // Use worktree_source (original repo) if available, otherwise working_dir
            let repo = config.worktree_source.as_ref().or(config.working_dir.as_ref());
            if let Some(dir) = repo {
                if seen.insert(dir.clone()) {
                    let residual = crate::worktree::list_residual(dir);
                    if !residual.is_empty() {
                        eprintln!("[worktree] residual in {}: {:?} (use `git worktree remove` to clean)", dir.display(), residual);
                    }
                }
            }
        }
    }

    crate::event_log::log(home, "daemon_stop", "", "shutdown");
    eprintln!("[daemon] cleaning up...");
    {
        let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        for (name, agent) in reg.iter() {
            let mut child = agent.child.lock().unwrap_or_else(|e| e.into_inner());
            let _ = child.kill();
            eprintln!("[daemon] killed {name}");
        }
    }
    // Remove entire run dir (sockets + PID isolation)
    let _ = std::fs::remove_dir_all(run_dir(home));

    {
        let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        reg.clear();
    }

    std::thread::sleep(std::time::Duration::from_millis(500));
    eprintln!("[daemon] exiting.");
    std::process::exit(0);
}

/// Send a notification to Telegram (instance topic or general).
fn notify_telegram(home: &Path, instance_name: &str, text: &str) {
    let fleet_path = home.join("fleet.yaml");
    if !fleet_path.exists() {
        return;
    }
    let config = match crate::fleet::FleetConfig::load(&fleet_path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let (token, group_id, topic_id) = match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram { bot_token_env, group_id, .. }) => {
            let token = match std::env::var(bot_token_env) {
                Ok(t) => t,
                Err(_) => return,
            };
            let topic_id = config.instances.get(instance_name)
                .and_then(|inst| inst.topic_id);
            (token, *group_id, topic_id)
        }
        None => return,
    };

    let text = text.to_string();
    // Fire-and-forget in a thread to avoid blocking daemon loop
    std::thread::Builder::new()
        .name("tg_notify".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };
            if let Err(e) = rt.block_on(async {
                let bot = teloxide::Bot::new(&token);
                let chat_id = teloxide::types::ChatId(group_id);
                if let Some(tid) = topic_id {
                    if tid == 1 {
                        bot.send_message(chat_id, &text).await?;
                    } else {
                        bot.send_message(chat_id, &text)
                            .message_thread_id(teloxide::types::ThreadId(teloxide::types::MessageId(tid)))
                            .await?;
                    }
                } else {
                    // No topic — send to general
                    bot.send_message(chat_id, &text).await?;
                }
                Ok::<(), anyhow::Error>(())
            }) {
                eprintln!("[telegram] notify failed: {e}");
            }
        })
        .ok();
}

/// Check cron schedules and inject messages for due ones.
fn check_schedules(home: &Path, registry: &AgentRegistry) {
    use cron::Schedule;
    use std::str::FromStr;

    let store_path = home.join("schedules.json");
    if !store_path.exists() {
        return;
    }

    let content = match std::fs::read_to_string(&store_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let store: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return,
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
        let enabled = sched["enabled"].as_bool().unwrap_or(true);
        if !enabled {
            continue;
        }

        let cron_expr = match sched["cron"].as_str() {
            Some(c) => c,
            None => continue,
        };

        // cron crate needs 6-field format (sec min hour dom month dow)
        // User provides 5-field (min hour dom month dow), prepend "0 " for seconds
        let full_expr = if cron_expr.split_whitespace().count() == 5 {
            format!("0 {cron_expr}")
        } else {
            cron_expr.to_string()
        };

        let schedule = match Schedule::from_str(&full_expr) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[schedule] invalid cron '{}': {e}", cron_expr);
                continue;
            }
        };

        // Check if there's a trigger between last_check and now
        let should_trigger = schedule.after(&last_check)
            .take(1)
            .any(|next| next <= now);

        if should_trigger {
            let sched_id = sched["id"].as_str().unwrap_or("");
            let target = sched["target"].as_str().unwrap_or("");
            let message = sched["message"].as_str().unwrap_or("");
            let label = sched["label"].as_str().unwrap_or("(unnamed)");

            eprintln!("[schedule] triggering '{label}' → {target}: {message}");
            crate::event_log::log(home, "schedule_trigger", target, &format!("{label}: {message}"));

            // Inject message to target agent via registry
            let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
            let status = if let Some(handle) = reg.get(target) {
                match agent::inject_to_agent(handle, message.as_bytes()) {
                    Ok(()) => "ok",
                    Err(e) => {
                        eprintln!("[schedule] inject failed: {e}");
                        "inject_failed"
                    }
                }
            } else {
                // Fallback: enqueue to inbox
                let _ = crate::inbox::enqueue(home, target, crate::inbox::InboxMessage {
                    from: "system:schedule".to_string(),
                    text: message.to_string(),
                    kind: Some("schedule".to_string()),
                    timestamp: now.to_rfc3339(),
                });
                "ok_inbox"
            };
            drop(reg);

            // Record run history
            if !sched_id.is_empty() {
                crate::schedules::record_run(home, sched_id, status);
            }
            any_triggered = true;
        }
    }

    if any_triggered || now.signed_duration_since(last_check).num_seconds() >= 10 {
        let _ = std::fs::write(&last_check_path, now.to_rfc3339());
    }
}
