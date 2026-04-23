//! Daemon: manages agent registry, TUI sockets, auto-respawn, fleet lifecycle,
//! schedule checking, health monitoring, Telegram notifications.

pub(crate) mod ci_watch;
pub(crate) mod cron_tick;
pub(crate) mod supervisor;
mod tui_bridge;

use crate::agent::{self, AgentRegistry};
use crate::channel::telegram::notify_telegram;
use ci_watch::check_ci_watches;
use cron_tick::check_schedules;
pub use tui_bridge::serve_agent_tui;

use std::collections::HashMap;
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

/// Remove every `~/.agend/run/<pid>/` whose daemon is not reachable.
///
/// `find_active_run_dir` cleans only the one entry it visits before returning
/// the first alive-PID match, so a second (or third) stale dir whose PID has
/// been recycled by an unrelated OS process survives indefinitely. On the next
/// `agend-terminal app` launch the bootstrap probe might pick any of them; the
/// losers stay on disk and keep accumulating. This runs once at the winning
/// daemon's startup (after the exclusive lock) and clears the backlog.
///
/// An entry survives only if BOTH `is_pid_alive` returns true AND `probe_api`
/// can reach its `api.port`. Missing/malformed `.daemon` or `api.port` counts
/// as stale.
pub fn sweep_stale_run_dirs(home: &Path) {
    let run = home.join("run");
    let Ok(entries) = std::fs::read_dir(&run) else {
        return;
    };
    for entry in entries.flatten() {
        let pid_str = entry.file_name().to_string_lossy().into_owned();
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        let alive = crate::process::is_pid_alive(pid) && crate::ipc::probe_api(&entry.path());
        if !alive {
            tracing::info!(
                path = %entry.path().display(),
                "sweeping stale run dir"
            );
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Write daemon identity file for PID reuse detection.
pub(crate) fn write_daemon_id(run_dir: &Path) {
    let pid = std::process::id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::write(run_dir.join(".daemon"), format!("{pid}:{now}"));
}

/// Read the PID recorded in `{run_dir}/.daemon`. Returns `None` if the file is
/// missing or malformed — callers should treat that as "unknown PID".
pub(crate) fn read_daemon_pid(run_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(run_dir.join(".daemon"))
        .ok()?
        .trim()
        .split_once(':')
        .and_then(|(pid, _)| pid.parse().ok())
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

/// Start daemon: do preflight (lock, run dir, cookie) then run the core loop.
///
/// Used by the `Commands::Daemon { agents }` escape hatch path (no fleet.yaml).
/// The fleet-driven path uses [`run_with_prepared`] instead, which skips the
/// preflight because [`crate::bootstrap::prepare`] has already done it.
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
    // P1-10: issue the connection cookie *before* spawning any TUI / API
    // server thread, since `serve_agent_tui` and `api::serve` both expect
    // `api.cookie` to already exist. Failure here aborts startup —
    // running the control plane without auth would be a silent security
    // regression.
    crate::auth_cookie::issue(&run)
        .map_err(|e| anyhow::anyhow!("failed to issue API auth cookie: {e}"))?;
    tracing::info!(path = %run.display(), "run dir");

    // Extract embedded fleet protocol to AGEND_HOME/protocol/.default/
    crate::protocol::extract_default(home);

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

    run_core(home, agents, None, None)
}

/// Start daemon with a fleet already prepared by [`crate::bootstrap::prepare`].
///
/// Skips the preflight (lock, run dir, cookie issuance, fleet load/normalize,
/// telegram init) since bootstrap already performed those. The `OwnedFleet`
/// is held for the full call so the flock guard, cookie bytes, and Telegram
/// state stay alive for the daemon's lifetime.
pub fn run_with_prepared(mut prepared: Box<crate::bootstrap::OwnedFleet>) -> anyhow::Result<()> {
    tracing::info!(path = %prepared.run_dir.display(), "run dir");
    // Move the agent vec out without cloning (~N×String+Vec+HashMap). `home`
    // is a short PathBuf — cheap to clone. Keep `prepared` alive through the
    // scope so flock / cookie / telegram / config persist for the full run.
    let home = prepared.home.clone();
    let agents = std::mem::take(&mut prepared.agents);
    // Reuse the already-parsed FleetConfig for the initial reload digest;
    // avoids re-reading + re-parsing fleet.yaml inside run_core.
    let initial_digest = crate::bootstrap::reload::digest_from_config(&prepared.config);
    let telegram = prepared.telegram.clone();
    // Reconcile fleet.yaml teams: section → teams.json (additive only)
    crate::teams::reconcile_teams(&home, &prepared.config);
    let _owned = prepared;
    run_core(&home, agents, Some(initial_digest), telegram)
}

fn run_core(
    home: &Path,
    agents: Vec<AgentDef>,
    initial_digest: Option<HashMap<String, crate::bootstrap::reload::InstanceDigest>>,
    telegram: Option<Arc<dyn crate::channel::Channel>>,
) -> anyhow::Result<()> {
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    if let Some(tg) = telegram.as_ref() {
        tg.attach_registry(Arc::clone(&registry));
    }

    // External agents registry (connected via `agend-terminal connect`)
    let externals: crate::agent::ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));

    // Crash channel for auto-respawn.
    //
    // Bounded to prevent the reaper from accumulating unbounded crash events
    // if the main loop stalls (P2-2, review 2026-04-18). 64 is comfortably
    // more than a plausible burst — every fleet member crashing at once —
    // and senders use `try_send` so a full channel drops the event with a
    // warning rather than blocking the PTY close handler.
    let (crash_tx, crash_rx) = crossbeam::channel::bounded::<String>(64);

    // Store configs for respawn
    let configs: Arc<Mutex<HashMap<String, AgentConfig>>> = Arc::new(Mutex::new(HashMap::new()));

    // Shutdown flag — shared with agent reapers to distinguish crash from shutdown
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    tracing::info!(count = agents.len(), "starting agents");

    for def in &agents {
        spawn_and_register_agent(home, def, &registry, &configs, &crash_tx, &shutdown)?;
        if agents.len() > 1 {
            std::thread::sleep(spawn_stagger());
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

    // Shutdown wake channel — signal handler sends on this so the main loop's
    // select! wakes immediately instead of waiting up to 10s for the next tick.
    let (shutdown_tx, shutdown_rx) = crossbeam::channel::bounded::<()>(1);
    crate::bootstrap::signals::install(Arc::clone(&shutdown), shutdown_tx);

    crate::event_log::log(
        home,
        "daemon_start",
        "",
        &format!("{} agents", agents.len()),
    );
    tracing::info!("running, Ctrl+C or `agend-terminal stop` to stop");

    // Ready ping → agend-supervisor (if we were started under one, i.e.
    // `AGEND_SUPERVISOR_SOCK` env is set). Must come after agents are up
    // and API is bound — supervisor treats this as "upgrade succeeded".
    // Fire-and-forget: supervisor crashes shouldn't stop the daemon.
    // Unix-only (the supervisor itself is Unix-only).
    #[cfg(unix)]
    {
        let pid = std::process::id();
        let version = env!("CARGO_PKG_VERSION");
        if let Err(e) = agend_terminal::supervisor::client::notify_ready(pid, version) {
            tracing::warn!(error = %e, "supervisor ready ping failed (continuing)");
        }
    }

    // If the supervisor just upgraded us, it dropped an upgrade-marker with
    // from/to versions. Consume it asynchronously: wait for agent prompts to
    // settle, inject a single "daemon upgraded" notice into each, then delete
    // the marker. Spawned so we don't delay the main loop; errors are logged
    // and otherwise ignored (agents missing the message is a cosmetic loss).
    #[cfg(unix)]
    consume_upgrade_marker(home.to_path_buf(), Arc::clone(&registry));

    supervisor::spawn(home.to_path_buf(), Arc::clone(&registry));

    // Recover any half-written inbox files from a previous crash.
    crate::inbox::recover_half_writes(home);

    // Replay missed one-shot schedules from before daemon was down.
    // Must run once at startup, before the tick loop, so missed one-shots
    // fire exactly once. The tick loop's check_schedules handles future
    // one-shots and recurring crons.
    replay_missed_at_startup(home, &registry);

    let mut last_snapshot_json = String::new();

    // Hot-reload watcher: poll fleet.yaml mtime on each tick. `known_digest`
    // starts from the initial fleet (so a reload detects only real changes,
    // not startup state) and advances as added agents materialize. Removed /
    // command-changed / args-changed / working_dir-changed are warn-only; see
    // `bootstrap::reload` docs for policy.
    let fleet_path = home.join("fleet.yaml");
    let mut fleet_watcher = crate::bootstrap::reload::FleetWatcher::new(fleet_path.clone());
    let mut known_digest = initial_digest.unwrap_or_else(|| {
        crate::fleet::FleetConfig::load(&fleet_path)
            .ok()
            .map(|c| crate::bootstrap::reload::digest_from_config(&c))
            .unwrap_or_default()
    });

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

    // Watchdog dry-run mode: log classifications without mutating health state.
    let watchdog_dry_run = std::env::var("AGEND_WATCHDOG_DRY_RUN")
        .map(|v| matches!(v.as_str(), "1" | "true"))
        .unwrap_or(false);

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

        // Periodic maintenance (runs on every wake, whether crash or tick).
        // AwaitingOperator detection lives in the dedicated supervisor thread
        // (spawned earlier) so app mode gets the same behavior as daemon mode.
        // Hang detection and health decay stay here — they're daemon-only
        // concerns (hang notifications tie into crash respawn accounting).
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

        // Watchdog: classify PTY output → BlockedReason
        {
            let reg = agent::lock_registry(&registry);
            for (name, handle) in reg.iter() {
                let backend = match crate::backend::Backend::from_command(&handle.backend_command) {
                    Some(b) => b,
                    None => continue,
                };
                if let Ok(mut core) = handle.core.lock() {
                    let rows = core.vterm.rows() as usize;
                    let screen = core.vterm.tail_lines(rows);
                    if let Some(reason) = crate::state::classify_pty_output(&backend, &screen) {
                        if watchdog_dry_run {
                            crate::event_log::log(
                                home,
                                "watchdog_dry_run",
                                name,
                                &format!("{reason:?}"),
                            );
                        } else {
                            core.health.set_blocked_reason(reason);
                        }
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
            let cfgs = crate::sync::lock_poisoned(&configs, "configs");
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

        // Periodic inbox maintenance — every 60 ticks (≈10 min at 10s/tick)
        {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SWEEP_COUNTER: AtomicU64 = AtomicU64::new(0);
            if SWEEP_COUNTER
                .fetch_add(1, Ordering::Relaxed)
                .is_multiple_of(60)
            {
                crate::inbox::sweep_expired(home);
                crate::inbox::check_disk_space(home);
            }
        }

        // Hot-reload: poll fleet.yaml; spawn newly-added instances, warn on
        // changes we can't safely apply in-flight.
        if let Some(new_cfg) = fleet_watcher.check() {
            apply_fleet_reload(
                home,
                &new_cfg,
                &mut known_digest,
                &registry,
                &configs,
                &crash_tx,
                &shutdown,
            );
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

        // Get config for respawn
        let config = crate::sync::lock_poisoned(&configs, "configs")
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
                    let mut core = crate::sync::lock_poisoned(&handle.core, "agent_core");
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
                let r = crate::sync::lock_poisoned(&registry, "registry");
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
                            // Respawn fresh: stale --resume after a crash tends
                            // to loop on "conversation not found".
                            match agent::spawn_agent(
                                &agent::SpawnConfig {
                                    name: &config.name,
                                    backend_command: &config.backend_command,
                                    args: &config.args,
                                    spawn_mode: crate::backend::SpawnMode::Fresh,
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
                                        let r = crate::sync::lock_poisoned(&reg, "registry");
                                        if let Some(handle) = r.get(&config.name) {
                                            let mut core = crate::sync::lock_poisoned(&handle.core, "agent_core");
                                            if let Some(ref old_health) = saved_health {
                                                core.health = old_health.clone();
                                            }
                                            core.health.respawn_ok();
                                        }
                                    }

                                    // Inject system message
                                    std::thread::sleep(std::time::Duration::from_secs(2));
                                    {
                                        let r = crate::sync::lock_poisoned(&reg, "registry");
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
        let cfgs = crate::sync::lock_poisoned(&configs, "configs");
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
        let mut reg = crate::sync::lock_poisoned(&registry, "registry");
        let agents: Vec<_> = reg
            .drain()
            .map(|(name, handle)| (name, handle.child))
            .collect();
        agents
    };
    for (name, child) in &agents_to_kill {
        let mut c = crate::sync::lock_poisoned(child, "child_proc");
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

/// Apply a newly-loaded fleet.yaml to the running daemon.
///
/// Policy:
/// - **added**: resolve via `bootstrap::resolve_one` and spawn in-place. The
///   new agent gets the same `AgentConfig` registration (for respawn) and
///   per-agent TUI server as any startup agent.
/// - **removed / command_changed / args_changed / working_dir_changed**: log
///   a warn. Tearing down a live PTY to realign fleet.yaml risks destroying
///   in-progress user work, so the operator has to explicitly `delete` /
///   `restart` the agent.
/// - **role_changed / topic_id_changed**: log. These fields only matter at
///   spawn time for instructions generation and Telegram routing; a safe
///   in-place swap needs more plumbing (instructions files are read by the
///   agent process, routing tables are cached elsewhere).
///
/// `known_digest` is advanced to the new fleet's digest on successful apply
/// so the next tick's diff is computed relative to the just-applied state.
#[allow(clippy::too_many_arguments)]
fn apply_fleet_reload(
    home: &Path,
    new_config: &crate::fleet::FleetConfig,
    known_digest: &mut HashMap<String, crate::bootstrap::reload::InstanceDigest>,
    registry: &AgentRegistry,
    configs: &Arc<Mutex<HashMap<String, AgentConfig>>>,
    crash_tx: &crossbeam::channel::Sender<String>,
    shutdown: &Arc<std::sync::atomic::AtomicBool>,
) {
    let new_digest = crate::bootstrap::reload::digest_from_config(new_config);
    let diff = crate::bootstrap::reload::compute_diff(known_digest, &new_digest);
    if diff.is_empty() {
        return;
    }

    tracing::info!(
        added = ?diff.added,
        removed = ?diff.removed,
        command_changed = ?diff.command_changed,
        args_changed = ?diff.args_changed,
        role_changed = ?diff.role_changed,
        topic_id_changed = ?diff.topic_id_changed,
        working_dir_changed = ?diff.working_dir_changed,
        "fleet.yaml reload",
    );

    for name in &diff.removed {
        tracing::warn!(agent = %name, "fleet.yaml removed agent — left running; use `delete` to tear down");
    }
    for name in &diff.command_changed {
        tracing::warn!(agent = %name, "fleet.yaml command changed — requires manual restart (would kill live session)");
    }
    for name in &diff.args_changed {
        tracing::warn!(agent = %name, "fleet.yaml args changed — requires manual restart");
    }
    for name in &diff.working_dir_changed {
        tracing::warn!(agent = %name, "fleet.yaml working_directory changed — requires manual restart");
    }
    for name in &diff.role_changed {
        tracing::info!(agent = %name, "fleet.yaml role changed — won't be reflected until agent respawns");
    }
    for name in &diff.topic_id_changed {
        tracing::info!(agent = %name, "fleet.yaml topic_id changed — won't be reflected until agent respawns");
    }

    let added_count = diff.added.len();
    for (idx, name) in diff.added.iter().enumerate() {
        let Some(agent_def) = crate::bootstrap::resolve_one(new_config, name) else {
            tracing::warn!(agent = %name, "failed to resolve newly-added instance");
            continue;
        };
        let added_name = agent_def.0.clone();
        if let Err(e) =
            spawn_and_register_agent(home, &agent_def, registry, configs, crash_tx, shutdown)
        {
            tracing::warn!(agent = %added_name, error = %e, "failed to spawn reload-added agent");
            continue;
        }
        crate::event_log::log(
            home,
            "reload_add",
            &added_name,
            "agent added via fleet.yaml reload",
        );
        tracing::info!(agent = %added_name, "spawned via fleet.yaml reload");
        if added_count > 1 && idx + 1 < added_count {
            std::thread::sleep(spawn_stagger());
        }
    }

    *known_digest = new_digest;
}

/// Replay missed one-shot schedules on daemon startup.
/// Calls `schedules::replay_missed_oneshots` and fires each returned
/// schedule through the same path as `cron_tick::check_schedules`.
fn replay_missed_at_startup(home: &Path, registry: &AgentRegistry) {
    let missed = crate::schedules::replay_missed_oneshots(home);
    if missed.is_empty() {
        return;
    }
    tracing::info!(count = missed.len(), "replaying missed one-shot schedules");
    let now = chrono::Utc::now();
    for sched in &missed {
        let target = sched.target.as_str();
        let message = sched.message.as_str();
        let label = sched.label.as_deref().unwrap_or("(unnamed)");

        tracing::info!(label, target, message, "replaying missed one-shot");
        crate::event_log::log(
            home,
            "schedule_replay",
            target,
            &format!("{label}: {message}"),
        );

        let reg = agent::lock_registry(registry);
        if let Some(handle) = reg.get(target) {
            if let Err(e) = agent::inject_to_agent(handle, message.as_bytes()) {
                tracing::warn!(error = %e, "replay inject failed");
            }
        } else {
            drop(reg);
            let _ = crate::inbox::enqueue(
                home,
                target,
                crate::inbox::InboxMessage {
                    schema_version: 0,
                    id: None,
                    from: "system:schedule".to_string(),
                    text: message.to_string(),
                    kind: Some("schedule_replay".to_string()),
                    timestamp: now.to_rfc3339(),
                    read_at: None,
                },
            );
        }
    }
}

/// Staggered-spawn delay — rate-limits PTY init during multi-agent bursts
/// (startup or hot-reload). Tunable via `AGEND_SPAWN_STAGGER_MS`.
fn spawn_stagger() -> std::time::Duration {
    let ms: u64 = std::env::var("AGEND_SPAWN_STAGGER_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    std::time::Duration::from_millis(ms)
}

/// Shared "spawn one agent + register respawn config + start per-agent TUI
/// server" path. Used by startup (run_core), hot-reload (apply_fleet_reload),
/// and any future add-agent call site. Rolls back the `configs` entry on
/// spawn failure so retries start clean.
fn spawn_and_register_agent(
    home: &Path,
    def: &crate::bootstrap::AgentDef,
    registry: &AgentRegistry,
    configs: &Arc<Mutex<HashMap<String, AgentConfig>>>,
    crash_tx: &crossbeam::channel::Sender<String>,
    shutdown: &Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<()> {
    let (name, command, args, env, working_dir, submit_key) = def;
    let worktree_source = working_dir
        .as_ref()
        .and_then(|wd| crate::worktree::source_repo_of(wd));
    crate::sync::lock_poisoned(configs, "configs").insert(
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
    if let Err(e) = agent::spawn_agent(
        &agent::SpawnConfig {
            name,
            backend_command: command,
            args,
            spawn_mode: crate::backend::SpawnMode::Resume,
            cols,
            rows,
            env: env.as_ref(),
            working_dir: working_dir.as_deref(),
            submit_key,
            home: Some(home),
            crash_tx: Some(crash_tx.clone()),
            shutdown: Some(Arc::clone(shutdown)),
        },
        registry,
    ) {
        crate::sync::lock_poisoned(configs, "configs").remove(name);
        return Err(e);
    }

    let rdir = run_dir(home);
    let reg = Arc::clone(registry);
    let n = name.clone();
    std::thread::Builder::new()
        .name(format!("{n}_tui_server"))
        .spawn(move || serve_agent_tui(&n, &rdir, &reg))?;
    Ok(())
}

/// If `$AGEND_HOME/run/upgrade-marker` exists, we were just (re)launched by
/// the supervisor as part of a hot-upgrade. Wait briefly for agents to be
/// ready to accept input, inject a single "daemon upgraded" notice into each
/// PTY, then delete the marker so a subsequent crash-respawn won't repeat it.
///
/// The marker format is the JSON blob supervisor writes in `write_upgrade_marker`:
/// `{ "from_version": "...", "to_version": "...", "new_hash": "...", "prev_hash": "...", "at": "..." }`.
///
/// Failures are logged but never propagated — this is a cosmetic notice;
/// missing it must not abort daemon startup.
///
/// Unix-only: consumes output of `agend_terminal::supervisor::paths::upgrade_marker`,
/// which is itself gated behind `#[cfg(unix)]` in `src/lib.rs`.
#[cfg(unix)]
fn consume_upgrade_marker(home: PathBuf, registry: AgentRegistry) {
    let marker_path = agend_terminal::supervisor::paths::upgrade_marker(&home);
    if !marker_path.exists() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("upgrade_marker".into())
        .spawn(move || {
            let raw = match std::fs::read_to_string(&marker_path) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(path = %marker_path.display(), error = %e, "read upgrade-marker failed");
                    let _ = std::fs::remove_file(&marker_path);
                    return;
                }
            };
            let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
            let from = parsed
                .get("from_version")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let to = parsed
                .get("to_version")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let msg = format!(
                "[system] Daemon upgraded from {from} to {to}. All agents restarted.\r"
            );

            // Give agents a moment to paint their prompts; matches the delay
            // used by the crash-respawn system message injection above.
            std::thread::sleep(std::time::Duration::from_secs(2));
            let r = crate::sync::lock_poisoned(&registry, "registry");
            for handle in r.values() {
                let _ = agent::write_to_agent(handle, msg.as_bytes());
            }
            drop(r);
            if let Err(e) = std::fs::remove_file(&marker_path) {
                tracing::warn!(
                    path = %marker_path.display(),
                    error = %e,
                    "remove upgrade-marker failed (daemon will keep running)"
                );
            }
            tracing::info!(from, to, "upgrade-marker consumed, agents notified");
        });
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
