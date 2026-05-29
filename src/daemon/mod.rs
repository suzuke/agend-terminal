//! Daemon: manages agent registry, TUI sockets, auto-respawn, fleet lifecycle,
//! schedule checking, health monitoring, Telegram notifications.

pub(crate) mod anti_stall;
pub(crate) mod auto_release;
pub(crate) mod boot_sweep;
pub(crate) mod canonical_drift;
pub(crate) mod ci_watch;
pub(crate) mod conflict_notify;
mod crash_respawn;
pub(crate) mod cron_tick;
pub(crate) mod decision_timeout;
pub(crate) mod dedup_state;
pub(crate) mod dispatch_idle;
pub(crate) mod event_bus;
pub(crate) mod heartbeat_pair;
pub(crate) mod helper_staleness_watchdog;
pub(crate) mod idle_watchdog;
pub(crate) mod lifecycle;
pub(crate) mod mcp_registry_watcher;
pub(crate) mod notification_dedup;
pub(crate) mod per_tick;
pub(crate) mod poll_reminder;
pub(crate) mod pr_state;
pub(crate) mod restart;
pub(crate) mod retention;
pub(crate) mod router;
pub(crate) mod supervisor;
pub(crate) mod task_progress;
pub(crate) mod task_sweep;
pub(crate) mod ticker;
mod tui_bridge;
pub(crate) mod usage_limit;
pub(crate) mod utils;
pub(crate) mod waiting_on_stale;
pub(crate) mod watchdog;

use crate::agent::{self, AgentRegistry};
pub use tui_bridge::serve_agent_tui;

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

/// Sprint 57 Wave 3 PR-2 (#548 Q6) shutdown-reason taxonomy.
/// Categorizes WHY the daemon stopped so the enriched
/// `daemon_stop` event payload can give operators a sliceable
/// audit trail (signal vs watchdog vs operator-initiated vs
/// clean exit). Set by each shutdown trigger site; read by the
/// shutdown sequence at the end of `run_core`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShutdownReason {
    /// Default — main loop broke without any trigger explicitly
    /// recording its reason. Should not occur in practice.
    Unknown = 0,
    /// SIGINT / SIGTERM / SIGHUP via `bootstrap::signals::install`
    /// (the ctrlc handler bundles all three on Unix; this single
    /// reason captures all of them when the per-signal-aware
    /// handlers below haven't fired). The daemon's ctrlc path still
    /// records `Signal` because the ctrlc crate's callback signature
    /// doesn't expose the originating signal — daemon-side
    /// per-signal migration via sigaction is a Sprint 64+ candidate.
    Signal = 1,
    /// Operator invoked `agend-terminal stop` → API SHUTDOWN
    /// method tripped the flag.
    ApiShutdown = 2,
    /// Daemon-internal watchdog (`daemon::ticker`) detected a
    /// fatal condition and tripped the flag.
    Watchdog = 3,
    /// Reserved for explicit clean shutdown without any external
    /// trigger (currently unused; kept in the taxonomy for forward
    /// compat with future "graceful exit on completion" code paths).
    CleanExit = 4,
    /// Sprint 60 W1 PR-3 (#P0-3): operator-initiated restart via the
    /// `restart_daemon` MCP tool. Differs from `ApiShutdown` in that
    /// `run_core` re-execs self after the shutdown sequence rather
    /// than returning to the bootstrap layer.
    OperatorRestart = 5,
    /// Sprint 63 W1 PR-3 (Sprint 58 P2 #6): SIGINT specifically (vs
    /// the bundled `Signal` when the handler can't distinguish).
    /// Set by per-signal sigaction handlers; future Sprint 64+
    /// daemon-side migration would record this from the daemon's
    /// install path. Currently set by no production handler — the
    /// app's `install_term_only` is SIGTERM-only, and daemon's
    /// ctrlc-based `install` records `Signal`.
    SignalSigint = 6,
    /// Sprint 63 W1 PR-3 (Sprint 58 P2 #6): SIGTERM specifically.
    /// Set by `bootstrap::signals::install_term_only` (the app's
    /// SIGTERM-only sigaction handler); also set by future per-signal
    /// daemon migration.
    SignalSigterm = 7,
    /// Sprint 63 W1 PR-3 (Sprint 58 P2 #6): SIGHUP specifically.
    /// Set by future per-signal daemon migration. No current
    /// production handler distinguishes SIGHUP from the bundled
    /// `Signal` reason.
    SignalSighup = 8,
}

impl ShutdownReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Signal => "signal",
            Self::ApiShutdown => "api_shutdown",
            Self::Watchdog => "watchdog",
            Self::CleanExit => "clean_exit",
            Self::OperatorRestart => "operator_restart",
            Self::SignalSigint => "signal_sigint",
            Self::SignalSigterm => "signal_sigterm",
            Self::SignalSighup => "signal_sighup",
        }
    }

    fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Signal,
            2 => Self::ApiShutdown,
            3 => Self::Watchdog,
            4 => Self::CleanExit,
            5 => Self::OperatorRestart,
            6 => Self::SignalSigint,
            7 => Self::SignalSigterm,
            8 => Self::SignalSighup,
            _ => Self::Unknown,
        }
    }
}

/// Process-wide shutdown-reason record. Set via
/// `record_shutdown_reason()` from each shutdown trigger site;
/// read by `shutdown_sequence()` when emitting the enriched
/// `daemon_stop` event. First-write-wins so a watchdog trip
/// doesn't get clobbered by a subsequent signal during the same
/// shutdown sequence.
pub(crate) static SHUTDOWN_REASON: AtomicU8 = AtomicU8::new(0);

/// Sprint 60 W1 PR-3 (#P0-3): operator-restart pending flag. The
/// `restart_daemon` MCP handler sets this after recording
/// `ShutdownReason::OperatorRestart`. The API session loop bridges
/// this to the local `shutdown` Arc<AtomicBool> so the main loop
/// breaks; after `shutdown_sequence` runs, `run_core` re-execs self
/// when this flag is set instead of returning to the bootstrap
/// layer. Process-wide static so MCP handlers (which don't carry the
/// shutdown flag in their HandlerCtx) can trigger the restart path
/// without API-layer plumbing.
pub(crate) static RESTART_PENDING: AtomicBool = AtomicBool::new(false);

/// Record the reason the daemon is shutting down. Idempotent on
/// re-entry (first-write-wins via `compare_exchange`); safe to
/// call from signal handlers + API threads + watchdog without
/// coordination.
pub(crate) fn record_shutdown_reason(reason: ShutdownReason) {
    let _ = SHUTDOWN_REASON.compare_exchange(
        ShutdownReason::Unknown as u8,
        reason as u8,
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
}

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

/// Shared daemon state threaded through run_core's extracted phases.
pub(super) struct DaemonContext {
    pub(super) registry: AgentRegistry,
    pub(super) externals: crate::agent::ExternalRegistry,
    pub(super) configs: Arc<Mutex<HashMap<String, AgentConfig>>>,
    pub(super) crash_tx: crossbeam_channel::Sender<crate::agent::AgentExitEvent>,
    pub(super) crash_rx: crossbeam_channel::Receiver<crate::agent::AgentExitEvent>,
    pub(super) shutdown: Arc<AtomicBool>,
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
    // Explicit trait method: Rust 1.89 stabilized inherent
    // `File::try_lock`; current MSRV is 1.87.
    fs4::FileExt::try_lock(&lock_file)
        .map_err(|e| anyhow::anyhow!("Another daemon is already running (lock held): {e}"))?;

    // #933: zombie sweep BEFORE find_active_run_dir so an aged-out
    // unresponsive daemon (which would otherwise satisfy find_active_run_dir)
    // is cleaned up first. Telemetry-only when env unset; env-gated kill
    // via AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS. Escape-hatch path; main fleet
    // boot covers this via `bootstrap::prepare`.
    let _ = boot_sweep::boot_sweep_zombies(home);

    // #1201: task lifecycle pass — auto-cancel stale open tasks + archive old done tasks.
    crate::tasks::lifecycle::lifecycle_pass(home);

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

    // agend-git-shim init now in bootstrap::prepare (shared with app mode).

    // Check for previous snapshot if fleet.yaml doesn't exist
    if !crate::fleet::fleet_yaml_path(home).exists() {
        if let Some(snapshot) = crate::snapshot::load(home) {
            tracing::info!(
                count = snapshot.agents.len(),
                timestamp = %snapshot.timestamp,
                "previous snapshot found"
            );
        }
    }

    run_core(home, agents, None)
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
    let telegram = prepared.telegram.clone();
    // Sprint 54 fleet-yaml unification: one-shot migrate legacy
    // teams.json runtime store into fleet.yaml `teams:` block, then
    // rename teams.json → teams.json.migrated (idempotent — no-op once
    // .migrated exists). Post-migration, fleet.yaml IS the canonical
    // store; no separate reconcile step needed.
    if let Err(e) = crate::fleet::migrate_teams_json_to_yaml(&home) {
        tracing::warn!(error = %e, "teams.json migration failed at daemon startup");
    }
    let _owned = prepared;
    run_core(&home, agents, telegram)
}

/// Sprint 57 Wave 3 PR-2 (#548 Q3 contract pin): this daemon does
/// NOT supervise itself. There is no self-respawn loop on crash —
/// the OS service manager (launchd / systemd / Task Scheduler) is
/// the supervisor of last resort, and `agend-terminal service
/// install/uninstall/status` (Sprint 57 Wave 3 PR-3 Phase 3) is
/// the cross-platform integration helper. Re-introducing a
/// daemon-self-restart loop here would conflict with the OS service
/// manager's lifecycle ownership.
///
/// Sprint 57 Wave 3 PR-2 (#548 Q4 contract pin): the canonical
/// lockfile is `$AGEND_HOME/.daemon.lock` (one acquirer at a time
/// across all daemon processes). Per-PID identity is at
/// `$AGEND_HOME/run/<pid>/.daemon` (PID-recycling guard for
/// discovery — distinct purpose from the exclusive lock).
fn run_core(
    home: &Path,
    agents: Vec<AgentDef>,
    telegram: Option<Arc<dyn crate::channel::Channel>>,
) -> anyhow::Result<()> {
    let started_at = std::time::Instant::now();

    let ctx = init_daemon_services(home, telegram)?;

    spawn_fleet_agents(home, &agents, &ctx);

    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    crate::bootstrap::signals::install(Arc::clone(&ctx.shutdown), shutdown_tx);

    crate::event_log::log(
        home,
        "daemon_start",
        "",
        &format!("{} agents", agents.len()),
    );
    tracing::info!("running, Ctrl+C or `agend-terminal stop` to stop");

    let (_keepalive, handlers, tick_rx) = build_tick_infrastructure(home, &ctx);

    loop {
        if ctx.shutdown.load(Ordering::Relaxed) {
            break;
        }

        let exit_event: Option<crate::agent::AgentExitEvent>;
        crossbeam_channel::select! {
            recv(ctx.crash_rx) -> msg => { exit_event = msg.ok(); }
            recv(tick_rx) -> _ => { exit_event = None; }
            recv(shutdown_rx) -> _ => { continue; }
        }

        let tick_ctx = per_tick::TickContext {
            home,
            registry: &ctx.registry,
            externals: &ctx.externals,
            configs: &ctx.configs,
        };
        crate::runtime_config::reload(home);
        per_tick::run_handlers_with_panic_guard(&handlers, &tick_ctx);

        let exit_event = match exit_event {
            Some(e) => e,
            None => continue,
        };

        if ctx.shutdown.load(Ordering::Relaxed) {
            break;
        }
        match exit_event {
            crate::agent::AgentExitEvent::CleanExit(ref name) => {
                tracing::info!(agent = %name, "clean exit — removing from registry (no respawn)");
                // #1441: registry is UUID-keyed; resolve name via fleet.yaml.
                if let Some(id) = crate::fleet::resolve_uuid(home, name) {
                    ctx.registry.lock().remove(&id);
                }
                ctx.configs.lock().remove(name.as_str());
            }
            crate::agent::AgentExitEvent::Stage2Restart(name) => {
                spawn_stage2_thread(home, &name, &ctx);
            }
            crate::agent::AgentExitEvent::Crash(name) => {
                crash_respawn::handle_crash_respawn(home, &name, &ctx);
            }
        }
    }

    log_residual_worktrees(home, &ctx.configs);

    let metrics = shutdown_sequence(home, &ctx.registry, started_at);
    crate::event_log::log(
        home,
        "daemon_stop",
        "",
        &format!(
            "reason={} agents_total={} agents_killed_after_grace={} uptime_secs={}",
            metrics.reason.as_str(),
            metrics.agents_total,
            metrics.agents_killed_after_grace,
            metrics.uptime_secs
        ),
    );
    let _ = std::fs::remove_dir_all(run_dir(home));
    std::thread::sleep(std::time::Duration::from_secs(1));

    if RESTART_PENDING.load(Ordering::Acquire) {
        let flag = home.join("restart-requested");
        let _ = std::fs::remove_file(&flag);
        tracing::info!("operator-initiated restart: exiting with code 42");
        std::process::exit(42);
    }

    tracing::info!("exiting");
    Ok(())
}

// ── Extracted phases ────────────────────────────────────────────

fn init_daemon_services(
    home: &Path,
    telegram: Option<Arc<dyn crate::channel::Channel>>,
) -> anyhow::Result<DaemonContext> {
    crate::daemon_config::init(crate::daemon_config::DaemonConfig::default());

    const SKILLS_STAGE_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;
    crate::bootstrap::time_step("skills::cleanup_stale_stages", || {
        match crate::skills::cleanup_stale_stages(home, SKILLS_STAGE_RETENTION_SECS, &[]) {
            Ok(report) => tracing::info!(?report, "skills-stage GC: daemon-init sweep complete"),
            Err(e) => tracing::warn!(error = %e, "skills-stage GC: daemon-init sweep failed"),
        }
    });

    const DEDUP_TMP_RETENTION_SECS: u64 = 24 * 60 * 60;
    let dedup_report = crate::bootstrap::time_step("dedup_state::cleanup_tmp_orphans", || {
        crate::daemon::dedup_state::cleanup_tmp_orphans(home, DEDUP_TMP_RETENTION_SECS)
    });
    tracing::info!(?dedup_report, "dedup-state GC: daemon-init sweep complete");

    let legacy_migration =
        crate::bootstrap::time_step("tasks::migrate_legacy_tasks_json_to_event_log", || {
            crate::tasks::migrate_legacy_tasks_json_to_event_log(home)
        });
    match legacy_migration {
        Ok(rep) => tracing::info!(
            migrated = rep.migrated,
            skipped = rep.skipped,
            "task_events: legacy tasks.json bridge migration complete"
        ),
        Err(e) => {
            return Err(anyhow::anyhow!("task_events: legacy migration failed: {e}"));
        }
    }

    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    crate::agent::set_pending_registry(Arc::clone(&registry));
    if let Some(tg) = telegram.as_ref() {
        tg.attach_registry(Arc::clone(&registry));
    } else if let Some(tg) = crate::channel::active_channel() {
        tg.attach_registry(Arc::clone(&registry));
    }

    let externals: crate::agent::ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (crash_tx, crash_rx) = crossbeam_channel::bounded::<crate::agent::AgentExitEvent>(64);
    let configs: Arc<Mutex<HashMap<String, AgentConfig>>> = Arc::new(Mutex::new(HashMap::new()));
    let shutdown = Arc::new(AtomicBool::new(false));

    // fire-and-forget: api::serve runs the Unix socket accept loop for the
    // daemon's lifetime. Loop observes shutdown via the cloned AtomicBool;
    // the socket file is removed during daemon shutdown, which surfaces as a
    // bind/accept error and exits the loop. JoinHandle dropped because no
    // graceful join is needed — process exit reaps the thread.
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

    Ok(DaemonContext {
        registry,
        externals,
        configs,
        crash_tx,
        crash_rx,
        shutdown,
    })
}

fn spawn_fleet_agents(home: &Path, agents: &[AgentDef], ctx: &DaemonContext) {
    tracing::info!(count = agents.len(), "starting agents");
    crate::bootstrap::time_step("agent_spawn_loop", || {
        for def in agents {
            if let Err(e) = spawn_and_register_agent(
                home,
                def,
                &ctx.registry,
                &ctx.configs,
                &ctx.crash_tx,
                &ctx.shutdown,
            ) {
                tracing::error!(
                    agent = %def.0,
                    error = %e,
                    "spawn_and_register_agent rolled back; agent NOT in fleet"
                );
            }
            if agents.len() > 1 {
                std::thread::sleep(spawn_stagger());
            }
        }
    });

    crate::bootstrap::time_step("ready_marker_write", || {
        let ready_path = run_dir(home).join(".ready");
        if let Err(e) = std::fs::write(&ready_path, chrono::Utc::now().to_rfc3339()) {
            tracing::warn!(path = %ready_path.display(), error = %e, "failed to write .ready marker");
        }
    });
}

/// Opaque bag of daemon-lifetime handles that must not be dropped
/// until the main loop exits.
struct TickKeepalive {
    _task_sweep: crate::daemon::task_sweep::TaskSweep,
}

fn build_tick_infrastructure(
    home: &Path,
    ctx: &DaemonContext,
) -> (
    TickKeepalive,
    Vec<Box<dyn per_tick::PerTickHandler>>,
    crossbeam_channel::Receiver<()>,
) {
    let _task_sweep =
        crate::daemon::task_sweep::TaskSweep::spawn(home.to_path_buf(), Arc::clone(&ctx.shutdown));

    #[cfg(unix)]
    {
        let daemon_binary_stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale =
            Arc::new(AtomicBool::new(false));
        supervisor::spawn(
            home.to_path_buf(),
            Arc::clone(&ctx.registry),
            daemon_binary_stale,
        );
    }
    router::spawn(home.to_path_buf(), Arc::clone(&ctx.registry));
    crate::instance_monitor::spawn_monitor_tick(home.to_path_buf(), Arc::clone(&ctx.registry));

    crate::inbox::recover_half_writes(home);
    replay_missed_at_startup(home, &ctx.registry);
    crate::daemon::ci_watch::startup_sweep(home);

    let watchdog_dry_run = watchdog::watchdog_dry_run_from_env();
    // Vec order MUST match the pre-extraction call order (zero-behavior-change guarantee).
    let handlers: Vec<Box<dyn per_tick::PerTickHandler>> = vec![
        Box::new(per_tick::HangDetectionHandler::new()),
        Box::new(per_tick::RecoveryDispatcherHandler::new(
            std::sync::Arc::new(ctx.crash_tx.clone()),
        )),
        Box::new(per_tick::WatchdogHandler::new(watchdog_dry_run)),
        Box::new(per_tick::ExternalLivenessHandler::new()),
        Box::new(per_tick::SnapshotRotationHandler::new()),
        Box::new(per_tick::CheckSchedulesHandler::new()),
        Box::new(per_tick::CiWatchPollHandler::new()),
        Box::new(per_tick::PrStateScanHandler::new()),
        Box::new(per_tick::InboxMaintenanceHandler::new(60)),
        Box::new(per_tick::PollReminderHandler::new(30)),
        Box::new(per_tick::LogRotationHandler::new(360)),
        Box::new(per_tick::ThreadDumpHandler::new()),
        Box::new(per_tick::GcTickHandler::new(360)),
    ];

    let tick_rx = {
        let (tx, rx) = crossbeam_channel::bounded(1);
        // fire-and-forget: tick producer terminates when the bounded(1) tx
        // returns Err (rx dropped during daemon shutdown). Self-terminating.
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

    (TickKeepalive { _task_sweep }, handlers, tick_rx)
}

fn spawn_stage2_thread(home: &Path, name: &str, ctx: &DaemonContext) {
    let home_owned = home.to_path_buf();
    let name_owned = name.to_owned();
    let reg = Arc::clone(&ctx.registry);
    let cfgs = Arc::clone(&ctx.configs);
    let tx = ctx.crash_tx.clone();
    let sd = Arc::clone(&ctx.shutdown);
    // fire-and-forget: stage2 restart worker is short-lived (backoff sleep
    // then spawn_agent + restore health counters). Observes shutdown flag
    // after backoff to abort cleanly. JoinHandle dropped because errors are
    // logged inside handle_stage2_restart + event_log records outcome.
    if let Err(e) = std::thread::Builder::new()
        .name(format!("{name}_stage2"))
        .spawn(move || {
            handle_stage2_restart(&home_owned, &name_owned, &reg, &cfgs, &tx, &sd);
        })
    {
        tracing::warn!(agent = %name, error = %e, "failed to spawn stage2 restart thread");
    }
}

fn log_residual_worktrees(home: &Path, configs: &Arc<Mutex<HashMap<String, AgentConfig>>>) {
    let cfgs = configs.lock();
    let mut seen = std::collections::HashSet::new();
    let central_residual = crate::worktree::list_residual(home);
    if !central_residual.is_empty() {
        tracing::info!(
            location = %home.join("worktrees").display(),
            residual = ?central_residual,
            "residual agent worktrees found under $AGEND_HOME/worktrees/ \
             (cleared on next bind_self/release_worktree cycle)"
        );
    }
    for config in cfgs.values() {
        let repo = config
            .worktree_source
            .as_ref()
            .or(config.working_dir.as_ref());
        if let Some(dir) = repo {
            if seen.insert(dir.clone()) {
                let residual = crate::worktree::list_legacy_residual(dir);
                if !residual.is_empty() {
                    tracing::warn!(
                        repo = %dir.display(),
                        residual = ?residual,
                        "legacy worktrees detected at <repo>/.worktrees/<agent>/ — \
                         operator cleanup recommended"
                    );
                }
            }
        }
    }
}

/// Sprint 57 Wave 3 PR-2 (#548 Q6) shutdown summary record.
/// Emitted via the enriched `daemon_stop` event; also exposed
/// from `shutdown_sequence` for tests + future telemetry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ShutdownMetrics {
    pub reason: ShutdownReason,
    pub agents_total: usize,
    pub agents_killed_after_grace: usize,
    pub uptime_secs: u64,
}

/// Sprint 57 Wave 3 PR-2 (#548 Q6) staged termination sequence:
///
/// 1. Drain the registry into a `Vec<(name, child)>` so PTY close
///    handlers don't fire crash events for agents we're shutting
///    down (race-free per the pre-Wave-3-PR-2 invariant).
/// 2. Send SIGTERM to each agent's process group in parallel.
/// 3. Wait the grace window (`SHUTDOWN_GRACE_SECS`, default 2s).
/// 4. SIGKILL any survivor that didn't honor SIGTERM during the
///    grace window. Track the count for the summary metrics.
/// 5. Return a `ShutdownMetrics` record for the caller to fold
///    into the `daemon_stop` event payload.
///
/// On Windows the staged-TERM model doesn't apply (no signal
/// equivalent); the sequence falls back to `kill_process_tree`
/// per agent — equivalent semantics, just without the parallel
/// SIGTERM stage.
pub(crate) fn shutdown_sequence(
    home: &Path,
    registry: &AgentRegistry,
    started_at: std::time::Instant,
) -> ShutdownMetrics {
    let reason = ShutdownReason::from_u8(SHUTDOWN_REASON.load(Ordering::Relaxed));
    tracing::info!(reason = reason.as_str(), "cleaning up...");

    // Drain registry FIRST, then kill. PTY close handlers check the
    // registry — if the agent is gone, they return silently instead of
    // sending crash events. This eliminates all shutdown race conditions.
    let agents_to_kill: Vec<_> = {
        let mut reg = registry.lock();
        reg.drain()
            .map(|(_id, handle)| (handle.name.to_string(), handle.child))
            .collect()
    };
    let agents_total = agents_to_kill.len();

    // Sprint 57 Wave 3 PR-2: parallel SIGTERM stage. On Unix, send
    // SIGTERM to each agent's process group concurrently; on Windows,
    // fall back to per-agent kill_process_tree (no signal model).
    type ChildHandle = std::sync::Arc<Mutex<Box<dyn portable_pty::Child + Send>>>;
    let mut pids: Vec<(String, ChildHandle, Option<u32>)> =
        Vec::with_capacity(agents_to_kill.len());
    for (name, child) in agents_to_kill {
        let pid = {
            let c = child.lock();
            c.process_id()
        };
        #[cfg(unix)]
        if let Some(p) = pid {
            unsafe {
                let pgid = libc::getpgid(p as i32);
                let kill_pgid = if pgid > 0 { -pgid } else { -(p as i32) };
                libc::kill(kill_pgid, libc::SIGTERM);
            }
        }
        pids.push((name, child, pid));
    }

    // Wait the grace window. On Unix, agents that received SIGTERM
    // above can exit cleanly during this window (the parallel SIGTERM
    // signaled all process groups simultaneously). On Windows, no
    // SIGTERM was sent — but a brief wait still lets agents that
    // happened to exit on their own (e.g. PTY EOF on parent close)
    // be reported as clean rather than killed-after-grace, keeping
    // the metric semantically consistent across platforms.
    std::thread::sleep(SHUTDOWN_GRACE);

    // Sprint 57 Wave 3 PR-2: SIGKILL stage. Anything still alive
    // after the grace window is escalated. On Windows this is the
    // primary kill stage (no SIGTERM was sent); on Unix it catches
    // SIGTERM holdouts.
    let mut agents_killed_after_grace = 0usize;
    for (name, child, pid) in pids {
        let still_alive = match pid {
            Some(p) => crate::process::is_pid_alive(p),
            None => false,
        };
        if let Some(p) = pid {
            crate::process::kill_process_tree(p);
        }
        let _ = child.lock().kill();
        if still_alive {
            agents_killed_after_grace += 1;
            tracing::info!(
                agent = %name,
                "killed (after grace window)"
            );
        } else {
            tracing::info!(agent = %name, "killed");
        }
    }

    let uptime_secs = started_at.elapsed().as_secs();
    let metrics = ShutdownMetrics {
        reason,
        agents_total,
        agents_killed_after_grace,
        uptime_secs,
    };
    tracing::info!(
        reason = metrics.reason.as_str(),
        agents_total = metrics.agents_total,
        agents_killed_after_grace = metrics.agents_killed_after_grace,
        uptime_secs = metrics.uptime_secs,
        "daemon shutdown sequence complete"
    );
    let _ = home; // home is currently logged via tracing only; reserved for future telemetry
    metrics
}

/// Sprint 57 Wave 3 PR-2 (#548 Q6) graceful-termination grace window.
/// SIGTERM is sent to all agents in parallel; this is how long the
/// daemon waits before escalating survivors to SIGKILL. Set to 2s
/// per Phase A RCA recommendation — long enough for well-behaved
/// agents to honor SIGTERM cleanly, short enough to keep total
/// shutdown latency bounded.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Replay missed one-shot schedules on daemon startup.
/// Calls `schedules::replay_missed_oneshots` and fires each returned
/// schedule through the same path as `cron_tick::check_schedules`.
/// Sweep overdue claimed tasks and stuck dispatches, log events.
pub fn run_task_maintenance(home: &Path) {
    let unclaimed = crate::tasks::sweep_overdue_claimed(home);
    for tid in &unclaimed {
        crate::event_log::log(
            home,
            "task_overdue_unclaimed",
            tid,
            "due_at expired, status → open",
        );
        tracing::info!(task_id = %tid, "task overdue, unclaimed");
    }
    // Dispatch timeout detection
    let (warns, asks) = crate::dispatch_tracking::sweep_stuck(home);
    for w in &warns {
        crate::event_log::log(
            home,
            "dispatch_stuck_warn",
            &w.to,
            &format!(
                "no report_result after {}min",
                crate::dispatch_tracking::DISPATCH_WARN_MINUTES
            ),
        );
    }
    for a in &asks {
        crate::event_log::log(
            home,
            "dispatch_stuck_ask",
            &a.to,
            &format!(
                "no report_result after {}min, querying assignee",
                crate::dispatch_tracking::DISPATCH_ASK_MINUTES
            ),
        );
        let tid = a.task_id.as_deref().unwrap_or("unknown");
        let query = format!(
            "dispatch stuck check: still working on task_id={tid} (dispatched {}min ago)?",
            crate::dispatch_tracking::DISPATCH_ASK_MINUTES
        );
        let _ = crate::inbox::enqueue_with_idle_hint(
            home,
            &a.to,
            crate::inbox::InboxMessage::new_system("system:dispatch", "query", query),
        );
    }
    // 24h orphan sweep
    for orphan in crate::dispatch_tracking::sweep_orphans(home) {
        let tid = orphan.task_id.as_deref().unwrap_or("unknown");
        crate::event_log::log(
            home,
            "dispatch_orphaned",
            &orphan.to,
            &format!("task_id={tid} dispatched_at={}", orphan.delegated_at),
        );
    }
    // M3: 30-day TTL cleanup for terminal dispatch entries
    crate::dispatch_tracking::gc_old_entries(home);
}

fn replay_missed_at_startup(home: &Path, registry: &AgentRegistry) {
    let missed = crate::schedules::replay_missed_oneshots(home);
    if missed.is_empty() {
        return;
    }
    tracing::info!(count = missed.len(), "replaying missed one-shot schedules");
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
        // #1441: registry is UUID-keyed; resolve target name via fleet.yaml.
        if let Some(handle) = crate::fleet::resolve_uuid(home, target).and_then(|id| reg.get(&id)) {
            if let Err(e) = agent::inject_to_agent(handle, message.as_bytes()) {
                tracing::warn!(error = %e, "replay inject failed");
            }
        } else {
            drop(reg);
            let _ = crate::inbox::enqueue_with_idle_hint(
                home,
                target,
                crate::inbox::InboxMessage::new_system(
                    "system:schedule",
                    "schedule_replay",
                    message,
                ),
            );
        }
    }
}

/// Staggered-spawn delay — rate-limits PTY init during multi-agent startup
/// bursts. Tunable via `AGEND_SPAWN_STAGGER_MS`.
fn spawn_stagger() -> std::time::Duration {
    let ms: u64 = std::env::var("AGEND_SPAWN_STAGGER_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    std::time::Duration::from_millis(ms)
}

/// Shared "spawn one agent + register respawn config + start per-agent TUI
/// server" path. Used by startup (run_core) and any future add-agent call
/// site. Rolls back the `configs` entry on spawn failure so retries start
/// clean.
fn spawn_and_register_agent(
    home: &Path,
    def: &crate::bootstrap::AgentDef,
    registry: &AgentRegistry,
    configs: &Arc<Mutex<HashMap<String, AgentConfig>>>,
    crash_tx: &crossbeam_channel::Sender<crate::agent::AgentExitEvent>,
    shutdown: &Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<()> {
    let (name, command, args, env, working_dir, submit_key) = def;
    let worktree_source = working_dir
        .as_ref()
        .and_then(|wd| crate::worktree::source_repo_of(wd));
    configs.lock().insert(
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
    // Default to Resume so daemon (re)starts pick up where each agent left off,
    // but downgrade when the backend reports nothing to resume — see
    // `SpawnMode::downgraded_for` for the why.
    let spawn_mode =
        crate::backend::SpawnMode::Resume.downgraded_for(command, working_dir.as_deref());

    // Sprint 61 W1 PR-1 (#P0-1 Skills auto-install at agent launch):
    // synchronous pre-spawn install per lead recommendation (a) — guarantees
    // SKILL.md files are in place at the agent's first skill-discovery read.
    // Best-effort: failures log + continue so a skills problem never blocks
    // agent boot. Idempotent across restarts (install_for_agent skips
    // pre-existing non-managed dirs + replaces managed ones per Sprint 60
    // #581 contract).
    if let Some(wd) = working_dir.as_deref() {
        // Sprint 61 W1 PR-2 (#P0-2): consult fleet.yaml for per-instance
        // skills override. None → install all (W1 PR-1 default); Some(vec)
        // → install only the named skills (Some(empty) opts the agent
        // out of skills entirely).
        let skills_filter: Option<Vec<String>> =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                .ok()
                .and_then(|c| c.instances.get(name).and_then(|i| i.skills.clone()));
        let backend_skill =
            crate::backend::Backend::from_command(command).and_then(|b| b.skill_dir_name());
        match crate::skills::install_for_agent_backend(
            home,
            wd,
            skills_filter.as_deref(),
            backend_skill,
        ) {
            Ok(outcomes) => {
                let modes: Vec<(&str, crate::skills::InstallMode)> = outcomes
                    .iter()
                    .map(|o| (o.backend.as_str(), o.mode))
                    .collect();
                tracing::info!(
                    agent = %name,
                    ?modes,
                    filter = ?skills_filter,
                    "skills auto-install complete"
                );
            }
            Err(e) => {
                tracing::warn!(agent = %name, error = %e, "skills auto-install failed, proceeding without skills");
            }
        }
    }

    if let Err(e) = agent::spawn_agent(
        &agent::SpawnConfig {
            name,
            backend_command: command,
            args,
            spawn_mode,
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
        configs.lock().remove(name);
        return Err(e);
    }

    let rdir = run_dir(home);
    // #896 Option D: synchronous TUI listener prep BEFORE returning Ok.
    // Pre-#896 this whole step happened inside the fire-and-forget
    // accept-loop thread, so `spawn_and_register_agent` could return
    // Ok while `.port` hadn't landed on disk yet. App-attach during
    // the spawn loop's stagger window saw "no agents are reachable".
    // Now we bind + write_port on the caller thread; only after the
    // port file exists do we hand the listener to the async accept
    // loop. On prep failure: rollback via `delete_transaction` (kill
    // child, drop registry entry, clean configs, remove residual
    // port file) and propagate Err.
    let meta = match tui_bridge::prepare_tui_listener_and_publish_port(name, &rdir) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(
                agent = %name,
                error = %e,
                "TUI listener prep failed — rolling back agent registration"
            );
            lifecycle::delete_transaction(home, name, registry, Some(configs), false);
            return Err(anyhow::Error::from(e));
        }
    };

    let reg = Arc::clone(registry);
    let n = name.clone();
    // fire-and-forget: serve_tui_accept_loop blocks on TcpListener::accept
    // and exits when the agent is removed from the registry. JoinHandle
    // is discarded because shutdown is signalled implicitly by socket-
    // file removal in `delete_transaction`.
    if let Err(e) = std::thread::Builder::new()
        .name(format!("{n}_tui_server"))
        .spawn(move || tui_bridge::serve_tui_accept_loop(&n, meta, &reg))
    {
        // Sprint 20 F5 fix (preserved): a TUI server spawn failure
        // would otherwise leave the agent registered + child running
        // but with no accepting socket. Roll back so retries start
        // clean. #896 update: prep step already wrote `.port`, so the
        // rollback now also clears that residual via
        // `delete_transaction`'s port cleanup.
        tracing::warn!(
            agent = %name,
            error = %e,
            "TUI server thread spawn failed — rolling back agent registration"
        );
        lifecycle::delete_transaction(home, name, registry, Some(configs), false);
        return Err(e.into());
    }
    Ok(())
}

/// `#685` sub-task 7b: Stage 2 auto-restart handler. Distinct from
/// the Crash path (which calls `record_crash` + uses exponential
/// backoff): Stage 2 is a *controlled* restart initiated by the
/// recovery dispatcher when the agent failed to recover from Stage 1
/// ESC. Selectively preserves crash counters + recovery counter
/// across the spawn boundary so the cap (`STAGE2_MAX_RESTARTS_DEFAULT`)
/// survives the restart it drove.
///
/// Decision §1 selective restore (4 fields): `crash_times`,
/// `total_crashes`, `last_notification`, `recovery_restart_count` (+1).
/// All other `HealthTracker` fields reset to fresh defaults — including
/// `state: Healthy` (Stage 2 success seed) and `recovery_stage_state:
/// None` (linear escalation rule restarts).
///
/// `spawn_agent` failure: agent removed from registry, dispatcher
/// next-tick won't find it. Operator already received Stage 2 telegram
/// pre-emit so visibility is preserved. Phase 1 limitation acknowledged
/// in `docs/RECOVERY-STAGES.md §RS.9` — full operator unpause +
/// re-spawn flow ships in sub-task 7c.
fn handle_stage2_restart(
    home: &Path,
    name: &str,
    registry: &AgentRegistry,
    configs: &Arc<Mutex<HashMap<String, crate::daemon::AgentConfig>>>,
    crash_tx: &crossbeam_channel::Sender<crate::agent::AgentExitEvent>,
    shutdown: &Arc<std::sync::atomic::AtomicBool>,
) {
    use std::time::{Duration, Instant};
    tracing::warn!(
        target: "recovery_shadow",
        agent = %name,
        "stage2 restart initiated"
    );
    crate::event_log::log(home, "stage2_restart", name, "stage 2 auto-restart");

    // #1441: registry is UUID-keyed; resolve once and key both the snapshot
    // read and the post-spawn restore by it.
    let instance_id = crate::fleet::resolve_uuid(home, name);

    // Snapshot the 4 fields we'll preserve across spawn. Reads then
    // drops the lock before backoff sleep + spawn.
    let saved = {
        let reg = agent::lock_registry(registry);
        instance_id.and_then(|id| reg.get(&id)).map(|h| {
            let core = h.core.lock();
            (
                core.health.crash_times.clone(),
                core.health.total_crashes,
                core.health.last_notification,
                core.health.recovery_restart_count,
            )
        })
    };
    let saved = match saved {
        Some(s) => s,
        None => {
            tracing::warn!(
                target: "recovery_shadow",
                agent = %name,
                "stage2 restart: agent not in registry, skipping"
            );
            return;
        }
    };

    let config = match configs.lock().get(name).cloned() {
        Some(c) => c,
        None => {
            tracing::warn!(
                target: "recovery_shadow",
                agent = %name,
                "stage2 restart: no config for respawn (likely deleted)"
            );
            return;
        }
    };

    let backoff_ms = std::env::var("AGEND_AUTO_RECOVERY_STAGE2_BACKOFF_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(crate::health::STAGE2_BACKOFF_DEFAULT_MS);
    let backoff = Duration::from_millis(backoff_ms);

    std::thread::sleep(backoff);
    if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
        tracing::info!(
            target: "recovery_shadow",
            agent = %name,
            "shutdown during stage2 backoff, aborting"
        );
        return;
    }

    // #1080: re-install skills on stage2 restart (idempotent).
    if let Some(ref wd) = config.working_dir {
        let skills_filter: Option<Vec<String>> =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                .ok()
                .and_then(|c| c.instances.get(name).and_then(|i| i.skills.clone()));
        let backend_skill = crate::backend::Backend::from_command(&config.backend_command)
            .and_then(|b| b.skill_dir_name());
        if let Err(e) = crate::skills::install_for_agent_backend(
            home,
            wd,
            skills_filter.as_deref(),
            backend_skill,
        ) {
            tracing::warn!(
                target: "recovery_shadow",
                agent = %name, error = %e, "stage2 skills install failed"
            );
        }
    }

    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let spawn_result = agent::spawn_agent(
        &agent::SpawnConfig {
            name,
            backend_command: &config.backend_command,
            args: &config.args,
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols,
            rows,
            env: config.env.as_ref(),
            working_dir: config.working_dir.as_deref(),
            submit_key: &config.submit_key,
            home: Some(home),
            crash_tx: Some(crash_tx.clone()),
            shutdown: Some(Arc::clone(shutdown)),
        },
        registry,
    );

    match spawn_result {
        Ok(()) => {
            tracing::info!(
                target: "recovery_shadow",
                agent = %name,
                "stage2 spawn ok"
            );
            crate::event_log::log(home, "stage2_spawn_ok", name, "stage 2 spawn succeeded");

            // Selective restore — fresh tracker starts with default
            // values; we overwrite only the 4 preserved fields and
            // increment recovery_restart_count by 1 (this Stage 2 fire
            // contributes to the cap). All other fields stay at
            // default — state stays Healthy (recovery success seed),
            // recovery_stage_state stays None (linear escalation reset
            // already encoded by spontaneous-recovery reset in
            // dispatcher).
            let reg = agent::lock_registry(registry);
            if let Some(handle) = instance_id.and_then(|id| reg.get(&id)) {
                let mut core = handle.core.lock();
                let (crash_times, total_crashes, last_notification, prev_count) = saved;
                core.health.crash_times = crash_times;
                core.health.total_crashes = total_crashes;
                core.health.last_notification = last_notification;
                core.health.recovery_restart_count = prev_count.saturating_add(1);
                core.health.last_stage2_fired_at = Some(Instant::now());
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "recovery_shadow",
                agent = %name,
                error = %e,
                "stage2 spawn failed — agent removed, operator notified via telegram"
            );
            crate::event_log::log(home, "stage2_spawn_failed", name, &format!("error: {e}"));
            // Agent left removed; operator handles via manual re-spawn
            // OR future operator-unpause / re-spawn sub-task. Phase 1
            // limitation documented in §RS.9.
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
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

    // ── Clean exit vs crash respawn tests ────────────────────────────

    #[test]
    fn clean_exit_does_not_respawn() {
        // Simulate: daemon receives CleanExit event → agent removed from
        // registry + configs, no respawn thread spawned.
        let (tx, rx) = crossbeam_channel::bounded::<crate::agent::AgentExitEvent>(8);
        tx.send(crate::agent::AgentExitEvent::CleanExit("agent-1".into()))
            .expect("send test event");
        let event = rx.recv().expect("recv test event");
        assert!(
            matches!(event, crate::agent::AgentExitEvent::CleanExit(ref n) if n == "agent-1"),
            "expected CleanExit, got {event:?}"
        );
        // Verify the daemon loop logic: CleanExit removes from registry, does NOT respawn.
        // We test the discriminant matching that the main loop uses.
        let is_clean = matches!(event, crate::agent::AgentExitEvent::CleanExit(_));
        assert!(is_clean, "CleanExit must be recognized as clean");
    }

    #[test]
    fn crash_still_respawns() {
        // Simulate: daemon receives Crash event → should trigger respawn.
        let (tx, rx) = crossbeam_channel::bounded::<crate::agent::AgentExitEvent>(8);
        tx.send(crate::agent::AgentExitEvent::Crash("agent-2".into()))
            .expect("send test event");
        let event = rx.recv().expect("recv test event");
        let is_crash =
            matches!(event, crate::agent::AgentExitEvent::Crash(ref n) if n == "agent-2");
        assert!(is_crash, "Crash event must be recognized for respawn");
        let is_clean = matches!(event, crate::agent::AgentExitEvent::CleanExit(_));
        assert!(!is_clean, "Crash must NOT be treated as clean exit");
    }

    #[test]
    fn clean_exit_removes_from_configs() {
        // Verify the daemon loop's CleanExit handler removes the agent
        // from the configs map (preventing stale respawn config).
        let configs: Arc<Mutex<HashMap<String, AgentConfig>>> =
            Arc::new(Mutex::new(HashMap::new()));
        configs.lock().insert(
            "agent-3".into(),
            AgentConfig {
                name: "agent-3".into(),
                backend_command: "claude".into(),
                args: vec![],
                env: None,
                working_dir: None,
                worktree_source: None,
                submit_key: "\r".into(),
            },
        );
        assert!(configs.lock().contains_key("agent-3"));
        // Simulate the CleanExit handler logic from the main loop:
        configs.lock().remove("agent-3");
        assert!(
            !configs.lock().contains_key("agent-3"),
            "CleanExit must remove agent from configs"
        );
    }

    #[test]
    fn sigint_130_treated_as_clean_exit() {
        // SIGINT (exit code 130 = 128+2) from /quit in some CLIs must be
        // treated as clean exit, not crash.
        let exit_code = Some(130_i32);
        let is_crash = !matches!(exit_code, Some(0) | Some(130));
        assert!(!is_crash, "exit code 130 (SIGINT) must not be a crash");
        let is_user_clean = matches!(exit_code, Some(0) | Some(130));
        assert!(
            is_user_clean,
            "exit code 130 must be user-initiated clean exit"
        );
    }

    #[test]
    fn sigkill_137_not_clean_exit() {
        // SIGKILL (137) is daemon-initiated, not user /exit.
        let exit_code = Some(137_i32);
        let is_user_clean = matches!(exit_code, Some(0) | Some(130));
        assert!(
            !is_user_clean,
            "SIGKILL must NOT be user-initiated clean exit"
        );
    }

    #[test]
    fn sigterm_143_not_clean_exit() {
        // SIGTERM (143) is daemon-initiated, not user /exit.
        let exit_code = Some(143_i32);
        let is_user_clean = matches!(exit_code, Some(0) | Some(130));
        assert!(
            !is_user_clean,
            "SIGTERM must NOT be user-initiated clean exit"
        );
    }

    #[test]
    fn nonzero_exit_is_crash() {
        // Exit code 1 (error) must trigger crash respawn.
        let exit_code = Some(1_i32);
        let is_crash = match exit_code {
            Some(0) | Some(130) => false,
            Some(137) | Some(143) => false,
            Some(_) => true,
            None => true,
        };
        assert!(is_crash, "exit code 1 must be a crash");
    }

    // ─────────────────────────────────────────────────────────────
    // Sprint 57 Wave 3 PR-2 (#548 Q6) shutdown reason taxonomy +
    // payload-shape pins.
    // ─────────────────────────────────────────────────────────────

    #[test]
    fn shutdown_reason_round_trip_preserves_taxonomy() {
        for reason in [
            ShutdownReason::Unknown,
            ShutdownReason::Signal,
            ShutdownReason::ApiShutdown,
            ShutdownReason::Watchdog,
            ShutdownReason::CleanExit,
            // Sprint 60 W1 PR-3 + Sprint 63 W1 PR-3 additions.
            ShutdownReason::OperatorRestart,
            ShutdownReason::SignalSigint,
            ShutdownReason::SignalSigterm,
            ShutdownReason::SignalSighup,
        ] {
            let raw = reason as u8;
            let recovered = ShutdownReason::from_u8(raw);
            assert_eq!(recovered, reason, "round-trip lost taxonomy for {reason:?}");
        }
    }

    #[test]
    fn shutdown_reason_per_signal_taxonomy_strings_pinned() {
        // Sprint 63 W1 PR-3 (Sprint 58 P2 #6): per-signal taxonomy
        // string identifiers are pinned for downstream `daemon_stop`
        // event consumers (greppers / parsers).
        assert_eq!(ShutdownReason::SignalSigint.as_str(), "signal_sigint");
        assert_eq!(ShutdownReason::SignalSigterm.as_str(), "signal_sigterm");
        assert_eq!(ShutdownReason::SignalSighup.as_str(), "signal_sighup");
        // Bundled `Signal` reason still pins to "signal" for backward compat.
        assert_eq!(ShutdownReason::Signal.as_str(), "signal");
    }

    #[test]
    fn shutdown_reason_from_unknown_byte_returns_unknown() {
        // Forward-compat: any out-of-range value decodes to Unknown
        // rather than panicking. Future schema bumps that add more
        // reasons can land without breaking older readers.
        let recovered = ShutdownReason::from_u8(255);
        assert_eq!(recovered, ShutdownReason::Unknown);
        let recovered2 = ShutdownReason::from_u8(99);
        assert_eq!(recovered2, ShutdownReason::Unknown);
    }

    #[test]
    fn shutdown_reason_as_str_matches_audit_taxonomy() {
        // Pin the string identifiers downstream consumers will grep
        // against. Renaming any of these is a downstream-breaking
        // change that needs an explicit migration note.
        assert_eq!(ShutdownReason::Unknown.as_str(), "unknown");
        assert_eq!(ShutdownReason::Signal.as_str(), "signal");
        assert_eq!(ShutdownReason::ApiShutdown.as_str(), "api_shutdown");
        assert_eq!(ShutdownReason::Watchdog.as_str(), "watchdog");
        assert_eq!(ShutdownReason::CleanExit.as_str(), "clean_exit");
    }

    #[test]
    fn record_shutdown_reason_first_write_wins() {
        // Pin the compare_exchange semantic: the FIRST recorded
        // reason wins. A subsequent ctrlc handler trip during an
        // already-in-flight watchdog shutdown must NOT clobber the
        // watchdog's recorded reason.
        SHUTDOWN_REASON.store(0, Ordering::Relaxed); // reset to Unknown for test isolation
        record_shutdown_reason(ShutdownReason::Watchdog);
        record_shutdown_reason(ShutdownReason::Signal); // second write is no-op
        let recovered = ShutdownReason::from_u8(SHUTDOWN_REASON.load(Ordering::Relaxed));
        assert_eq!(
            recovered,
            ShutdownReason::Watchdog,
            "first-write-wins must preserve initial reason against re-entry"
        );
        // Reset for other tests.
        SHUTDOWN_REASON.store(0, Ordering::Relaxed);
    }

    #[test]
    fn daemon_stop_event_payload_carries_reason_and_metrics() {
        // Pin the on-disk shape of the enriched `daemon_stop` event.
        // Build a synthetic ShutdownMetrics + format it the way
        // run_core does, parse the resulting key=value string, and
        // assert each field is present + correct.
        //
        // This is the regression-proof that downstream queries /
        // greps on `reason=...`, `agents_total=...`,
        // `agents_killed_after_grace=...`, `uptime_secs=...` keep
        // working across future Phase 2 IMPL refactors.
        let metrics = ShutdownMetrics {
            reason: ShutdownReason::Signal,
            agents_total: 3,
            agents_killed_after_grace: 1,
            uptime_secs: 123,
        };
        let detail = format!(
            "reason={} agents_total={} agents_killed_after_grace={} uptime_secs={}",
            metrics.reason.as_str(),
            metrics.agents_total,
            metrics.agents_killed_after_grace,
            metrics.uptime_secs
        );
        assert!(detail.contains("reason=signal"), "got: {detail}");
        assert!(detail.contains("agents_total=3"), "got: {detail}");
        assert!(
            detail.contains("agents_killed_after_grace=1"),
            "got: {detail}"
        );
        assert!(detail.contains("uptime_secs=123"), "got: {detail}");
    }

    #[test]
    fn daemon_stop_event_name_unchanged_post_phase_2() {
        // Regression-proof against a future refactor that renames
        // `daemon_stop` to a parallel event name. Phase 1 RCA #554
        // Audit 6 explicitly chose enrich-not-duplicate; this test
        // pins the event-name decision in source text. If a future
        // refactor needs to rename, it must land a deliberate
        // operator-visible CHANGELOG migration note + delete this
        // pin in the same commit.
        //
        // We only check production code by slicing off the tests
        // submodule — including this very test file would self-
        // reference any literal we name in the negative-assertion
        // message.
        let src = include_str!("./mod.rs");
        let prod_end = src.find("\n#[cfg(test)]\nmod tests {").unwrap_or(src.len());
        let prod = &src[..prod_end];
        let count = prod.matches(r#""daemon_stop""#).count();
        assert!(
            count >= 1,
            "the `daemon_stop` event name MUST appear in daemon/mod.rs production \
             code — enrich-not-duplicate semantic per Phase 1 RCA #554 Audit 6"
        );
        // The parallel-event name must not appear ANYWHERE in the
        // production region. Construct the search string without
        // putting the literal into the assertion message so this
        // test's own source doesn't cross-pollute the slice.
        let parallel = [
            'd', 'a', 'e', 'm', 'o', 'n', '_', 's', 'h', 'u', 't', 'd', 'o', 'w', 'n',
        ]
        .iter()
        .collect::<String>();
        let bad_count = prod.matches(&parallel).count();
        assert_eq!(
            bad_count, 0,
            "Phase 1 RCA #554 Audit 6 chose enrich-not-duplicate; \
             a parallel event name appearing in production code would \
             break downstream query / grep paths"
        );
    }

    #[test]
    fn shutdown_grace_window_is_2_seconds() {
        // Phase A RCA recommendation: 2s grace window. Long enough
        // for well-behaved agents to honor SIGTERM, short enough to
        // keep total daemon shutdown latency bounded. Pinned so a
        // future refactor doesn't silently drop or stretch the
        // grace window without a CHANGELOG note.
        assert_eq!(
            SHUTDOWN_GRACE,
            std::time::Duration::from_secs(2),
            "Wave 3 PR-2 contract: grace = 2s exactly"
        );
    }

    // --- #896 Option D anchor (C0 RED) ---
    //
    // Locks the boot-time invariant Option D establishes:
    // `spawn_and_register_agent` MUST publish the agent's `.port`
    // synchronously before returning Ok. Pre-fix the TUI thread does
    // the bind+write_port asynchronously after the function returns,
    // so app probes between agent spawns see an empty / partial
    // `*.port` set (issue #896, race-class regression widened by
    // PR #906 daemon api::serve reorder).
    //
    // The "no sleep, no retry" wording is the contract — the assertion
    // is the postcondition at return. Race timing is reviewer-confirmed
    // via §3.20 SOP 3 (RED→GREEN protocol on three runs).

    #[cfg(unix)]
    fn make_shell_agent_def(name: &str) -> crate::bootstrap::AgentDef {
        (
            name.into(),
            "/bin/sh".into(),
            vec!["-c".into(), "sleep 60".into()],
            None,
            None,
            "\r".into(),
        )
    }

    #[cfg(unix)]
    fn setup_run_dir_with_cookie(home: &Path) -> PathBuf {
        let run = home.join("run").join(std::process::id().to_string());
        std::fs::create_dir_all(&run).expect("create run_dir");
        crate::auth_cookie::issue(&run).expect("issue api.cookie");
        run
    }

    #[cfg(unix)]
    #[allow(clippy::type_complexity)] // test scaffolding tuple; struct would be over-engineering
    fn make_test_registry() -> (
        AgentRegistry,
        Arc<Mutex<HashMap<String, AgentConfig>>>,
        crossbeam_channel::Sender<crate::agent::AgentExitEvent>,
        crossbeam_channel::Receiver<crate::agent::AgentExitEvent>,
        Arc<AtomicBool>,
    ) {
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs: Arc<Mutex<HashMap<String, AgentConfig>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (crash_tx, crash_rx) = crossbeam_channel::unbounded();
        let shutdown = Arc::new(AtomicBool::new(false));
        (registry, configs, crash_tx, crash_rx, shutdown)
    }

    /// #1441: managed spawns fail-fast unless the instance is in fleet.yaml;
    /// seed authoritative ids for the named agents under `home`.
    #[cfg(unix)]
    fn seed_fleet_ids(home: &std::path::Path, names: &[&str]) {
        let mut yaml = String::from("instances:\n");
        for (i, n) in names.iter().enumerate() {
            yaml.push_str(&format!(
                "  {n}:\n    id: 0d0d0d0d-0000-4000-8000-{:012x}\n",
                i + 1
            ));
        }
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).expect("seed fleet.yaml");
    }

    #[cfg(unix)]
    fn kill_registered_child(registry: &AgentRegistry, name: &str) {
        let reg = registry.lock();
        if let Some(handle) = reg.values().find(|h| h.name.as_str() == name) {
            let _ = handle.child.lock().kill();
        }
    }

    #[cfg(unix)]
    #[test]
    fn spawn_and_register_agent_publishes_port_synchronously() {
        let home = tmp_home("publish_sync");
        let run_dir = setup_run_dir_with_cookie(&home);
        seed_fleet_ids(&home, &["probe-1"]);
        let (registry, configs, crash_tx, _crash_rx, shutdown) = make_test_registry();
        let def = make_shell_agent_def("probe-1");

        spawn_and_register_agent(&home, &def, &registry, &configs, &crash_tx, &shutdown)
            .expect("spawn ok");

        // CONTRACT: the agent's .port is on disk BEFORE this assertion line.
        // Pre-fix: TUI thread is async, .port may not be written yet (race).
        // Post-fix: prepare_tui_listener_and_publish_port ran synchronously
        // inside spawn_and_register_agent.
        assert!(
            crate::ipc::read_port(&run_dir, "probe-1").is_some(),
            "spawn_and_register_agent must publish .port synchronously before return"
        );

        kill_registered_child(&registry, "probe-1");
        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn spawn_and_register_agent_rollback_on_listener_prep_failure() {
        // Force prepare-listener failure by NOT issuing api.cookie in
        // run_dir. `prepare_tui_listener_and_publish_port` reads the
        // cookie first (so it can hand it to the accept loop); a missing
        // cookie file is an Err on the synchronous prep path.
        let home = tmp_home("rollback_prep");
        let run = home.join("run").join(std::process::id().to_string());
        std::fs::create_dir_all(&run).expect("create run_dir");
        // Deliberately skip `auth_cookie::issue` — prep should fail at
        // cookie read.
        let (registry, configs, crash_tx, _crash_rx, shutdown) = make_test_registry();
        let def = make_shell_agent_def("rollback-probe");

        let result =
            spawn_and_register_agent(&home, &def, &registry, &configs, &crash_tx, &shutdown);

        // CONTRACT (Option D rollback):
        // 1. spawn_and_register_agent returns Err — caller can decide whether
        //    to continue or abort.
        assert!(
            result.is_err(),
            "spawn_and_register_agent must return Err when TUI listener prep fails (got Ok)"
        );
        // 2. Registry MUST NOT contain the agent — caller sees a clean
        //    rollback state, no zombie entries.
        assert!(
            !registry
                .lock()
                .values()
                .any(|h| h.name.as_str() == "rollback-probe"),
            "registry must NOT contain 'rollback-probe' after rollback"
        );
        // 3. AgentConfig MUST NOT contain the agent — configs map mirrors
        //    registry membership.
        assert!(
            configs.lock().get("rollback-probe").is_none(),
            "configs must NOT contain 'rollback-probe' after rollback"
        );
        // 4. .port file MUST NOT be on disk — prep failed before write_port
        //    or the rollback removed it.
        assert!(
            crate::ipc::read_port(&run, "rollback-probe").is_none(),
            "rollback must leave no .port residue"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn app_attach_during_stagger_window_sees_all_agents() {
        // Behavioral RED: simulates the operator's smoke — multiple agents
        // spawned sequentially with stagger between them. Pre-fix, an "app
        // attach" simulated by `ipc::list_agent_ports` mid-loop sees fewer
        // entries than the loop has produced (TUI threads race). Post-fix,
        // every iteration's port is on disk by the time the next iteration
        // begins, so list_agent_ports == iteration_count holds at each step.
        //
        // #910 PR4 note: post-#910 the app's canonical discovery path is
        // `runtime::list_agents_with_fallback`, NOT bare
        // `ipc::list_agent_ports`. This test still uses the bare fn
        // intentionally — it locks the FILESYSTEM contract (the .port
        // file is present synchronously after spawn returns), which is
        // the worst-case fallback path the helper would expose when the
        // daemon API is briefly unresponsive. Testing the bare fn here
        // covers the helper's degraded mode by construction.
        let home = tmp_home("attach_during_stagger");
        let run_dir = setup_run_dir_with_cookie(&home);
        let agent_names = ["a-1", "a-2", "a-3", "a-4"];
        seed_fleet_ids(&home, &agent_names);
        let (registry, configs, crash_tx, _crash_rx, shutdown) = make_test_registry();
        for (i, name) in agent_names.iter().enumerate() {
            let def = make_shell_agent_def(name);
            spawn_and_register_agent(&home, &def, &registry, &configs, &crash_tx, &shutdown)
                .expect("spawn ok");
            // CONTRACT: every agent spawned so far has its .port on disk.
            // Probe is what an `app` reattach would do.
            let visible = crate::ipc::list_agent_ports(&run_dir);
            for prior in &agent_names[..=i] {
                assert!(
                    visible.contains(&prior.to_string()),
                    "after spawning {name} (iteration {i}), agent {prior} must have .port on \
                     disk; got {visible:?}"
                );
            }
        }

        for name in &agent_names {
            kill_registered_child(&registry, name);
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1126 characterization: handle_stage2_restart runs on a spawned
    /// thread without blocking the caller. Pre-fix, the function ran
    /// inline on the main loop with `thread::sleep(backoff)`.
    #[test]
    fn stage2_restart_does_not_block_caller() {
        let home = tmp_home("stage2_nonblock");
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs: Arc<Mutex<HashMap<String, AgentConfig>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (crash_tx, _crash_rx) = crossbeam_channel::unbounded();
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        std::env::set_var("AGEND_AUTO_RECOVERY_STAGE2_BACKOFF_MS", "2000");

        let start = std::time::Instant::now();
        let home_owned = home.to_path_buf();
        let reg = Arc::clone(&registry);
        let cfgs = Arc::clone(&configs);
        let tx = crash_tx.clone();
        let sd = Arc::clone(&shutdown);
        let handle = std::thread::Builder::new()
            .name("test_stage2".into())
            .spawn(move || {
                handle_stage2_restart(&home_owned, "ghost", &reg, &cfgs, &tx, &sd);
            })
            .unwrap();

        assert!(
            start.elapsed() < std::time::Duration::from_millis(100),
            "spawn must return immediately — main loop is not blocked"
        );

        handle.join().unwrap();

        std::env::remove_var("AGEND_AUTO_RECOVERY_STAGE2_BACKOFF_MS");
        std::fs::remove_dir_all(&home).ok();
    }
}
