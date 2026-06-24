//! Daemon: manages agent registry, TUI sockets, auto-respawn, fleet lifecycle,
//! schedule checking, health monitoring, Telegram notifications.

pub(crate) mod anti_stall;
pub(crate) mod auto_release;
pub(crate) mod boot_sweep;
pub(crate) mod cadence_gate;
pub(crate) mod canonical_drift;
pub(crate) mod ci_handoff_track;
pub(crate) mod ci_watch;
pub(crate) mod conflict_notify;
mod crash_respawn;
pub(crate) mod cron_tick;
pub(crate) mod decision_timeout;
pub(crate) mod dedup_state;
pub(crate) mod dispatch_idle;
pub(crate) mod escalation_persist;
pub(crate) mod event_bus;
pub(crate) mod handoff_timeout_watchdog;
pub(crate) mod heartbeat_pair;
pub(crate) mod helper_staleness_watchdog;
pub mod hook_shadow;
pub(crate) mod idle_watchdog;
pub(crate) mod inbox_stuck_watchdog;
pub(crate) mod inject_delivery;
pub(crate) mod lifecycle;
pub(crate) mod mcp_registry_watcher;
pub(crate) mod notification_dedup;
pub(crate) mod orphan_sweep;
pub(crate) mod per_tick;
pub(crate) mod poll_reminder;
pub(crate) mod pr_state;
pub(crate) mod recovery_shadow;
pub(crate) mod restart;
pub(crate) mod retention;
pub(crate) mod router;
/// #2413 Shadow Observer — local plane (claude hooks side-channel). Spike, flag-OFF.
pub mod shadow;
pub(crate) mod supervisor;
pub(crate) mod task_progress;
pub(crate) mod task_sweep;
pub(crate) mod ticker;
pub(crate) mod transcript_tail;
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

/// #2098: positive "this process's `run_core` loop is the active restart
/// consumer" marker. Set true at `run_core` entry; stays false in every other
/// mode — notably `agend-terminal app` (combined TUI+daemon, `run_app`), which
/// brings up the api server under an `OwnedFleet`/`ApiGuard` but NEVER enters
/// `run_core`. Only `run_core` consumes `RESTART_PENDING` (the tail
/// `confirm_shutdown_or_abort_respawn` + post-serve exit/promote); app mode has
/// NO consumer, so setting `RESTART_PENDING` there bricks the control plane
/// permanently (api/mod.rs breaks every session, the held-flock successor
/// 30s-times-out and dies, the flag stays latched). The `restart_daemon` MCP
/// handler reads this fail-SAFE: it proceeds with the restart machinery ONLY
/// when run_core is the active loop, and fail-closes otherwise (default-deny —
/// any future non-run_core mode is blocked without code change). See
/// `mcp::handlers::restart::handle_restart_daemon`.
pub(crate) static RUN_CORE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// #1814 FIX2 (reviewer race High): the spawned successor's child handle, parked
/// by `handle_self_respawn` at commit so the run_core loop can do a FINAL
/// liveness recheck (`try_wait`, which also reaps) before the irreversible
/// teardown. Phase-1 only proves the successor was healthy at probe time; it has
/// not yet acquired the flock / spawned agents. If it dies in the commit→exit
/// window, exiting anyway would brick the control plane — so the loop aborts
/// (clears RESTART_PENDING, stays alive) instead. (Residual: a successor that
/// dies AFTER the predecessor has already exited needs an external supervisor —
/// the d-2 step-6 accepted residual, not closable here.)
static SELF_RESPAWN_SUCCESSOR: std::sync::Mutex<Option<std::process::Child>> =
    std::sync::Mutex::new(None);

/// #1814 FIX2: park the successor child handle for the pre-exit liveness recheck.
pub(crate) fn park_self_respawn_successor(child: std::process::Child) {
    *SELF_RESPAWN_SUCCESSOR
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(child);
}

/// #1814 FIX2: true iff a parked successor has already exited (`try_wait` reaps
/// it). `None` parked → false (no self-respawn in flight).
fn self_respawn_successor_died() -> bool {
    let mut guard = SELF_RESPAWN_SUCCESSOR
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match guard.as_mut() {
        Some(child) => matches!(child.try_wait(), Ok(Some(_))),
        None => false,
    }
}

/// #1814 FIX2: when the shutdown flag is set, decide whether to truly tear down.
/// For a self-respawn restart, do a final successor-liveness recheck; if the
/// successor died after commit, ABORT-STAY-ALIVE (clear the flags, drop the
/// dead handle, keep serving). Returns `true` → break the loop (teardown);
/// `false` → keep running. (Residual race: an in-flight api session could
/// re-set the shutdown flag from a stale RESTART_PENDING read after we clear it
/// → the next iteration would then exit; this is no worse than pre-FIX2 and the
/// window is vanishingly small.)
fn confirm_shutdown_or_abort_respawn(shutdown: &AtomicBool) -> bool {
    if !RESTART_PENDING.load(Ordering::Acquire) || !crate::daemon::restart::self_respawn_enabled() {
        return true;
    }
    if self_respawn_successor_died() {
        tracing::error!(
            target: "handoff",
            event = "abort_stay_alive",
            "#1814 self-respawn: successor died after commit but before predecessor exit — \
             ABORTING restart, staying alive (no brick). Operator may retry restart_daemon."
        );
        RESTART_PENDING.store(false, Ordering::Release);
        shutdown.store(false, Ordering::Relaxed);
        *SELF_RESPAWN_SUCCESSOR
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        return false;
    }
    true
}

/// #1814 round-2: settle window before the FINAL pre-exit liveness recheck (the
/// recover-as-primary gate in `run_core`). Gives a successor that is crashing
/// around the predecessor's teardown a moment to surface so the recheck catches
/// it. Production is ALWAYS 1s.
///
/// `AGEND_SELF_RESPAWN_SETTLE_SECS` is a **test-only seam, NOT a production
/// tunable** (same convention as `AGEND_FORCE_SUCCESSOR_FAIL*`): it exists only
/// so the cross-process integration tests can widen the window deterministically
/// (the successor's death must land inside it). Operators should never set it —
/// it is intentionally absent from the operator-facing tuning docs.
fn self_respawn_settle() -> std::time::Duration {
    // Test-only override (see doc above); unset/garbage → the 1s prod default.
    let secs = crate::env_util::env_parse::<u64>("AGEND_SELF_RESPAWN_SETTLE_SECS", 1);
    std::time::Duration::from_secs(secs)
}

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
    run_dir_for_pid(home, std::process::id())
}

/// Run dir for an arbitrary daemon `pid` (`home/run/<pid>`). #1814: the
/// self-respawn Phase-1 gate needs the SUCCESSOR's run dir (a different pid),
/// which `run_dir` — pinned to the current process — can't give.
pub fn run_dir_for_pid(home: &Path, pid: u32) -> PathBuf {
    home.join("run").join(pid.to_string())
}

/// #1812: the SINGLE process-wide lock that every test mutating (or
/// reading) process-global env must hold.
///
/// `std::env::set_var` / `remove_var` / `var` race across the WHOLE
/// environment, not per key — the libc `environ` is one shared,
/// non-atomic array (which is exactly why Rust 1.84 made the mutators
/// `unsafe`). So two tests guarding DIFFERENT keys with DIFFERENT
/// per-module mutexes still data-race each other. A reviewer caught this
/// when `cargo test restart` interleaved `daemon::restart` and
/// `per_tick::recovery_dispatcher` env tests under their separate locks.
/// Cross-module env tests must lock THIS, not a local static.
#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static std::sync::Mutex<()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
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
                // Format: "pid:boot_unix:start_token" (third field appended
                // CR-2026-06-14; legacy files have only "pid:boot_unix").
                let mut fields = content.trim().split(':');
                if let Some(file_pid) = fields.next() {
                    if file_pid == pid_str {
                        // DP5: PID matches, but a recycled PID can land on the
                        // SAME number. If the `.daemon` recorded a start-token
                        // AND the live process's token can be read, a mismatch
                        // means this is a different process wearing the old PID
                        // → PID reused. (Legacy no-token or unreadable token →
                        // fall back to PID-only accept for back-compat.)
                        let recorded_token = fields.nth(1).and_then(|t| t.parse::<u64>().ok());
                        if let (Some(rec), Some(cur)) =
                            (recorded_token, crate::process::process_start_token(pid))
                        {
                            if rec != cur {
                                tracing::info!(
                                    pid,
                                    recorded_token = rec,
                                    current_token = cur,
                                    "PID reused (start-token mismatch), cleaning"
                                );
                                let _ = std::fs::remove_dir_all(entry.path());
                                continue;
                            }
                        }
                        return Some(entry.path());
                    }
                    // PID alive but .daemon file has different PID → PID was reused
                    tracing::info!(pid, old_pid = file_pid, "PID reused, cleaning");
                    let _ = std::fs::remove_dir_all(entry.path());
                    continue;
                }
            }
            // No (valid) `.daemon` identity file but PID alive → NOT discoverable.
            // #1814 (reviewer race High): a handoff successor publishes its
            // run dir + api.port pre-flock (so the predecessor can Phase-1-probe
            // it by name via `connect_run_dir_api`) but writes `.daemon` only
            // AFTER it acquires the flock (promotes). Skipping un-`.daemon`'d
            // dirs here keeps a half-promoted successor invisible to generic
            // discovery during the overlap window — generic clients route only
            // to the fully-promoted daemon (single-primary-lease invariant). A
            // normal daemon writes `.daemon` at boot (microsecond gap), so this
            // never hides a real primary. (Pre-#1814 this fell through to
            // "accept it" for a since-extinct legacy/no-`.daemon` daemon class.)
            tracing::debug!(
                path = %entry.path().display(),
                "#1814: run dir has no `.daemon` identity (pre-promote successor or mid-boot) — not discoverable yet"
            );
            continue;
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
///
/// Format: `{pid}:{boot_unix}:{start_token}`. The third field
/// (CR-2026-06-14 zombie-kill identity-compare) is the OS process start-time
/// token (see [`crate::process::process_start_token`]) so a stale `.daemon`
/// whose PID got recycled onto an unrelated process is detectable: the
/// recorded token won't match the live process's. Appended (not inserted) so
/// the existing first/second-field readers keep working. `0` when the
/// self-token can't be resolved — a recorded `0` will never match a real
/// live token, so the conservative outcome is fail-closed (never signal),
/// which is the safe direction.
pub(crate) fn write_daemon_id(run_dir: &Path) {
    let pid = std::process::id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let token = crate::process::process_start_token(pid).unwrap_or(0);
    // A1: atomic write — a plain `fs::write` can be read mid-write (torn) by a
    // concurrent `read_daemon_pid`/liveness probe, which then parse-fails on a
    // truncated `pid:now:token` record. `store::atomic_write` publishes via a
    // unique tmp + rename so readers only ever see a complete record.
    let _ = crate::store::atomic_write(
        &run_dir.join(".daemon"),
        format!("{pid}:{now}:{token}").as_bytes(),
    );
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

/// Read the boot epoch (unix seconds) recorded in `{run_dir}/.daemon`
/// (`{pid}:{boot_unix}:{start_token}`). Used by the worktree force-reclaim
/// boot-grace (reviewer-2 #5). `None` if the file is missing or malformed.
///
/// CR-2026-06-14: splits on `:` and takes field index 1 rather than
/// `split_once(':').1` — the latter would capture `"{boot_unix}:{start_token}"`
/// once the third field was appended and fail to parse as a u64.
pub(crate) fn read_daemon_boot_unix(run_dir: &Path) -> Option<u64> {
    std::fs::read_to_string(run_dir.join(".daemon"))
        .ok()?
        .trim()
        .split(':')
        .nth(1)
        .and_then(|ts| ts.parse().ok())
}

/// Read the OS process start-time token recorded in `{run_dir}/.daemon`
/// (`{pid}:{boot_unix}:{start_token}`, field index 2). `None` if the file is
/// missing, malformed, or written by a pre-CR-2026-06-14 daemon (no third
/// field) — callers MUST treat `None` as "identity unverifiable → fail
/// closed" per the zombie-kill identity-compare design.
pub(crate) fn read_daemon_start_token(run_dir: &Path) -> Option<u64> {
    std::fs::read_to_string(run_dir.join(".daemon"))
        .ok()?
        .trim()
        .split(':')
        .nth(2)
        .and_then(|t| t.parse().ok())
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

    run_core(
        home,
        FleetSource::Resolved {
            agents,
            telegram: None,
        },
    )
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
    // #1688: intentionally NO startup binding re-sign pass — see `binding.rs`.
    // It was a wash-white hole: "no sidecar" can't distinguish a legit unsigned
    // binding from a tampered-then-sidecar-deleted one, and the daemon has no
    // trusted source at startup to tell them apart. Unsigned bindings fail closed
    // (unbound) and re-sign on their next dispatch / bind_self.
    let _owned = prepared;
    run_core(&home, FleetSource::Resolved { agents, telegram })
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
/// #event-bus: register every per-pattern delivery subscriber on the
/// process-global bus. Post-cutover (#1719 legacy-zero) the bus is the SOLE
/// delivery path, so this MUST run in every mode that ticks producers.
///
/// Called by BOTH `run_core` (headless daemon mode) AND `app::run_app` (owned
/// `agend-terminal app` mode). The latter never calls `run_core` — so before
/// this was shared, app mode registered NOTHING and every emit (cron fire,
/// idle nudge, ci-ready handoff, …) silently dropped. That was the live
/// silent-drop behind #1720/#1723, and the same regression class as #1002
/// (`pr_state`) / #982 (idle notifications): "wired only in run_core, broke in
/// app mode". The test harness (`event_bus::register_all_subscribers_for_test`)
/// routes through this SAME fn — ONE subscriber list, so test wiring can never
/// drift from live wiring again (the drift that masked this bug). Each
/// subscriber is home-agnostic (the home travels on every event); cron captures
/// the live `registry` to resolve + inject to the fleet.
pub(crate) fn register_event_subscribers(registry: &AgentRegistry) {
    crate::daemon::anti_stall::register_subscriber();
    crate::daemon::decision_timeout::register_subscriber();
    crate::daemon::dispatch_idle::register_subscriber();
    crate::daemon::waiting_on_stale::register_subscriber();
    crate::daemon::helper_staleness_watchdog::register_subscriber();
    crate::daemon::idle_watchdog::register_subscriber();
    crate::tasks::register_cascade_subscriber();
    crate::daemon::poll_reminder::register_subscriber();
    crate::daemon::cron_tick::register_subscriber(registry.clone());
    crate::daemon::supervisor::register_subscriber();
    crate::daemon::conflict_notify::register_subscriber();
    crate::daemon::ci_watch::register_subscriber();
}

/// Build the canonical per-tick handler pipeline. Shared by `run_core` (daemon)
/// and `app::run_app` (owned `agend-terminal app`) so both run the IDENTICAL set
/// — the single source of truth that closes the recurring "app hand-picks a
/// subset → silently drops a handler" class (#1002 / #982 / #1719). App filters
/// only an explicit allowlist (see `app::APP_TICK_ALLOWLIST`), and a completeness
/// invariant fails CI if a new handler lands here but neither runs in app nor is
/// allowlisted.
///
/// `crash_tx` is consumed by `RecoveryDispatcherHandler`. `stage2_dispatch_available`
/// tells the handler whether a `Stage2Restart` on that channel has a live consumer
/// in this runtime: `run_core` wires `crash_rx` (true); app-standalone passes a
/// throwaway sender (false) — #1694(a) now RUNS the handler in app mode (Stage1
/// ESC-nudge), but its Stage2 path escalates to Stage3 rather than silent-drop
/// onto the consumerless channel.
pub(crate) fn build_default_handlers(
    crash_tx: crossbeam_channel::Sender<crate::agent::AgentExitEvent>,
    stage2_dispatch_available: bool,
    daemon_binary_stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale,
) -> Vec<Box<dyn per_tick::PerTickHandler>> {
    let watchdog_dry_run = watchdog::watchdog_dry_run_from_env();
    // #2127 Phase 1: the inbox-stuck handler and the new reclaim handler share one
    // dedup latch so reclaim can clear an agent's repeat-alert entry. Construct the
    // inbox-stuck handler first, clone its latch, then move it into the vec at its
    // original position (order preserved).
    let inbox_stuck = per_tick::InboxStuckHandler::new(30);
    let work_stuck_latch = inbox_stuck.latch();
    // Vec order MUST match the pre-extraction call order (zero-behavior-change guarantee).
    vec![
        Box::new(per_tick::HangDetectionHandler::new()),
        Box::new(per_tick::RecoveryDispatcherHandler::new(
            std::sync::Arc::new(crash_tx),
            stage2_dispatch_available,
        )),
        // #t-777-3: respawn-stuck watchdog — auto-Fresh-restart an agent whose
        // Resume spawn hung (corrupt-session `resume --last`), bounded by a
        // retry cap that escalates a P0 + pause. Recovers via the proven API
        // restart path so it works in BOTH run_core and the live app-mode daemon
        // (where the crash_tx→respawn machinery is inert). Disjoint state class
        // from the Hung ladder above (no crash_tx needed).
        Box::new(per_tick::RespawnWatchdogHandler::new()),
        Box::new(per_tick::WatchdogHandler::new(watchdog_dry_run)),
        Box::new(per_tick::ExternalLivenessHandler::new()),
        // #2413 (B): ShadowObserve MUST run immediately BEFORE SnapshotRotation so the
        // snapshot's operated `agent_state` promotion reads THIS tick's `observed_status`
        // (it was previously LAST in the list → the snapshot would read last tick's). The
        // reorder is confirm-first safe:
        //   - ShadowObserve only WRITES `observed_status` + `published_observed`; nothing
        //     else in this list writes those, so there is no write-write hazard, and no
        //     handler except SnapshotRotation (below) reads them.
        //   - Its INPUTS are order-independent of the per-tick sequence: `api_activity` is
        //     written by a BACKGROUND thread (`api_activity_probe::spawn`), `state` /
        //     productive-silence by the PTY-feed thread, hook Evidence by the socket
        //     thread — none are per-tick handlers, so moving ShadowObserve earlier does not
        //     stale them.
        //   - The state-transition handlers (HangDetection / RecoveryDispatcher /
        //     RespawnWatchdog / Watchdog) sit ABOVE this point in BOTH the old and new
        //     layout, so ShadowObserve observes the same post-transition `state` either way.
        // The only behaviour change is the intended one: the snapshot promotes from a fresh
        // (this-tick) `observed_status`.
        Box::new(per_tick::ShadowObserveHandler::new()),
        Box::new(per_tick::SnapshotRotationHandler::new()),
        Box::new(per_tick::CheckSchedulesHandler::new()),
        Box::new(per_tick::CiWatchPollHandler::new()),
        Box::new(per_tick::PrStateScanHandler::new()),
        Box::new(per_tick::InboxMaintenanceHandler::new(60)),
        Box::new(per_tick::PollReminderHandler::new(30)),
        // #1491(A): inbox-stuck watchdog — every 30 ticks (~5min). Detects an
        // agent receiving but not draining its inbox; notifies lead (no auto-restart).
        Box::new(inbox_stuck),
        // #1491(B): next_after_ci handoff-timeout watchdog. #1859 lowered the
        // cadence to ~2min (12 ticks) so the daemon-side RE-NUDGE of the target
        // (Fix A) is timely; the lead ESCALATION stays gated by its own 10min age
        // + 30min re-alert windows, so the faster scan doesn't escalate sooner.
        Box::new(per_tick::HandoffTimeoutHandler::new(12)),
        // #2090 (report mode): progress-backstop watchdog — ~30s cadence, boot-
        // grace suppressed. Only fires when `progress_mode == 2`; nudges an agent
        // to self-report if an external-channel turn runs long with no reply. It
        // never authors/relays content itself (zero exfil). Default progress_mode
        // is 0 (off) → this is a cheap early-return no-op for a default fleet.
        Box::new(per_tick::ProgressBackstopHandler::new()),
        // #2090 (mirror mode): progress-mirror — every tick. Only relays when
        // `progress_mode == 1`; tails each agent's transcript and sends NEW
        // assistant text to its origin channel (active-turn gated, no broadcast,
        // no backlog replay, truncated). ⚠ exfil surface — default OFF, so this
        // is a cheap early-return no-op for a default fleet.
        Box::new(per_tick::ProgressMirrorHandler::new()),
        // Daemon-side deferred-notification flush — every tick (~10s). The
        // #1513 busy-gate defers notifications into the queue whose only other
        // flusher is the TUI loop; headless `run_core` (`start --foreground`)
        // has no TUI, so without this handler deferred operator messages
        // strand forever (7 stranded Telegram messages, 2026-06-10). Idle
        // cost per instance: one read_dir of notification-queue/ plus a line
        // count of any existing queue files — trivial at fleet sizes.
        Box::new(per_tick::NotificationFlushHandler::new(1)),
        Box::new(per_tick::LogRotationHandler::new(360)),
        Box::new(per_tick::ThreadDumpHandler::new()),
        Box::new(per_tick::GcTickHandler::new(360)),
        // #2158 item 2: hourly stray-managed-worktree sweep (edge-triggered
        // event-log + fleet health count). Same 360-tick GC cadence; runs in app
        // mode too (not allowlisted out) since the live daemon is app-mode.
        Box::new(per_tick::WorkspaceBoundarySweepHandler::new(360)),
        // #1747: slow-cadence backstop GC for stale /tmp review worktrees (mtime
        // > 2d). Same 360-tick cadence as the other GC siblings; runs in app mode
        // (not allowlisted out) since the live daemon is app-mode.
        Box::new(per_tick::TmpReviewGcHandler::new(360)),
        // #2234 (B) prereq: hourly retention GC for <home>/reconcile-backups/
        // (mtime-age ≥ 14d), with a per-agent newest-1 floor as the destroy-work
        // safety net. Same 360-tick cadence as the GC siblings; app-mode (the
        // live daemon is app-mode). (B) OFF → no backups → natural no-op.
        Box::new(per_tick::ReconcileBackupsGcHandler::new(360)),
        // #1967 Phase-1 (PR1): reap ephemeral workers every ~1min (6 ticks) —
        // removes/terminates terminal, max-wall-TTL-expired (cost guard), or
        // already-dead workers. Runs in app mode too (not allowlisted out); the
        // live daemon is app-mode. Idle cost: one read of a usually-tiny JSON sidecar.
        Box::new(per_tick::EphemeralReapHandler::new(6)),
        // Context% alert (operator-directed): every 6 ticks (~1min) refresh +
        // ≥80% orchestrator alert. The transcript-estimate file IO lives in
        // this handler's tick (lock-free during the read), NOT in the PTY
        // feed path. Runs in app mode (the live daemon is app-mode).
        Box::new(per_tick::ContextAlertHandler::new(6)),
        // #2007 context-full safety net: every 6 ticks (~1min) — 85% one-shot
        // [AGEND-AUTO kind=context-handoff] injection to the agent itself,
        // 92% one-shot operator escalation. Noise-budgeted (per-episode
        // latch + hysteresis re-arm). Runs in app mode (live daemon).
        Box::new(per_tick::ContextHandoffHandler::new(6)),
        // #2044 inject-delivery watchdog: every tick (~10s) verify that an
        // armed actionable wake produced a UserPromptSubmit; re-deliver once
        // + WARN if a dialog swallowed it. Cheap (iterates a usually-empty
        // map); claude-only in practice (arm self-gates on hook history).
        Box::new(per_tick::InjectDeliveryHandler::new(1)),
        // ── W1.1 (#2050): the 12 trackers migrated from the supervisor
        // `run_loop` (supervisor.rs:384-395). Appended in their original
        // relative order; each self-throttles internally (TICKS_PER_SCAN), so
        // running them every tick here is the same cadence the supervisor ran.
        // They previously executed on the supervisor thread; the main loop
        // ticks at the identical 10s interval and holds no lock across them, so
        // this is behavior-preserving on unix (both run_core and app mode).
        // Cadence-hoist to the handler (`should_fire`) is W2.4, not W1.1.
        Box::new(per_tick::AntiStallHandler::new()),
        Box::new(per_tick::IdleWatchdogHandler::new()),
        Box::new(per_tick::DecisionTimeoutHandler::new()),
        Box::new(per_tick::HelperStalenessHandler::new()),
        Box::new(per_tick::McpRegistryHandler::new(daemon_binary_stale)),
        Box::new(per_tick::WaitingOnStaleHandler::new()),
        Box::new(per_tick::ConflictNotifyHandler::new()),
        Box::new(per_tick::CanonicalDriftHandler::new()),
        Box::new(per_tick::AutoReleaseHandler::new()),
        Box::new(per_tick::DispatchIdleHandler::new()),
        Box::new(per_tick::DispatchIdleNudgeHandler::new()),
        Box::new(per_tick::RetentionHandler::new()),
        // #2127 Phase 1: reclaim board tasks from agents stuck in a non-recoverable
        // usage_limit window (operator decision d-…085112: Phase 1, grace=10min).
        // Every 30 ticks (~5min). Fires ONLY for UsageLimit/QuotaExceeded with a
        // remaining window > grace and no recent recovery — releases the agent's
        // claimed/in_progress tasks back to Open + clears the work-stuck latch.
        // Runs in both run_core and app mode (live daemon is app-mode).
        Box::new(per_tick::ReclaimHandler::new(30, work_stuck_latch)),
    ]
}

/// Sprint 57 Wave 3 PR-2 (#548 Q4 contract pin): the canonical
/// lockfile is `$AGEND_HOME/.daemon.lock` (one acquirer at a time
/// across all daemon processes). Per-PID identity is at
/// `$AGEND_HOME/run/<pid>/.daemon` (PID-recycling guard for
/// discovery — distinct purpose from the exclusive lock).
/// Where `run_core` sources its fleet from.
///
/// `Resolved` is the normal path: `bootstrap::prepare` already resolved the
/// agents (and channel) under the flock. `HandoffDeferred` is the #1814
/// successor-handoff path: the agents are NOT resolved yet because the
/// destructive reconciles + resolve MUST wait until this successor acquires
/// the flock (after the predecessor exits), never running in the two-daemon
/// overlap window.
enum FleetSource {
    Resolved {
        agents: Vec<AgentDef>,
        telegram: Option<Arc<dyn crate::channel::Channel>>,
    },
    HandoffDeferred {
        fleet_path: PathBuf,
        opts: crate::bootstrap::PrepareOptions,
    },
}

/// #1814: how long a successor waits for its predecessor to release the flock
/// (by exiting) after Phase-1 passed. Generous — the predecessor only needs to
/// run `shutdown_sequence` (≤2s grace) + a 1s settle before exit. A timeout is
/// a backstop for a predecessor wedged in shutdown.
const HANDOFF_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// #1814: marker file a handoff successor writes once its control plane (api
/// socket) is bound, signalling the predecessor's Phase-1 gate that the
/// control plane is ready. Distinct from `.ready` (which means "agent spawn
/// loop complete" and is written only after promotion).
pub const CONTROL_READY_FILE: &str = "control-ready";

/// #1814 successor-handoff boot entry. Runs the minimal pre-lock prep (run dir
/// and cookie), then `run_core` in deferred-fleet mode: bind api, write
/// control-ready, block on the flock until the predecessor exits, then run the
/// destructive reconciles, resolve, and spawn agents. Routed to from the
/// `start` command only when a legitimate `AGEND_SUCCESSOR_HANDOFF` marker is
/// present.
pub fn run_successor_handoff(home: &Path, fleet_path: &Path) -> anyhow::Result<()> {
    tracing::info!("#1814 successor-handoff boot: minimal pre-lock prep (no flock, no reconcile)");
    // §3.9 test injection seam: force the successor to crash on launch (before
    // it writes control-ready) so the integration test can exercise the
    // predecessor's abort-stay-alive path against a REAL spawned successor.
    // Only the handoff boot path reads this — a normal start never reaches here.
    if std::env::var("AGEND_FORCE_SUCCESSOR_FAIL").as_deref() == Ok("1") {
        tracing::warn!(
            "#1814 AGEND_FORCE_SUCCESSOR_FAIL=1 — successor aborting on launch (test seam)"
        );
        std::process::exit(1);
    }
    crate::bootstrap::prepare_handoff_prelock(home)?;
    run_core(
        home,
        FleetSource::HandoffDeferred {
            fleet_path: fleet_path.to_path_buf(),
            opts: crate::bootstrap::PrepareOptions::default(),
        },
    )
}

/// #1814: write the `control-ready` marker into this daemon's run dir.
fn write_control_ready(home: &Path) {
    let path = run_dir(home).join(CONTROL_READY_FILE);
    if let Err(e) = std::fs::write(&path, chrono::Utc::now().to_rfc3339()) {
        tracing::warn!(path = %path.display(), error = %e, "failed to write control-ready marker (handoff)");
    }
}

/// `CleanExit` handler — a clean agent exit removes it from the live registry
/// (UUID-keyed; name resolved via fleet.yaml) and from the respawn-config map,
/// and does NOT respawn. Extracted from `run_core`'s select loop (sibling of
/// [`crash_respawn::handle_crash_respawn`]) so the eviction / no-respawn contract
/// is unit-testable without driving the whole daemon event loop. Evicting the
/// config is what prevents a later resurrect: `handle_crash_respawn` reads
/// `configs` to respawn, so a cleanly-exited agent with no config can't come back.
fn handle_clean_exit(
    home: &Path,
    name: &str,
    registry: &crate::agent::AgentRegistry,
    configs: &Mutex<HashMap<String, AgentConfig>>,
) {
    tracing::info!(agent = %name, "clean exit — removing from registry (no respawn)");
    // #1441: registry is UUID-keyed; resolve name via fleet.yaml.
    if let Some(id) = crate::fleet::resolve_uuid(home, name) {
        registry.lock().remove(&id);
    }
    configs.lock().remove(name);
}

fn run_core(home: &Path, source: FleetSource) -> anyhow::Result<()> {
    let started_at = std::time::Instant::now();

    // #2098: mark run_core as the active restart consumer. Set at entry (before
    // the api server is brought up below, so it is always true by the time any
    // `restart_daemon` MCP call can arrive) — once we are in run_core we will
    // reach the tail that consumes RESTART_PENDING, so an in-process self-respawn
    // is safe here. The handoff successor also runs through run_core, so it
    // inherits this and can restart normally in turn.
    RUN_CORE_ACTIVE.store(true, Ordering::Release);

    // For the handoff path, the channel inits post-lock (its registry attaches
    // via the #945 pending-registry bridge that `init_daemon_services` arms),
    // so pass None here; `Resolved` carries the already-inited channel.
    let telegram_pre = match &source {
        FleetSource::Resolved { telegram, .. } => telegram.clone(),
        FleetSource::HandoffDeferred { .. } => None,
    };

    let ctx = init_daemon_services(home, telegram_pre)?;

    // #2413 Shadow Observer — local plane: start the unix-socket hook-event server
    // (no-op under AGEND_SHADOW_OBSERVER=0; default-ON). Observe-only side-channel; never blocks.
    crate::daemon::shadow::start(home);

    // #event-bus Step 2 (legacy-zero): register the per-pattern delivery
    // subscribers once (the bus is the SOLE delivery path). Shared with
    // `app::run_app` so owned `agend-terminal app` mode wires the IDENTICAL
    // set — see `register_event_subscribers`.
    register_event_subscribers(&ctx.registry);

    // `init_daemon_services` has now bound + published this process's api port.
    // For the handoff path the predecessor is still alive: signal control-ready
    // (so its Phase-1 gate can confirm us), then block on the flock until it
    // exits (the commit point), and ONLY THEN run the destructive reconciles +
    // resolve. `_handoff_lock` holds the flock for the daemon's lifetime on the
    // handoff path (None on the normal path, where `prepare`'s `OwnedFleet`
    // already holds it).
    let (agents, _handoff_lock) = match source {
        FleetSource::Resolved { agents, .. } => {
            // Normal boot: the flock is already held by `prepare`'s `OwnedFleet`,
            // so the post-lock GC/migration runs here — the same early point as
            // before the #1814 Stage-2 split (behavior unchanged).
            init_daemon_services_post_lock(home)?;
            (agents, None::<crate::bootstrap::DaemonLock>)
        }
        FleetSource::HandoffDeferred { fleet_path, opts } => {
            write_control_ready(home);
            // §3.9 FIX2 test seam: pass Phase-1 (api stays up to answer STATUS
            // for a moment) then die BEFORE acquiring the flock — exercises the
            // predecessor's commit→exit liveness recheck (abort-stay-alive).
            if std::env::var("AGEND_FORCE_SUCCESSOR_FAIL_AFTER_CONTROL_READY").as_deref() == Ok("1")
            {
                tracing::warn!(
                    "#1814 AGEND_FORCE_SUCCESSOR_FAIL_AFTER_CONTROL_READY=1 — successor answering Phase-1 then aborting before flock (test seam)"
                );
                std::thread::sleep(std::time::Duration::from_secs(3));
                std::process::exit(1);
            }
            // §3.9 round-2 test seam: stay alive LONG enough to pass the
            // predecessor's loop-break recheck (so teardown begins), then die
            // DURING the predecessor's teardown window — exercises the final
            // recover-as-primary gate (predecessor re-spawns agents + resumes).
            if std::env::var("AGEND_FORCE_SUCCESSOR_FAIL_DURING_TEARDOWN").as_deref() == Ok("1") {
                tracing::warn!(
                    "#1814 AGEND_FORCE_SUCCESSOR_FAIL_DURING_TEARDOWN=1 — successor surviving Phase-1 + loop-break, dying in teardown window (test seam)"
                );
                std::thread::sleep(std::time::Duration::from_secs(15));
                std::process::exit(1);
            }
            tracing::info!(
                "#1814 successor-handoff: control-ready; waiting for predecessor to release flock"
            );
            let lock = crate::bootstrap::acquire_daemon_lock_blocking(home, HANDOFF_LOCK_WAIT)?;
            // #1814 FIX1: NOW that we hold the flock (predecessor has exited),
            // publish the `.daemon` identity so generic discovery
            // (`find_active_run_dir`) starts routing to us. Before this point we
            // were intentionally undiscoverable (pre-flock = not the primary).
            write_daemon_id(&run_dir(home));
            tracing::info!(
                "#1814 successor-handoff: flock acquired — sole daemon, running deferred reconciles + resolve"
            );
            // #t-27: NOW that the flock is held, run the shared-state GC/migration
            // that `init_daemon_services` no longer does pre-flock — keeping them
            // off the predecessor-overlap window. Ordered before the reconciles,
            // matching the pre-split ordering.
            init_daemon_services_post_lock(home)?;
            crate::bootstrap::boot_hygiene_sweeps(home);
            let (_config, agents, _telegram) =
                crate::bootstrap::resolve_fleet_and_reconcile(home, &fleet_path, &opts)?;
            (agents, Some(lock))
        }
    };

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

    // #1814 round-2: `'serve` wraps the tick loop + teardown so the final
    // recover-as-primary gate (below) can `continue 'serve` to resume serving if
    // the successor dies during the predecessor's teardown — instead of exiting
    // into a brick. Flag-off never enters the recover gate, so it falls straight
    // through to the byte-identical exit path after the loop.
    'serve: loop {
        loop {
            if ctx.shutdown.load(Ordering::Relaxed) {
                // #1814 FIX2: a set shutdown flag from a self-respawn commit only
                // tears down if the successor is still alive; otherwise abort-stay-alive.
                if confirm_shutdown_or_abort_respawn(&ctx.shutdown) {
                    break;
                }
                continue;
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
            // #1339: operator-mode.json reloaded each tick — a mode change (via the
            // `mode` MCP tool) propagates fleet-wide without a restart (reload-coherent).
            crate::operator_mode::reload(home);
            per_tick::run_handlers_with_panic_guard(&handlers, &tick_ctx);

            let exit_event = match exit_event {
                Some(e) => e,
                None => continue,
            };

            if ctx.shutdown.load(Ordering::Relaxed) {
                if confirm_shutdown_or_abort_respawn(&ctx.shutdown) {
                    break;
                }
                continue;
            }
            match exit_event {
                crate::agent::AgentExitEvent::CleanExit(ref name) => {
                    handle_clean_exit(home, name.as_str(), &ctx.registry, &ctx.configs);
                }
                crate::agent::AgentExitEvent::Stage2Restart(name) => {
                    spawn_stage2_thread(home, &name, &ctx);
                }
                crate::agent::AgentExitEvent::Crash(name) => {
                    crash_respawn::handle_crash_respawn(home, &name, &ctx);
                }
            }
        }

        log_residual_worktrees(home);

        let metrics = shutdown_sequence(home, &ctx.registry, started_at);
        // #t-41673 gap-instrument: clock from shutdown-complete to the
        // predecessor's final exit log — the "old-exit 收尾" portion of the
        // ~4s no-log gap (file removals + self-respawn settle, then exit(0)).
        let teardown_started = std::time::Instant::now();
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

        // #1814 round-2 (reviewer TOCTOU): FINAL recover-as-primary gate. We have
        // killed our agents (`shutdown_sequence` drained the registry) but the run
        // dir + cookie + api-server thread are STILL intact (`remove_dir_all` below
        // hasn't run). This is the last point before the irreversible exit/flock-
        // release. Re-check the successor's liveness as late as possible:
        //   • successor DEAD → do NOT exit. Recover as primary: clear the restart
        //     flags, re-spawn our fleet agents into the (still-live) registry, and
        //     `continue 'serve` to resume serving. No brick; agents re-spawned.
        //   • successor ALIVE → commit: drop the run dir and exit(0) IMMEDIATELY
        //     (no intervening sleep) so the only un-closable window is the
        //     microseconds between this check and the exit syscall (the d-2 step-6
        //     residual — a successor death after THIS point needs an external
        //     supervisor and is out of scope for Stage 1).
        // Flag-off never enters this block → it falls through to the byte-identical
        // exit path after `'serve`.
        //
        // INVARIANT (do not break): this gate's safety relies on
        // `flag-on + RESTART_PENDING ⟹ a successor was parked`
        // (`handle_self_respawn` parks the child BEFORE setting RESTART_PENDING).
        // `self_respawn_successor_died()` returns `false` when nothing is parked.
        // So if a FUTURE path sets RESTART_PENDING under flag-on WITHOUT parking a
        // successor, this gate sees "not died" → falls through to exit(0) with no
        // successor coming up → BRICK. Any new RESTART_PENDING writer on the
        // self-respawn path MUST park a live successor first (or gate itself out).
        if RESTART_PENDING.load(Ordering::Acquire) && crate::daemon::restart::self_respawn_enabled()
        {
            std::thread::sleep(self_respawn_settle());
            if self_respawn_successor_died() {
                tracing::error!(
                    target: "handoff",
                    event = "recover_as_primary",
                    "#1814 self-respawn: successor died DURING predecessor teardown — recovering as \
                     primary (re-spawning agents, resuming; no brick)."
                );
                RESTART_PENDING.store(false, Ordering::Release);
                ctx.shutdown.store(false, Ordering::Relaxed);
                *SELF_RESPAWN_SUCCESSOR
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
                let _ = std::fs::remove_file(home.join("restart-requested"));
                spawn_fleet_agents(home, &agents, &ctx);
                continue 'serve;
            }
            let _ = std::fs::remove_file(home.join("restart-requested"));
            let _ = std::fs::remove_dir_all(run_dir(home));
            // instrument-only: D3 #2310 — gap-instrument anchor, control-flow-
            // inert; the real op is the `exit(0)` below.
            tracing::info!(
                target: "handoff",
                event = "predecessor_exit",
                reason = "self_respawn_exit0",
                teardown_elapsed_ms = teardown_started.elapsed().as_millis() as u64,
                "#1814 self-respawn: successor healthy through teardown — exiting 0"
            );
            // #t-41673/C1: process::exit skips main's guard Drop → flush the
            // non-blocking log writer so the predecessor_exit anchor above lands.
            crate::logging::flush_daemon_log();
            std::process::exit(0);
        }

        break 'serve;
    }

    // #t-41673 gap-instrument: post-'serve teardown clock for the legacy
    // exit-42 / normal-stop paths (self-respawn-disabled). Mirrors the in-loop
    // teardown clock used by the self-respawn exit(0); note the fixed 1s sleep
    // below is part of this window.
    let post_serve_teardown = std::time::Instant::now();
    let _ = std::fs::remove_dir_all(run_dir(home));
    std::thread::sleep(std::time::Duration::from_secs(1));

    if RESTART_PENDING.load(Ordering::Acquire) {
        let flag = home.join("restart-requested");
        let _ = std::fs::remove_file(&flag);
        // #t-23: the self-respawn `exit(0)` that used to live here was
        // unreachable. The only way out of `'serve` to this post-loop point is
        // `break 'serve` above, taken when the loop's self-respawn gate
        // (`RESTART_PENDING && self_respawn_enabled()`) is FALSE.
        // `self_respawn_enabled()` is a process-constant env read
        // (`AGEND_RESTART_HANDOFF=="1"`), so reaching here with RESTART_PENDING
        // still set implies the flag is OFF — under flag-on the healthy exit(0)
        // already happened inside `'serve` (after `shutdown_sequence`). Keep the
        // invariant as a debug_assert and take the operator-restart exit, where
        // an external supervisor (exit-code-42 contract) respawns us.
        debug_assert!(
            !crate::daemon::restart::self_respawn_enabled(),
            "#1814: post-'serve RESTART_PENDING under self-respawn flag-on — the \
             flag-on exit must occur inside 'serve, never here"
        );
        tracing::info!(
            target: "handoff",
            event = "predecessor_exit",
            reason = "operator_restart_exit42",
            teardown_elapsed_ms = post_serve_teardown.elapsed().as_millis() as u64,
            "operator-initiated restart: exiting with code 42"
        );
        // #t-41673/C1: process::exit skips main's guard Drop → flush the
        // non-blocking log writer so the predecessor_exit anchor above lands.
        crate::logging::flush_daemon_log();
        std::process::exit(42);
    }

    // #t-41673/C2: normal stop returns to `main`, whose RAII guard-flush runs on
    // return — no explicit flush needed here. `target: "handoff"` keeps the
    // predecessor_exit family filterable alongside the exit0/exit42 markers.
    tracing::info!(
        target: "handoff",
        event = "predecessor_exit",
        reason = "normal_stop",
        teardown_elapsed_ms = post_serve_teardown.elapsed().as_millis() as u64,
        "exiting"
    );
    Ok(())
}

// ── Extracted phases ────────────────────────────────────────────

fn init_daemon_services(
    home: &Path,
    telegram: Option<Arc<dyn crate::channel::Channel>>,
) -> anyhow::Result<DaemonContext> {
    // #1487: source the operator timezone from fleet.yaml `display_timezone:`
    // (reusing the same operator-tz concept as ci_watch / display_time) for the
    // `now=` header field; `None`/empty → system local time.
    let display_timezone = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .and_then(|f| f.display_timezone)
        .filter(|s| !s.is_empty());
    crate::daemon_config::init(crate::daemon_config::DaemonConfig {
        display_timezone,
        ..crate::daemon_config::DaemonConfig::default()
    });

    // #1814 Stage-2 (#t-27): the three shared-state GC/migration steps live in
    // `init_daemon_services_post_lock`, NOT here. On the successor-handoff path
    // `init_daemon_services` runs PRE-flock (before the predecessor exits), so
    // running shared-state mutation here would escape the "minimal pre-lock"
    // contract (d-3). The caller invokes `init_daemon_services_post_lock` only
    // after the flock is held (normal boot: already held; handoff: post-acquire).

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

/// #1814 Stage-2 (#t-27): the shared-state GC + migration steps that MUST run
/// only while this process holds the daemon flock. Split out of
/// [`init_daemon_services`] because that runs PRE-flock on the successor-handoff
/// path (before the predecessor exits); running shared-state mutation there
/// overlaps the predecessor and escapes the d-3 "minimal pre-lock" contract.
///
/// Callers:
/// - normal boot — invoked right after `init_daemon_services`, where the flock
///   is already held by `prepare`'s `OwnedFleet` (behavior unchanged);
/// - successor handoff — invoked only after `acquire_daemon_lock_blocking`.
///
/// The legacy-migration hard-error is preserved: a failed migration aborts boot
/// (returns `Err`), exactly as before the split.
fn init_daemon_services_post_lock(home: &Path) -> anyhow::Result<()> {
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
    Ok(())
}

fn spawn_fleet_agents(home: &Path, agents: &[AgentDef], ctx: &DaemonContext) {
    tracing::info!(count = agents.len(), "starting agents");
    crate::bootstrap::time_step("agent_spawn_loop", || {
        for def in agents {
            // #1913 (b): re-check fleet membership immediately before each spawn.
            // The boot loop iterates a SNAPSHOT of `agents` (resolved before the
            // loop) and `spawn_stagger()` sleeps ~500ms between spawns — a wide
            // window in which a `delete_instance` can remove an agent from
            // fleet.yaml. Spawning a just-deleted agent RESURRECTS it: it
            // re-creates `workspace/<name>` + a registry handle AFTER teardown
            // cleanup already ran (the intermittent residual that flaked the
            // #1907/#1909 teardown oracle). `instance_is_known` going false means
            // the agent was torn down mid-boot — skip it. The check→spawn gap is
            // not atomic; the load-bearing fix is the `spawn_agent` chokepoint
            // (#1915, a deleting-set guard that subsumes this), but this closes the
            // wide stagger window that the oracle actually hit.
            if !crate::fleet::instance_is_known(home, &def.0) {
                tracing::info!(
                    agent = %def.0,
                    "skipping boot spawn — instance left fleet.yaml during stagger (#1913 spawn-vs-delete)"
                );
                continue;
            }
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
        supervisor::spawn(home.to_path_buf(), Arc::clone(&ctx.registry));
    }
    router::spawn(home.to_path_buf(), Arc::clone(&ctx.registry));
    crate::instance_monitor::spawn_monitor_tick(home.to_path_buf(), Arc::clone(&ctx.registry));
    // #2413 Phase 1: out-of-path lsof API-activity probe (feeds
    // AgentCore::api_activity for false-idle detection). Self-disables if
    // `lsof` is absent.
    crate::api_activity_probe::spawn(Arc::clone(&ctx.registry));
    // #2413 Phase D: codex rollout-tail observer source (Stream plane) — read-only tail of
    // ~/.codex/sessions/.../rollout-*.jsonl → Evidence → the shared buffer the reducer
    // consumes. No-op under the AGEND_SHADOW_OBSERVER=0 kill-switch (default-ON).
    // ALSO wired into run_app (the live fleet daemon is app mode — #2434 lesson).
    crate::daemon::shadow::rollout::spawn(Arc::clone(&ctx.registry), home.to_path_buf());
    // #2413 opencode plane: SSE `/event` observer source (Stream plane). Subscribes to each
    // opencode agent's embedded server (port injected at spawn) → Evidence → shared buffer.
    // No-op under AGEND_SHADOW_OBSERVER=0 (default-ON). ALSO wired into run_app (#2434).
    crate::daemon::shadow::opencode::spawn(Arc::clone(&ctx.registry), home.to_path_buf());
    // #2413 kiro plane: read-only tail of ~/.kiro/sessions/cli/<uuid>.jsonl → Evidence →
    // shared buffer (attribution via the <uuid>.json sidecar cwd). No-op under
    // AGEND_SHADOW_OBSERVER=0 (default-ON). ALSO wired into run_app (#2434 lesson).
    crate::daemon::shadow::kiro::spawn(Arc::clone(&ctx.registry), home.to_path_buf());

    crate::inbox::recover_half_writes(home);
    // #1988: same half-write recovery for the task-event log — quarantine a
    // crash-torn tail line and rewrite the hot log with the good events only,
    // so a single bad byte cannot brick the whole task board on replay.
    crate::task_events::recover_half_writes(home);
    replay_missed_at_startup(home, &ctx.registry);
    crate::daemon::ci_watch::startup_sweep(home);
    // #1488: GC bindings (schedules/dispatch_tracking/ci_watch) left orphaned
    // by instances deleted before the cascade-on-delete fix existed.
    crate::daemon::orphan_sweep::run(home);

    // #1694(a): run_core wires `crash_rx` → `handle_crash_respawn`, so Stage2
    // restarts have a live consumer here (true). `run_core` is headless (no
    // TUI), so the `DaemonBinaryStale` flag the `mcp_registry` handler flips is
    // a throwaway here — nothing surfaces it, exactly as the pre-W1.1
    // supervisor-side flag was in run_core.
    let daemon_binary_stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale =
        Arc::new(AtomicBool::new(false));
    let handlers = build_default_handlers(ctx.crash_tx.clone(), true, daemon_binary_stale);

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

fn log_residual_worktrees(home: &Path) {
    // #1458: pre-Wave-4 legacy detection (`<repo>/.worktrees/<agent>/`) retired.
    // Only the new-layout check under `$AGEND_HOME/worktrees/` remains.
    let central_residual = crate::worktree::list_residual(home);
    if !central_residual.is_empty() {
        tracing::info!(
            location = %home.join("worktrees").display(),
            residual = ?central_residual,
            "residual agent worktrees found under $AGEND_HOME/worktrees/ \
             (cleared on next bind_self/release_worktree cycle)"
        );
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
///
/// #bughunt-r1: per-agent disposition at the post-grace SIGKILL stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraceDisposition {
    /// Child still alive after the grace window → escalate to a process-group
    /// SIGKILL (`kill_process_tree`).
    HardKill,
    /// Child already exited cleanly during the grace window → only reap it.
    /// MUST NOT `kill_process_tree`: the exited child's PID may have been reused
    /// by an unrelated process, so SIGKILLing that group is collateral damage.
    ReapOnly,
}

/// #bughunt-r1 (HIGH): a child that exited cleanly within the grace window must
/// NOT be hard-killed (PID-reuse → wrong-process-group SIGKILL). Only a holdout
/// (`still_alive`) escalates to `kill_process_tree`.
fn grace_disposition(still_alive: bool) -> GraceDisposition {
    if still_alive {
        GraceDisposition::HardKill
    } else {
        GraceDisposition::ReapOnly
    }
}

/// A registry agent's child-process handle (`AgentHandle.child`).
pub(crate) type ChildHandle = std::sync::Arc<Mutex<Box<dyn portable_pty::Child + Send>>>;

/// Parallel agent-teardown core, shared by `run_core`'s [`shutdown_sequence`]
/// and app-mode `app_teardown` so the two paths cannot drift (restart-freeze
/// 真嫌#1, t-…55279: app teardown previously killed agents in a *sequential*
/// per-agent wait loop, ~0.5 s × N ≈ ~6 s of the operator-visible restart
/// freeze; routing it through this proven run_core core makes the wall time
/// ≈ one grace window regardless of N).
///
/// Stages (Sprint 57 Wave 3 PR-2):
/// 1. **Parallel SIGTERM** — on Unix, signal every agent's process group
///    concurrently; on Windows there is no signal model, so the grace wait
///    below just lets agents that exit on PTY EOF be reported as clean.
/// 2. **Single grace window** — one [`SHUTDOWN_GRACE`] sleep for ALL agents
///    (NOT per-agent), so they exit concurrently.
/// 3. **SIGKILL + reap** — holdouts still alive after grace are escalated to a
///    process-group SIGKILL then reaped; clean exits are reaped only
///    (#bughunt-r1: no `kill_process_tree` on an exited PID — it may be reused).
///
/// Returns the number of agents that had to be SIGKILLed after the grace window.
/// Callers MUST drain/snapshot the registry BEFORE calling so PTY-close handlers
/// observe the agent as gone (or shutting down) and return silently instead of
/// emitting crash/respawn events.
pub(crate) fn terminate_agents_parallel(agents: Vec<(String, ChildHandle)>) -> usize {
    // Stage 1: parallel SIGTERM.
    let mut pids: Vec<(String, ChildHandle, Option<u32>)> = Vec::with_capacity(agents.len());
    for (name, child) in agents {
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

    // Stage 2: single grace window for all agents.
    std::thread::sleep(SHUTDOWN_GRACE);

    // Stage 3: reap clean exits; SIGKILL + reap genuine holdouts.
    //
    // Detect exit with a non-blocking `try_wait` rather than a raw-PID
    // `is_pid_alive` probe. A child that exited during the grace window but has
    // not been reaped yet is a ZOMBIE, which `kill(pid, 0)` (is_pid_alive)
    // reports as STILL ALIVE — so an is_pid_alive gate pushes every grace-exiter
    // down the HardKill arm and pays `kill_process_tree`'s 500 ms
    // SIGTERM→SIGKILL sleep per agent, serialized. That N×~0.5 s is exactly the
    // cost this parallel teardown exists to remove (restart-freeze 真嫌#1), and
    // it bit app teardown hardest because the shutdown flag makes PTY-close
    // handlers fast-return WITHOUT reaping, leaving every child a zombie here.
    // `try_wait` reaps the zombie in place; it is also safer than is_pid_alive
    // (#bughunt-r1: acts on the owned child handle, so no reaped-then-reused-PID
    // window). Only a child still genuinely running after grace (`try_wait` →
    // `None`) escalates to the process-group SIGKILL.
    let mut killed_after_grace = 0usize;
    for (name, child, pid) in pids {
        // #t-41673 gap-instrument: per-agent reap clock — a slow `child.wait()`
        // (kill→reap or grace-window reap) is a prime suspect for the shutdown
        // half of the restart freeze; emit `reap_ms` on the existing per-agent
        // logs. Now in the shared helper, so it covers app-mode teardown too.
        let reap_started = std::time::Instant::now();
        // r6 latent-defense: ONLY a child that `try_wait` positively confirms is
        // still running (`Ok(None)`) escalates to `kill_process_tree`. A reaped
        // exit (`Ok(Some)`) AND a status-read error (`Err` — e.g. already reaped
        // elsewhere / ECHILD) both map to ReapOnly: we never SIGKILL a process
        // group whose PID we cannot PROVE is still ours (reused-PID hazard,
        // #bughunt-r1), even if a future reaper/flag change races us here.
        let still_running = matches!(child.lock().try_wait(), Ok(None));
        match grace_disposition(still_running) {
            GraceDisposition::HardKill => {
                // Holdout still running after grace — escalate to a SIGKILL of
                // the whole process group, then reap the child handle.
                if let Some(p) = pid {
                    crate::process::kill_process_tree(p);
                }
                let _ = child.lock().kill();
                let _ = child.lock().wait();
                killed_after_grace += 1;
                tracing::info!(
                    agent = %name,
                    reap_ms = reap_started.elapsed().as_millis() as u64,
                    "killed (after grace window)"
                );
            }
            GraceDisposition::ReapOnly => {
                // Clean exit during the grace window — already reaped by the
                // `try_wait` above (no `kill_process_tree`: #bughunt-r1, a reused
                // PID's group must never be SIGKILLed).
                tracing::info!(
                    agent = %name,
                    reap_ms = reap_started.elapsed().as_millis() as u64,
                    "exited cleanly during grace window"
                );
            }
        }
    }
    killed_after_grace
}

pub(crate) fn shutdown_sequence(
    home: &Path,
    registry: &AgentRegistry,
    started_at: std::time::Instant,
) -> ShutdownMetrics {
    let reason = ShutdownReason::from_u8(SHUTDOWN_REASON.load(Ordering::Relaxed));
    // #t-41673 gap-instrument: time the whole shutdown sequence so the ~6s
    // shutdown half of the restart freeze is attributable separately from the
    // old-exit→new-launch gap. Pure tracing; mirrors the #2271 restart_timing
    // (`target: "handoff"`, `elapsed_ms`) style.
    let shutdown_started = std::time::Instant::now();
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

    // Parallel SIGTERM → single grace → SIGKILL/reap holdouts (shared core).
    // The per-agent `reap_ms` gap-instrument (#t-41673) lives inside the helper
    // so it covers app-mode teardown too.
    let agents_killed_after_grace = terminate_agents_parallel(agents_to_kill);

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
        shutdown_elapsed_ms = shutdown_started.elapsed().as_millis() as u64,
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
        persist_or_log!(
            crate::inbox::enqueue_with_idle_hint(
                home,
                &a.to,
                crate::inbox::InboxMessage::new_system("system:dispatch", "query", query),
            ),
            "dispatch_stuck_check",
            a.to
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

        // #1530/F1: snapshot the inject target under the registry lock, then
        // RELEASE it before the (up to 5s + payload-scaled) blocking PTY write —
        // never hold the registry across inject. #1441: registry is UUID-keyed.
        let inject_snap = {
            let reg = agent::lock_registry(registry);
            crate::fleet::resolve_uuid(home, target)
                .and_then(|id| reg.get(&id))
                .map(|h| (agent::InjectTarget::from_handle(h), h.name.to_string()))
        };
        if let Some((tgt, name)) = inject_snap {
            // #1769: not a daemon auto-nudge (operator/relay message) → no marker.
            if let Err(e) =
                agent::inject_with_target_gated(&tgt, &name, message.as_bytes(), false, None)
            {
                tracing::warn!(error = %e, "replay inject failed");
            }
        } else {
            persist_or_log!(
                crate::inbox::enqueue_with_idle_hint(
                    home,
                    target,
                    crate::inbox::InboxMessage::new_system(
                        "system:schedule",
                        "schedule_replay",
                        message,
                    ),
                ),
                "schedule_replay",
                target
            );
        }
    }
}

/// Staggered-spawn delay — rate-limits PTY init during multi-agent startup
/// bursts. Production value is a fixed 500 ms.
///
/// `AGEND_SPAWN_STAGGER_MS` is a **test-only seam, NOT a production tunable**
/// (#env-cleanup): the daemon is a separate process, so a cross-process
/// integration test that spawns it has env as its only lever to set a
/// deterministic stagger (e.g. `tests/ready_marker_invariants` /
/// `tests/attached_path_mcp_invariants` pin a specific value to create a
/// reproducible startup-race window). Operators never set it.
fn spawn_stagger() -> std::time::Duration {
    // test-only seam (see fn doc): prod always falls through to the 500ms default.
    let ms = crate::env_util::env_parse::<u64>("AGEND_SPAWN_STAGGER_MS", 500);
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
    // #1915 chokepoint (boot path): skip an instance deleted mid-boot BEFORE the
    // skills-install below re-creates `workspace/<name>`. The boot loop iterates a
    // fleet snapshot with a ~500ms inter-spawn stagger; a delete in that window
    // must not be resurrected. Complements the #1918 (b) `instance_is_known`
    // recheck in the spawn loop — that catches a delete that already removed the
    // fleet.yaml entry; this deleting-set check covers the in-flight teardown
    // (entry not yet removed). Leaf-lock check, no registry lock held.
    if crate::agent::deleting::is_deleting(home, name) {
        tracing::info!(
            agent = %name,
            "skipping spawn — instance is mid-delete (#1915 deleting-set chokepoint)"
        );
        return Ok(());
    }
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
        let custom_skills_source: Option<std::path::PathBuf> =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                .ok()
                .and_then(|c| c.instances.get(name).and_then(|i| i.skills_path.clone()))
                .map(|p| crate::fleet::resolve::expand_tilde_path(&p));
        let backend_skill =
            crate::backend::Backend::from_command(command).and_then(|b| b.skill_dir_name());
        match crate::skills::install_for_agent_backend_with_source(
            home,
            wd,
            skills_filter.as_deref(),
            backend_skill,
            custom_skills_source.as_deref(),
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

    // #1744-H2: rehydrate persisted escalation state onto the freshly-spawned
    // tracker (this is the boot / agent-register path — Resume mode). A daemon
    // restart otherwise re-zeroes the crash budget, the Hung confirm-window, and
    // the notify cooldowns; re-applying the last-persisted snapshot keeps those
    // P0 gates correct across the restart. (The in-daemon crash-respawn path
    // carries health via its own in-mem `saved_health` clone, so it does not go
    // through here.)
    if let Some(snapshot) = escalation_persist::load_for(home, name) {
        if let Some(id) = crate::fleet::resolve_uuid(home, name) {
            let reg = agent::lock_registry(registry);
            if let Some(handle) = reg.get(&id) {
                handle.core.lock().health.rehydrate_escalation(&snapshot);
                tracing::info!(agent = %name, "#1744-H2: rehydrated escalation state from store");
            }
        }
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
/// Decision §1 selective restore (5 fields): `crash_times`,
/// `total_crashes`, `last_crash_notification`, `last_hung_notification`,
/// `recovery_restart_count` (+1).
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
                // #1744-H3: the former shared `last_notification` is now two
                // per-class cooldowns — preserve both across the Stage-2 respawn.
                core.health.last_crash_notification,
                core.health.last_hung_notification,
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

    let backoff = Duration::from_millis(crate::health::STAGE2_BACKOFF_DEFAULT_MS);

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
        let custom_skills_source: Option<std::path::PathBuf> =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                .ok()
                .and_then(|c| c.instances.get(name).and_then(|i| i.skills_path.clone()))
                .map(|p| crate::fleet::resolve::expand_tilde_path(&p));
        let backend_skill = crate::backend::Backend::from_command(&config.backend_command)
            .and_then(|b| b.skill_dir_name());
        if let Err(e) = crate::skills::install_for_agent_backend_with_source(
            home,
            wd,
            skills_filter.as_deref(),
            backend_skill,
            custom_skills_source.as_deref(),
        ) {
            tracing::warn!(
                target: "recovery_shadow",
                agent = %name, error = %e, "stage2 skills install failed"
            );
        }
    }

    // #1547 M2(b): re-run MCP config on the recovery/respawn path (idempotent).
    // Critical for agy: its HOME-level discovery cache means a respawn that does
    // NOT re-configure comes back WITHOUT fleet tools (recovery is exactly when
    // they're needed). `configure_agy` rewrites `.agents/mcp_config.json` and
    // busts the discovery cache; for other backends this is a harmless
    // idempotent rewrite of the project-local config. Mirrors the skills
    // re-install above (the spawn path's config-generation is otherwise only
    // run on the INITIAL spawn, not here).
    if let Some(ref wd) = config.working_dir {
        crate::mcp_config::configure(wd, &config.backend_command, Some(name));
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
        Ok(_) => {
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
                let (crash_times, total_crashes, last_crash_notif, last_hung_notif, prev_count) =
                    saved;
                core.health.crash_times = crash_times;
                core.health.total_crashes = total_crashes;
                core.health.last_crash_notification = last_crash_notif;
                core.health.last_hung_notification = last_hung_notif;
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

    // ── #bughunt-r1 #1: post-grace SIGKILL must not hit clean exits ─────────

    #[test]
    fn grace_disposition_reaps_clean_exit_does_not_hard_kill() {
        // A child that exited cleanly within the grace window → ReapOnly. This
        // is the crux of the fix: it must NOT escalate to kill_process_tree,
        // whose PID may have been reused by an unrelated process.
        assert_eq!(grace_disposition(false), GraceDisposition::ReapOnly);
        // A holdout still alive after the grace window → HardKill.
        assert_eq!(grace_disposition(true), GraceDisposition::HardKill);
    }

    /// Source-scan invariant: in `terminate_agents_parallel` (the shared
    /// parallel-teardown core, extracted from `shutdown_sequence` for
    /// restart-freeze 真嫌#1), `kill_process_tree` must only be reachable via the
    /// `GraceDisposition::HardKill` arm — never an unconditional call (the bug).
    /// Guards against a regression that bypasses `grace_disposition` and SIGKILLs
    /// every child's (possibly-reused) PID.
    #[test]
    fn shutdown_kill_process_tree_only_in_hard_kill_arm_bughunt_r1() {
        let src = include_str!("mod.rs");
        let start = src
            .find("pub(crate) fn terminate_agents_parallel(")
            .expect("terminate_agents_parallel present");
        let after = &src[start..];
        // Scope to the fn body up to the start of the #[cfg(test)] module.
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let end = after.find(&cfg_test).unwrap_or(after.len());
        let body = &after[..end];

        let hard_kill_arm = body
            .find("GraceDisposition::HardKill =>")
            .expect("shutdown_sequence must branch on GraceDisposition::HardKill");
        let kill_call = body
            .find("kill_process_tree(")
            .expect("shutdown_sequence still calls kill_process_tree for holdouts");
        assert!(
            kill_call > hard_kill_arm,
            "#bughunt-r1 #1: kill_process_tree must appear only inside the \
             GraceDisposition::HardKill arm, never unconditionally (a clean exit \
             during the grace window must be reaped, not SIGKILL'd)"
        );
    }

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

    /// A1: `write_daemon_id` publishes the `.daemon` identity via
    /// `store::atomic_write`, so a concurrent `read_daemon_pid` liveness probe
    /// never sees a torn `pid:now:token` record. DISCRIMINATING via a side
    /// effect unique to `atomic_write`: it `create_dir_all`s the parent, so a
    /// write into a not-yet-created run_dir succeeds and round-trips — the
    /// pre-fix plain `std::fs::write` (no parent creation, error swallowed by
    /// `let _ =`) would leave no file at all.
    #[test]
    fn write_daemon_id_atomic_write_roundtrip_a1() {
        let home = tmp_home("a1-daemon-id");
        let run_dir = home.join("run-not-yet-created");
        assert!(!run_dir.exists());
        write_daemon_id(&run_dir);
        assert_eq!(
            read_daemon_pid(&run_dir),
            Some(std::process::id()),
            "atomic_write must create run_dir + publish a complete .daemon record"
        );
        let raw = std::fs::read_to_string(run_dir.join(".daemon")).expect(".daemon present");
        assert_eq!(
            raw.split(':').count(),
            3,
            "complete pid:now:token record expected, got {raw:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1814 Stage-2 (#t-27) source-scan invariant: the three shared-state
    /// GC/migration steps must NOT live in the pre-flock `init_daemon_services`
    /// (which runs before the predecessor exits on the handoff path), and the
    /// handoff path must invoke `init_daemon_services_post_lock` only AFTER it
    /// acquires the flock. Regression-proof: move any of the three back into
    /// `init_daemon_services`, or call post_lock before the lock, and this fails.
    #[test]
    fn handoff_shared_state_init_runs_post_flock_not_pre_t27() {
        let src = include_str!("mod.rs");
        const NEEDLES: [&str; 3] = [
            "migrate_legacy_tasks_json_to_event_log",
            "cleanup_stale_stages",
            "cleanup_tmp_orphans",
        ];

        // Helper: body of a fn from its signature up to the next top-level `fn `.
        let body_of = |sig: &str| -> String {
            let start = src.find(sig).unwrap_or_else(|| panic!("{sig} present"));
            let rest = &src[start..];
            let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
            rest[..end].to_string()
        };

        // (1) pre-flock init does NOT run any shared-state mutation.
        let pre = body_of("fn init_daemon_services(");
        for n in NEEDLES {
            assert!(
                !pre.contains(n),
                "#t-27: pre-flock init_daemon_services must NOT run `{n}` (escapes minimal pre-lock)"
            );
        }

        // (2) post-lock init owns all three.
        let post = body_of("fn init_daemon_services_post_lock(");
        for n in NEEDLES {
            assert!(
                post.contains(n),
                "#t-27: init_daemon_services_post_lock must run `{n}`"
            );
        }

        // (3) on the handoff path, post_lock is invoked AFTER flock acquisition
        //     (the last post_lock call site is the handoff one; the earlier one
        //     is the normal-boot arm, which already holds the flock).
        let acquire = src
            .find("acquire_daemon_lock_blocking(")
            .expect("handoff acquires the flock");
        let handoff_post = src
            .rfind("init_daemon_services_post_lock(home)")
            .expect("handoff calls post_lock");
        assert!(
            handoff_post > acquire,
            "#t-27: the handoff path must call init_daemon_services_post_lock AFTER \
             acquire_daemon_lock_blocking, never pre-flock"
        );
    }

    /// #t-27: `init_daemon_services_post_lock` runs end-to-end (all three steps)
    /// on a clean home — the legacy migration is a no-op success (no tasks.json),
    /// the GC steps are retention-gated. Exercises the real extracted fn.
    #[test]
    fn post_lock_init_ok_on_clean_home_t27() {
        let home = tmp_home("t27-postlock-clean");
        init_daemon_services_post_lock(&home).expect("post_lock init must succeed on a clean home");
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

    /// #1720 app-mode root fix: `register_event_subscribers` is the SINGLE
    /// source of truth for the event-bus subscriber list (run_core, app::run_app,
    /// and the test harness all route through it). Pin every one of the 12
    /// per-pattern registrations so a dropped line — which would silently kill
    /// that pattern's delivery in BOTH prod modes at once — fails CI. Source-level
    /// pin (cross-platform-safe; survives rustfmt). When adding a pattern, add it
    /// here AND to `register_event_subscribers`.
    #[test]
    fn register_event_subscribers_lists_every_pattern() {
        let src = std::fs::read_to_string("src/daemon/mod.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/daemon/mod.rs"))
            .expect("source file must be readable from test cwd");
        let start = src
            .find("pub(crate) fn register_event_subscribers")
            .expect("register_event_subscribers must exist");
        let body = &src[start..(start + 1200).min(src.len())];
        for pat in [
            "anti_stall::register_subscriber",
            "decision_timeout::register_subscriber",
            "dispatch_idle::register_subscriber",
            "waiting_on_stale::register_subscriber",
            "helper_staleness_watchdog::register_subscriber",
            "idle_watchdog::register_subscriber",
            "tasks::register_cascade_subscriber",
            "poll_reminder::register_subscriber",
            "cron_tick::register_subscriber",
            "supervisor::register_subscriber",
            "conflict_notify::register_subscriber",
            "ci_watch::register_subscriber",
        ] {
            assert!(
                body.contains(pat),
                "register_event_subscribers must register '{pat}' — a missing \
                 pattern silently breaks its delivery in app + daemon mode (#1720)"
            );
        }
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
        // CR-2026-06-14: format is now `{pid}:{boot_unix}:{start_token}`.
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], std::process::id().to_string());
        // Timestamp should be a positive number
        let ts: u64 = parts[1].parse().expect("parse timestamp");
        assert!(ts > 0);
        // Start-token parses; for our own (alive) PID it resolves non-zero.
        let token: u64 = parts[2].parse().expect("parse start_token");
        assert_eq!(
            token,
            crate::process::process_start_token(std::process::id()).unwrap_or(0),
            "recorded token must equal the live self start-token"
        );
        // The middle-field reader must still parse boot_unix (not "ts:token").
        assert_eq!(read_daemon_boot_unix(&run), Some(ts));
        // The new third-field reader returns the recorded token.
        assert_eq!(read_daemon_start_token(&run), Some(token));
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

    /// #1814 FIX1 (reviewer race High): a run dir with an alive pid but NO
    /// `.daemon` identity file (= a handoff successor that has published its api
    /// pre-flock but NOT yet promoted) must NOT be discoverable via
    /// `find_active_run_dir`. Otherwise generic CLI/MCP clients could route to a
    /// half-promoted, no-agent successor during the overlap window (split-brain).
    #[test]
    fn find_active_run_dir_skips_dir_without_daemon_identity() {
        let home = tmp_home("no_daemon_identity");
        let pid = std::process::id(); // this test process — guaranteed alive
        let run = home.join("run").join(pid.to_string());
        std::fs::create_dir_all(&run).ok();
        // Successor pre-promote shape: api.port published, but NO `.daemon`.
        crate::ipc::write_port(&run, crate::ipc::API_NAME, 65000).ok();
        assert!(
            find_active_run_dir(&home).is_none(),
            "a run dir without a `.daemon` identity must not be discoverable (pre-promote successor)"
        );
        // The dir must survive (it's a live successor mid-handoff, not stale).
        assert!(
            run.exists(),
            "must NOT delete the un-promoted successor's run dir"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1814 FIX1 companion: once the `.daemon` identity (matching pid) IS
    /// present (= the successor promoted post-flock), the dir becomes
    /// discoverable again — the normal-daemon path is unchanged.
    #[test]
    fn find_active_run_dir_returns_dir_with_valid_daemon_identity() {
        let home = tmp_home("valid_daemon_identity");
        let pid = std::process::id();
        let run = home.join("run").join(pid.to_string());
        std::fs::create_dir_all(&run).ok();
        write_daemon_id(&run); // writes `<pid>:<ts>` for the current (alive) pid
        assert_eq!(
            find_active_run_dir(&home).as_deref(),
            Some(run.as_path()),
            "a run dir with a valid matching `.daemon` must be discoverable"
        );
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

    // ── Clean exit vs crash respawn ──────────────────────────────────
    // The earlier trio asserted only language / HashMap discriminant semantics
    // (re-implementing the main loop's `match` inline) and never touched
    // production, so a regression in the real handlers went uncaught. They
    // collapse to two tests that drive the REAL handlers: a clean exit evicts
    // the respawn config (so crash-respawn finds nothing to resurrect), and a
    // crash's respawn DECISION says "respawn" on a fresh agent. `AgentHandle`
    // needs a live PTY and can't be built in a unit test, so the
    // registry-removal half of `handle_clean_exit` is reached only via the prod
    // dispatch; the config eviction is the unit-testable contract.

    #[test]
    fn clean_exit_evicts_respawn_config_so_no_resurrect() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let home = std::env::temp_dir().join(format!(
            "agend-cleanexit-{}-{}",
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&home).unwrap();

        let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
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

        // Drive the REAL CleanExit handler (no fleet.yaml → resolve_uuid is None,
        // so the registry half is a no-op here; the config eviction must run).
        handle_clean_exit(&home, "agent-3", &registry, &configs);

        assert!(
            !configs.lock().contains_key("agent-3"),
            "clean exit must evict the respawn config so crash-respawn finds nothing to resurrect"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn crash_still_respawns() {
        // The Crash arm respawns iff the health gate says so. Drive the REAL
        // decision (`HealthTracker::record_crash`): a fresh agent's first crash
        // returns respawn=true — the opposite of a clean exit, which never
        // respawns. The max-retries → no-respawn case is covered by
        // `health::tests::test_failed_after_max_retries`.
        let mut health = crate::health::HealthTracker::new();
        let (respawn, _delay, _notify) = health.record_crash();
        assert!(
            respawn,
            "a fresh agent's first crash must respawn (the gate handle_crash_respawn relies on)"
        );
    }

    // These four drive the REAL production `crate::agent::classify_exit` (now
    // pub(crate)) instead of re-implementing its `match` inline, so a refactor
    // that reclassifies an exit code is actually caught.
    use crate::agent::{classify_exit, ExitKind};

    #[test]
    fn sigint_130_treated_as_clean_exit() {
        // SIGINT (exit code 130 = 128+2) from /quit in some CLIs must be
        // treated as clean exit, not crash.
        assert_eq!(
            classify_exit(Some(130)),
            ExitKind::UserExit,
            "exit code 130 (SIGINT) must be a user-initiated clean exit"
        );
    }

    #[test]
    fn sigkill_137_not_clean_exit() {
        // SIGKILL (137) is daemon-initiated, not user /exit.
        assert_eq!(
            classify_exit(Some(137)),
            ExitKind::SignalKill,
            "SIGKILL (137) must classify as SignalKill, not a clean UserExit"
        );
    }

    #[test]
    fn sigterm_143_not_clean_exit() {
        // SIGTERM (143) is daemon-initiated, not user /exit.
        assert_eq!(
            classify_exit(Some(143)),
            ExitKind::SignalKill,
            "SIGTERM (143) must classify as SignalKill, not a clean UserExit"
        );
    }

    #[test]
    fn nonzero_exit_is_crash() {
        // Exit code 1 (error) must trigger crash respawn.
        assert_eq!(
            classify_exit(Some(1)),
            ExitKind::Crash,
            "exit code 1 must classify as a crash"
        );
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

    /// #t-41673 gap-instrument: the shutdown-complete log MUST carry the new
    /// `shutdown_elapsed_ms` field so the shutdown half of the restart freeze is
    /// attributable separately from the old-exit→new-launch gap. Pure-tracing
    /// instrument — this asserts the field is actually emitted (drop the field →
    /// RED). `shutdown_sequence` treats `home` as reserved telemetry (`let _ =
    /// home`), so a throwaway path suffices; the empty registry means no agents
    /// are spawned/killed (still pays the 2s grace window).
    #[cfg(unix)]
    #[test]
    #[tracing_test::traced_test]
    fn shutdown_sequence_emits_shutdown_elapsed_ms() {
        let (registry, _configs, _tx, _rx, _shutdown) = make_test_registry();
        let _ = shutdown_sequence(
            std::path::Path::new("/tmp"),
            &registry,
            std::time::Instant::now(),
        );
        assert!(
            logs_contain("daemon shutdown sequence complete"),
            "gap-instrument: shutdown-complete log should still fire"
        );
        assert!(
            logs_contain("shutdown_elapsed_ms"),
            "gap-instrument: shutdown-complete log must carry shutdown_elapsed_ms"
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

    /// restart-freeze 真嫌#1 (t-…55279): the shared parallel-teardown core must
    /// kill and reap N agents in about one grace window, not N times a per-agent
    /// sequential wait (the ~6 s app-restart freeze). Spawn several real
    /// SIGTERM-responsive shells, drain to `(name, child)` exactly as
    /// `shutdown_sequence` and `app_teardown` do, then assert bounded wall time
    /// (a per-agent wait would scale with N), zero post-grace SIGKILLs (shells
    /// honour SIGTERM), and every child reaped (no zombies).
    #[cfg(unix)]
    #[test]
    fn terminate_agents_parallel_bounded_and_reaps_all() {
        let home = tmp_home("terminate_parallel");
        let _run_dir = setup_run_dir_with_cookie(&home);
        let names = ["t1", "t2", "t3", "t4", "t5", "t6"];
        seed_fleet_ids(&home, &names);
        let (registry, configs, crash_tx, _crash_rx, shutdown) = make_test_registry();
        for n in &names {
            spawn_and_register_agent(
                &home,
                &make_shell_agent_def(n),
                &registry,
                &configs,
                &crash_tx,
                &shutdown,
            )
            .expect("spawn ok");
        }

        // Drain to (name, child) — identical to shutdown_sequence / app_teardown.
        let agents: Vec<(String, ChildHandle)> = {
            let mut reg = registry.lock();
            reg.drain()
                .map(|(_id, h)| (h.name.to_string(), h.child))
                .collect()
        };
        assert_eq!(agents.len(), names.len());
        let children: Vec<ChildHandle> = agents.iter().map(|(_, c)| Arc::clone(c)).collect();

        let start = std::time::Instant::now();
        let killed = terminate_agents_parallel(agents);
        let elapsed = start.elapsed();

        // Bound: one grace window + CI margin. Grace-exiters are reaped in place
        // by `try_wait`, so the reap loop is ~instant; a regression to the
        // is_pid_alive gate (zombie reads alive → kill_process_tree's 500 ms per
        // agent, serialized) would add ~N×0.5 s ≈ 3 s for these 6 agents and
        // blow this bound.
        assert!(
            elapsed < SHUTDOWN_GRACE + std::time::Duration::from_secs(2),
            "parallel teardown of {} agents took {elapsed:?}; expected ≈ one grace window \
             (regression: per-agent kill_process_tree 500ms scaling with N)",
            names.len()
        );
        // `sleep 60` honours SIGTERM (default-terminate) → all exit within grace.
        assert_eq!(
            killed, 0,
            "SIGTERM-responsive shells should exit during grace"
        );
        for c in &children {
            assert!(
                matches!(c.lock().try_wait(), Ok(Some(_))),
                "every child must be reaped after terminate_agents_parallel"
            );
        }

        std::fs::remove_dir_all(&home).ok();
    }

    /// #1915 boot-path chokepoint: `spawn_and_register_agent` must SKIP an
    /// instance that is mid-delete (the boot-stagger resurrection: the loop holds
    /// a fleet snapshot, an instance deleted during the stagger must not be
    /// re-spawned + have its `workspace/<name>` re-created by skills-install).
    /// Also proves the deleting-set does NOT leak the name: after the delete
    /// completes (guard drop), a re-create of the SAME name spawns normally.
    #[cfg(unix)]
    #[test]
    fn spawn_and_register_agent_skips_mid_delete_1915() {
        let home = tmp_home("mid_delete");
        let run_dir = setup_run_dir_with_cookie(&home);
        seed_fleet_ids(&home, &["victim"]);
        let (registry, configs, crash_tx, _crash_rx, shutdown) = make_test_registry();
        let def = make_shell_agent_def("victim");

        // Mark victim mid-delete (as full_delete_instance's guard would).
        let guard = crate::agent::deleting::mark_deleting(&home, "victim");

        spawn_and_register_agent(&home, &def, &registry, &configs, &crash_tx, &shutdown)
            .expect("returns Ok (clean skip, not Err)");
        assert!(
            crate::ipc::read_port(&run_dir, "victim").is_none(),
            "#1915: a mid-delete instance must NOT be spawned — no port published"
        );
        assert!(
            registry.lock().is_empty(),
            "#1915: a mid-delete instance must NOT be registered — no resurrection"
        );

        // Delete completes → guard drops → name un-marked → re-create succeeds
        // (deleting-set must not leave the name permanently un-spawnable).
        drop(guard);
        spawn_and_register_agent(&home, &def, &registry, &configs, &crash_tx, &shutdown)
            .expect("re-create after delete spawns");
        assert!(
            crate::ipc::read_port(&run_dir, "victim").is_some(),
            "#1915 no-leak: same name re-creatable once the delete (guard) is done"
        );

        kill_registered_child(&registry, "victim");
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

        // Backoff is the fixed STAGE2_BACKOFF_DEFAULT_MS const; the spawned
        // worker sleeps it on its own thread, so the caller must still return
        // immediately (that non-blocking contract is what this test pins).
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

        std::fs::remove_dir_all(&home).ok();
    }
}
