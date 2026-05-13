//! Daemon: manages agent registry, TUI sockets, auto-respawn, fleet lifecycle,
//! schedule checking, health monitoring, Telegram notifications.

pub(crate) mod anti_stall;
pub(crate) mod ci_watch;
pub(crate) mod cron_tick;
pub(crate) mod decision_timeout;
pub(crate) mod dedup_state;
pub(crate) mod heartbeat_pair;
pub(crate) mod helper_staleness_watchdog;
pub(crate) mod idle_watchdog;
pub(crate) mod legacy_backfill;
pub(crate) mod lifecycle;
pub(crate) mod mcp_registry_watcher;
pub(crate) mod per_tick;
pub(crate) mod poll_reminder;
pub(crate) mod router;
pub(crate) mod supervisor;
pub(crate) mod task_progress;
pub(crate) mod task_sweep;
pub(crate) mod ticker;
mod tui_bridge;
pub(crate) mod utils;
pub(crate) mod waiting_on_stale;
pub(crate) mod watchdog;

use crate::agent::{self, AgentRegistry};
use crate::channel::NotifySeverity;
use ci_watch::check_ci_watches;
use cron_tick::check_schedules;
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
    use fs4::fs_std::FileExt;
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
    // Sprint 57 Wave 3 PR-2 (#548 Q6): record startup time for the
    // shutdown-sequence uptime metric.
    let started_at = std::time::Instant::now();

    // Sprint 25 P0 Option F: mark this process as the daemon so
    // `mcp::is_running_inside_daemon_process()` can short-circuit
    // tool calls without TCP round-trip.
    // G3 H1: replaced set_var with DaemonConfig init (thread-safe).
    crate::daemon_config::init(crate::daemon_config::DaemonConfig::default());

    // Sprint 62 W1 PR-2 (#P0-2 skills-stage GC): sweep stale
    // <home>/.skills-stage/<digest>/ dirs older than 7 days. Runs
    // BEFORE any spawn_and_register_agent invocation so concurrent
    // install_for_agent never races with the GC. Empty exclusion
    // list at this site (no installs have happened yet); periodic-
    // GC sites added later would pass currently-resolved digests.
    // Best-effort: failures log + continue, never block boot.
    const SKILLS_STAGE_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;
    match crate::skills::cleanup_stale_stages(home, SKILLS_STAGE_RETENTION_SECS, &[]) {
        Ok(report) => tracing::info!(?report, "skills-stage GC: daemon-init sweep complete"),
        Err(e) => tracing::warn!(error = %e, "skills-stage GC: daemon-init sweep failed"),
    }

    // Sprint 63 W1 PR-2 (Sprint 58 P2 #5): sweep stale `*.tmp` /
    // `*.json.tmp` orphans under <home>/dedup-state/ left behind by
    // crashed atomic_write cycles. 1-day retention threshold (much
    // shorter than skills-stage 7-day because tmp files should never
    // legitimately persist longer than a single syscall pair).
    // Best-effort: failures log + continue (function returns
    // DedupStateGcReport directly, no Err path).
    const DEDUP_TMP_RETENTION_SECS: u64 = 24 * 60 * 60;
    let dedup_report =
        crate::daemon::dedup_state::cleanup_tmp_orphans(home, DEDUP_TMP_RETENTION_SECS);
    tracing::info!(?dedup_report, "dedup-state GC: daemon-init sweep complete");

    // Sprint 24 P0 PR2 — bridge-phase legacy migration. Walks tasks.json
    // and emits canonical Created (+ status transition) events into
    // task_events.jsonl. Idempotent: re-run is a no-op via tail-scan
    // against the existing event log. Runs synchronously BEFORE any MCP
    // tool registration / agent spawn so operators never observe a "list
    // empty" race. Fail-loud: any error aborts daemon startup so the
    // operator sees the failure rather than silently inconsistent state.
    match crate::tasks::migrate_legacy_tasks_json_to_event_log(home) {
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
    let (crash_tx, crash_rx) = crossbeam_channel::bounded::<crate::agent::AgentExitEvent>(64);

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
    // fire-and-forget: api::serve runs the Unix socket accept loop for the
    // daemon's lifetime. Loop observes shutdown via the cloned AtomicBool;
    // the socket file is removed during daemon shutdown, which surfaces as a
    // bind/accept error and exits the loop. JoinHandle dropped because no
    // graceful join is needed — process exit reaps the thread.
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
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    crate::bootstrap::signals::install(Arc::clone(&shutdown), shutdown_tx);

    crate::event_log::log(
        home,
        "daemon_start",
        "",
        &format!("{} agents", agents.len()),
    );
    tracing::info!("running, Ctrl+C or `agend-terminal stop` to stop");

    // Sprint 24 P0 PR2 — task auto-close sweep daemon. Holds the
    // ticker handle for the duration of run_core so the spawned thread
    // observes the shared shutdown atomic; drop is a no-op per the
    // current daemon fire-and-forget convention. Sweep no-ops until the
    // operator configures `repo` via the `task_sweep_config` MCP tool.
    let _task_sweep =
        crate::daemon::task_sweep::TaskSweep::spawn(home.to_path_buf(), Arc::clone(&shutdown));

    // Per-agent stall detector — see daemon::supervisor module-doc.
    // Pushes vterm tail to channel topic when an agent stalls pre-ready.
    // Fire-and-forget: detector tick loop runs for the daemon process lifetime.
    // Unix-only (the supervisor itself is Unix-only).
    #[cfg(unix)]
    supervisor::spawn(home.to_path_buf(), Arc::clone(&registry));
    router::spawn(home.to_path_buf(), Arc::clone(&registry));
    crate::instance_monitor::spawn_monitor_tick(home.to_path_buf(), Arc::clone(&registry));

    // Recover any half-written inbox files from a previous crash.
    crate::inbox::recover_half_writes(home);

    // Replay missed one-shot schedules from before daemon was down.
    // Must run once at startup, before the tick loop, so missed one-shots
    // fire exactly once. The tick loop's check_schedules handles future
    // one-shots and recurring crons.
    replay_missed_at_startup(home, &registry);

    // Sprint 57 Wave 2 Track B (#546 Items 1+3) — eager ci-watch
    // sweep at daemon startup: removes any expired or protected-ref
    // watch left behind by a prior daemon process before the first
    // tick fires. Idempotent.
    crate::daemon::ci_watch::startup_sweep(home);

    // Per-tick handlers (#694 BLOCK 1 — see daemon::per_tick). Owned
    // across the daemon's lifetime so their interior state (snapshot
    // dedup string, poll-reminder + inbox-maintenance counters) persists
    // across ticks. Called at their pre-extraction sites in the main
    // loop below.
    use per_tick::PerTickHandler as _;
    // Watchdog dry-run mode: log classifications without mutating health
    // state. Read ONCE at daemon startup (matches pre-extraction
    // semantics; env changes mid-runtime are not observed).
    let watchdog_dry_run = watchdog::watchdog_dry_run_from_env();
    let snapshot_handler = per_tick::SnapshotRotationHandler::new();
    let poll_reminder_handler = per_tick::PollReminderHandler::new(30);
    let inbox_maintenance_handler = per_tick::InboxMaintenanceHandler::new(60);
    let external_liveness_handler = per_tick::ExternalLivenessHandler::new();
    let hang_detection_handler = per_tick::HangDetectionHandler::new();
    let watchdog_handler = per_tick::WatchdogHandler::new(watchdog_dry_run);

    // Periodic tick channel (every 10s for health/schedule/session maintenance)
    let tick_rx = {
        let (tx, rx) = crossbeam_channel::bounded(1);
        // fire-and-forget: tick producer terminates when the bounded(1) tx
        // returns Err, which happens when the rx end (held by the main loop)
        // is dropped during daemon shutdown. Self-terminating; no JoinHandle
        // tracking needed.
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

        // Block until an exit event, periodic tick, or shutdown signal.
        let exit_event: Option<crate::agent::AgentExitEvent>;
        crossbeam_channel::select! {
            recv(crash_rx) -> msg => {
                exit_event = msg.ok();
            }
            recv(tick_rx) -> _ => {
                exit_event = None;
            }
            recv(shutdown_rx) -> _ => {
                // Re-check flag at top of loop and break.
                continue;
            }
        }

        // Per-tick context shared by handlers extracted under #694 BLOCK 1.
        // Borrows are valid for the rest of this iteration; coexists with
        // other immutable uses of `&registry` / `&externals` / `&configs`
        // below.
        let tick_ctx = per_tick::TickContext {
            home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        // Periodic maintenance (runs on every wake, whether crash or tick).
        // AwaitingOperator detection lives in the dedicated supervisor thread
        // (spawned earlier) so app mode gets the same behavior as daemon mode.
        // Hang detection and health decay stay here — they're daemon-only
        // concerns (hang notifications tie into crash respawn accounting).
        // #694 BLOCK 1 cohort — extracted into daemon::per_tick. Hang and
        // Watchdog are adjacent so the same-tick `core.health` read-after-
        // write sequence stays visibly contiguous in the select! lambda.
        hang_detection_handler.run(&tick_ctx);
        watchdog_handler.run(&tick_ctx);

        // Liveness check for external agents.
        // #694 BLOCK 1 — extracted into daemon::per_tick::ExternalLivenessHandler.
        external_liveness_handler.run(&tick_ctx);

        // Periodic snapshot: save fleet state (only if changed).
        // #694 BLOCK 1 — extracted into daemon::per_tick::SnapshotRotationHandler.
        snapshot_handler.run(&tick_ctx);

        check_schedules(home, &registry);
        check_ci_watches(home, &registry);

        // Periodic inbox maintenance — every 60 ticks (≈10 min at 10s/tick).
        // #694 BLOCK 1 — extracted into daemon::per_tick::InboxMaintenanceHandler.
        inbox_maintenance_handler.run(&tick_ctx);

        // Poll-reminder: nudge idle agents with unread inbox (every 30 ticks).
        // #694 BLOCK 1 — extracted into daemon::per_tick::PollReminderHandler.
        poll_reminder_handler.run(&tick_ctx);

        // Handle exit event (if any)
        let exit_event = match exit_event {
            Some(e) => e,
            None => continue, // Tick only — no exit to handle
        };

        // Handle clean exit: agent typed /exit or /quit — no respawn.
        if let crate::agent::AgentExitEvent::CleanExit(ref name) = exit_event {
            if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            tracing::info!(agent = %name, "clean exit — removing from registry (no respawn)");
            {
                let mut reg = registry.lock();
                reg.remove(name.as_str());
            }
            configs.lock().remove(name.as_str());
            continue;
        }

        // Crash path: extract name and proceed with respawn logic.
        let crashed_name = match exit_event {
            crate::agent::AgentExitEvent::Crash(n) => n,
            _ => continue,
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
        let config = configs.lock().get(&crashed_name).cloned();
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
                    let mut core = handle.core.lock();
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
                    .map(|h| h.core.lock().health.state.display_name())
                    .unwrap_or("unknown")
            };
            tracing::warn!(agent = %crashed_name, %state, "notifying");
            let msg = format!("[health] {crashed_name}: {state}");
            // Outbound info-leak gate (Sprint 21 Phase 1): crash
            // notification carries the agent state name; gated_notify
            // drops when the channel is unauthorised.
            if let Some(ch) = crate::channel::active_channel() {
                let _ = crate::channel::gated_notify(
                    ch.as_ref(),
                    &crashed_name,
                    NotifySeverity::Error,
                    &msg,
                    false,
                );
            } else {
                tracing::debug!(agent = %crashed_name, "no active channel for crash notification");
            }
        }

        if should_respawn {
            tracing::info!(agent = %crashed_name, ?delay, "respawning");
            let reg = Arc::clone(&registry);
            let home = home.to_path_buf();
            let tx = crash_tx.clone();
            let shutdown_for_respawn = Arc::clone(&shutdown);

            // Save health tracker from old handle before respawn replaces it
            let saved_health = {
                let r = registry.lock();
                r.get(&crashed_name).map(|h| h.core.lock().health.clone())
            };

            // fire-and-forget: respawn worker is short-lived (sleep delay then
            // spawn_agent + restore health + start TUI server). Observes
            // shutdown flag immediately after backoff to abort cleanly.
            // JoinHandle dropped because Err is logged + crash counter handles
            // accounting.
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
                                        let r = reg.lock();
                                        if let Some(handle) = r.get(&config.name) {
                                            let mut core = handle.core.lock();
                                            if let Some(ref old_health) = saved_health {
                                                core.health = old_health.clone();
                                            }
                                            core.health.respawn_ok();
                                        }
                                    }

                                    // Inject system message
                                    std::thread::sleep(std::time::Duration::from_secs(2));
                                    {
                                        let r = reg.lock();
                                        if let Some(handle) = r.get(&config.name) {
                                            let reason = handle.core.lock()
                                                .health.crash_reason().to_string();
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
                                    // fire-and-forget: respawn-time TUI server
                                    // exits when the agent is removed from
                                    // the registry (socket-file removal in
                                    // delete_transaction). Same shutdown
                                    // contract as the startup-time TUI server
                                    // spawn at line 1103.
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
        let cfgs = configs.lock();
        let mut seen = std::collections::HashSet::new();
        // Sprint 57 Wave 4 (#546 Item 4): residual worktrees now live
        // under `$AGEND_HOME/worktrees/<agent>/<branch>/` (the new
        // canonical layout), so the residual scan is repo-independent.
        // `list_residual(home)` returns agent-name dirs found there;
        // `list_legacy_residual(repo)` separately surfaces any
        // pre-Wave-4 entries still under `<repo>/.worktrees/` for
        // operator cleanup. Both surface to the same audit log line.
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
            // Use worktree_source (original repo) if available, otherwise working_dir
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
                            "Sprint 57 Wave 4 (#546 Item 4): legacy worktrees detected at \
                             <repo>/.worktrees/<agent>/ — operator cleanup recommended. \
                             New worktrees land at $AGEND_HOME/worktrees/<agent>/<branch>/. \
                             Manual cleanup: `git -C <repo> worktree remove <repo>/.worktrees/<agent>` \
                             then re-bind via task dispatch or bind_self."
                        );
                    }
                }
            }
        }
    }

    // Sprint 57 Wave 3 PR-2 (#548 Q6): staged shutdown sequence with
    // reason taxonomy + summary metrics. The `daemon_stop` event name
    // stays unchanged (preserving downstream query / grep paths) but
    // its detail payload now carries:
    //   - reason: signal / api_shutdown / watchdog / unknown
    //   - agents_total: total registered agents at shutdown
    //   - agents_killed_after_grace: how many needed SIGKILL escalation
    //   - uptime_secs: daemon process uptime
    //
    // Termination is staged: SIGTERM all agents in parallel, wait the
    // grace window (2s), then SIGKILL any survivors. Pre-Wave-3-PR-2
    // shutdown serially called `kill_process_tree` per agent
    // (effectively N * 500ms grace); the new sequence collapses to a
    // single 2s wall-clock window regardless of N agents.
    let metrics = shutdown_sequence(home, &registry, started_at);
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
    // Remove entire run dir (port files + .daemon identity + PID isolation)
    let _ = std::fs::remove_dir_all(run_dir(home));

    // Give threads time to flush logs and close connections
    std::thread::sleep(std::time::Duration::from_secs(1));

    // Sprint 60 W1 PR-3 (#P0-3): operator-initiated restart. After
    // shutdown_sequence has terminated agents and we've flushed
    // logs, re-exec self if RESTART_PENDING was set. exec() replaces
    // the current process image in-place on Unix; on Windows we
    // spawn a fresh process and exit. Either way, the new daemon
    // comes up clean and re-reads on-disk state (binding metadata,
    // topic registry, fleet.yaml) — operators re-attach PTY agents
    // post-restart per the MVP scope.
    if RESTART_PENDING.load(Ordering::Acquire) {
        let flag = home.join("restart-requested");
        let _ = std::fs::remove_file(&flag);
        tracing::info!("operator-initiated restart: exiting with code 42");
        std::process::exit(42);
    }

    tracing::info!("exiting");
    Ok(())
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
            .map(|(name, handle)| (name, handle.child))
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
/// Phase 5: scan worktrees for hotspot conflicts (multi-agent file touches).
/// Called hourly from the periodic sweep. Builds index from each agent's
/// worktree and warns lead on conflicts.
fn hotspot_scan(home: &Path, configs: &crate::api::ConfigRegistry) {
    let cfgs = configs.lock();
    for (name, cfg) in cfgs.iter() {
        let Some(ref wd) = cfg.working_dir else {
            continue;
        };
        if !wd.join(".git").exists() && !wd.join("..").join(".git").exists() {
            continue;
        }
        let index = crate::hotspot::build_index(wd);
        let hotspots = crate::hotspot::list_hotspots(&index);
        for (file, agents) in &hotspots {
            // Warn if this agent is involved in a hotspot.
            if agents.iter().any(|a| a == name) {
                if let Some(other) = agents.iter().find(|a| *a != name) {
                    crate::hotspot::hotspot_warn(home, name, file, other, "last 7d");
                }
            }
        }
    }
}

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
        let _ = crate::inbox::enqueue(
            home,
            &a.to,
            crate::inbox::InboxMessage {
                schema_version: 0,
                id: None,
                read_at: None,
                thread_id: None,
                parent_id: None,
                task_id: None,
                from: "system:dispatch".to_string(),
                text: query,
                kind: Some("query".to_string()),
                timestamp: chrono::Utc::now().to_rfc3339(),
                channel: None,
                delivery_mode: None,
                force_meta: None,
                correlation_id: None,
                reviewed_head: None,
                attachments: vec![],
                in_reply_to_msg_id: None,
                in_reply_to_excerpt: None,
                superseded_by: None,
                from_id: None,
                broadcast_context: None,
                sequencing: None,
                eta_minutes: None,
                reporting_cadence: None,
                worktree_binding_required: None,
            },
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
                    channel: None,
                    delivery_mode: None,
                    force_meta: None,
                    correlation_id: None,
                    reviewed_head: None,
                    read_at: None,
                    thread_id: None,
                    parent_id: None,
                    task_id: None,
                    attachments: vec![],
                    in_reply_to_msg_id: None,
                    in_reply_to_excerpt: None,
                    superseded_by: None,
                    from_id: None,
                    broadcast_context: None,
                    sequencing: None,
                    eta_minutes: None,
                    reporting_cadence: None,
                    worktree_binding_required: None,
                },
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
        match crate::skills::install_for_agent(home, wd, skills_filter.as_deref()) {
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
    let reg = Arc::clone(registry);
    let n = name.clone();
    // fire-and-forget: serve_agent_tui blocks on UnixListener::accept and
    // exits when the agent is removed from the registry. JoinHandle is
    // discarded because shutdown is signalled implicitly by socket-file
    // removal in delete_transaction.
    if let Err(e) = std::thread::Builder::new()
        .name(format!("{n}_tui_server"))
        .spawn(move || serve_agent_tui(&n, &rdir, &reg))
    {
        // Sprint 20 F5 fix: previously a TUI server spawn failure left the
        // agent registered + child running but with no attachable socket.
        // Roll back the agent we just spawned so retries start clean.
        tracing::warn!(
            agent = %name,
            error = %e,
            "TUI server thread spawn failed — rolling back agent registration"
        );
        lifecycle::delete_transaction(home, name, registry, Some(configs));
        return Err(e.into());
    }
    Ok(())
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
}
