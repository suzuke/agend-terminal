//! Daemon: manages agent registry, TUI sockets, auto-respawn, fleet lifecycle,
//! schedule checking, health monitoring, Telegram notifications.

pub(crate) mod anti_stall;
pub(crate) mod assignment_authority;
pub(crate) mod auto_release;
pub(crate) mod boot_sweep;
pub(crate) mod cadence_gate;
pub(crate) mod canonical_drift;
pub(crate) mod channel_reply_discharge;
pub(crate) mod ci_delivery_ledger;
pub(crate) mod ci_handoff_track;
pub(crate) mod ci_watch;
pub(crate) mod conflict_notify;
mod crash_respawn;
pub(crate) mod cron_tick;
pub(crate) mod decision_board_timeout;
pub(crate) mod decision_timeout;
pub(crate) mod dedup_state;
pub(crate) mod delivery_worker;
pub(crate) mod discharge_ledger;
pub(crate) mod dispatch_idle;
pub(crate) mod escalation_persist;
pub(crate) mod event_bus;
pub(crate) mod event_hub;
pub(crate) mod handoff_timeout_watchdog;
pub(crate) mod heartbeat_pair;
pub(crate) mod helper_staleness_watchdog;
pub mod hook_shadow;
pub(crate) mod hygiene_task;
pub(crate) mod idle_watchdog;
pub(crate) mod inbox_stuck_watchdog;
pub(crate) mod inject_delivery;
pub(crate) mod janitor;
pub(crate) mod lifecycle;
pub(crate) mod mcp_registry_watcher;
pub(crate) mod notification_dedup;
pub(crate) mod orphan_sweep;
pub(crate) mod owned_maintenance;
pub(crate) mod owner_services;
pub(crate) mod per_tick;
pub(crate) mod poll_reminder;
pub(crate) mod pr_state;
pub(crate) mod restart;
pub(crate) mod retention;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod revoke_assignment_tests;
pub(crate) mod router;
/// #2413 Shadow Observer — local plane (claude hooks side-channel). Spike, flag-OFF.
pub mod shadow;
pub(crate) mod shutdown_cleanup;
pub(crate) mod supervisor;
pub(crate) mod task_progress;
pub(crate) mod task_sweep;
pub(crate) mod tick_stall;
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
    /// #3499: the daemon's `bootstrap::signals::install` now installs a
    /// dedicated SIGHUP handler that does NOT shut the daemon down (a
    /// detached daemon must survive losing its controlling terminal), so
    /// this reason is never recorded by that handler — SIGHUP simply does
    /// not trigger a shutdown, bundled or otherwise. Kept in the taxonomy
    /// (not deleted) in case a future caller records it deliberately.
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
    pub backend: Option<crate::backend::Backend>,
    pub backend_command: String,
    pub args: Vec<String>,
    pub env: Option<HashMap<String, String>>,
    pub working_dir: Option<PathBuf>,
    pub submit_key: String,
}

#[cfg(test)]
mod runtime_config_convergence_tests;

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

/// Mint the daemon identity record used for PID reuse detection.
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
pub(crate) fn daemon_identity() -> String {
    let pid = std::process::id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let token = crate::process::process_start_token(pid).unwrap_or(0);
    format!("{pid}:{now}:{token}")
}

/// Write daemon identity file for PID reuse detection.
pub(crate) fn write_daemon_id(run_dir: &Path) {
    write_daemon_id_value(run_dir, &daemon_identity());
}

/// Publish a previously minted identity. Successor handoff uses this after
/// flock promotion so the EventHub source and `.daemon` record are identical
/// while the run directory remains undiscoverable before promotion.
pub(crate) fn write_daemon_id_value(run_dir: &Path, identity: &str) {
    // A1: atomic write — a plain `fs::write` can be read mid-write (torn) by a
    // concurrent `read_daemon_pid`/liveness probe, which then parse-fails on a
    // truncated `pid:now:token` record. `store::atomic_write` publishes via a
    // unique tmp + rename so readers only ever see a complete record.
    let _ = crate::store::atomic_write(&run_dir.join(".daemon"), identity.as_bytes());
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
    Option<crate::backend::Backend>,
);

/// Start daemon: do preflight (lock, run dir, cookie) then run the core loop.
///
/// Used by the `Commands::Daemon { agents }` escape hatch path (no fleet.yaml).
/// The fleet-driven path uses [`run_with_prepared`] instead, which skips the
/// preflight because [`crate::bootstrap::prepare`] has already done it.
pub fn run(home: &Path, agents: Vec<AgentDef>) -> anyhow::Result<()> {
    // Acquire exclusive daemon lock (prevents TOCTOU race).
    //
    // Was a second, hand-rolled copy of the flock dance. Unified onto
    // `bootstrap::acquire_daemon_lock` so both singleton entry points report
    // contention as the same typed `DaemonAlreadyRunning` — otherwise a caller
    // that fails closed on the bootstrap error would still degrade on this one.
    // The guard is bound for the rest of `run`, matching the previous
    // `lock_file` lifetime.
    std::fs::create_dir_all(home)?;
    let _daemon_lock = crate::bootstrap::acquire_daemon_lock(home)?;

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

// #2538: `build_default_handlers` relocated to `per_tick::build_default_handlers`
// (move-only, zero behavior change) — this grandfathered file was already at its
// LOC ceiling (3217, `tests/src_file_size_invariant.rs`) with zero slack, so a
// new handler registration couldn't land here without breaking the anti-monolith
// invariant. The function only ever constructed `Vec<Box<dyn per_tick::PerTickHandler>>`
// from `per_tick::*` types, so `per_tick/mod.rs` (492 LOC, nowhere near its 2500
// cap) is also the conceptually correct home — same precedent as
// `dispatch_exit_event_guarded`, already relocated there for the identical reason.
// Re-exported below so all 5 existing call sites (`crate::daemon::build_default_handlers`)
// stay byte-identical.
pub(crate) use per_tick::build_default_handlers;

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
        resume_requester: Option<crate::types::InstanceId>,
        source_id: String,
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
    // Mint the identity before API/EventHub construction, but do not publish
    // it to `.daemon` until the successor owns the flock below. The exact
    // value is carried through the handoff and written after promotion.
    let source_id = daemon_identity();
    run_core(
        home,
        FleetSource::HandoffDeferred {
            fleet_path: fleet_path.to_path_buf(),
            opts: crate::bootstrap::PrepareOptions::default(),
            resume_requester: crate::daemon::restart::successor_requester_id(),
            source_id,
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
    // #P1-2607-followup (reviewer4, PR #2620): `remove_and_unregister`, not a
    // bare registry remove — see `daemon::lifecycle::delete_transaction`'s
    // comment for why.
    if let Some(id) = crate::fleet::resolve_uuid(home, name) {
        agent::remove_and_unregister(registry, &id);
    }
    configs.lock().remove(name);
}

fn run_core(home: &Path, source: FleetSource) -> anyhow::Result<()> {
    let started_at = std::time::Instant::now();

    // PR-D6: fail LOUD if the retired `AGEND_WORKTREE_PRUNE_LIVE` is still set —
    // sweep gating is now `AGEND_WORKTREE_AUTO_CLEANUP` only. One warn at boot so
    // an operator carrying the stale flag learns it is ignored (not silent).
    crate::worktree_cleanup::warn_if_prune_live_retired();

    // For the handoff path, the channel inits post-lock (its registry attaches
    // via the #945 pending-registry bridge that `init_daemon_services` arms),
    // so pass None here; `Resolved` carries the already-inited channel.
    let telegram_pre = match &source {
        FleetSource::Resolved { telegram, .. } => telegram.clone(),
        FleetSource::HandoffDeferred { .. } => None,
    };

    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    crate::runtime_controls::reload_runtime_controls(home);
    let event_source_id = match &source {
        FleetSource::Resolved { .. } => crate::daemon::event_hub::source_id(&run_dir(home)),
        FleetSource::HandoffDeferred { source_id, .. } => source_id.clone(),
    };
    let ctx = init_daemon_services(home, telegram_pre, shutdown_tx.clone(), event_source_id)?;

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
    let (agents, _handoff_lock, resume_requester) = match source {
        FleetSource::Resolved { agents, .. } => {
            // Normal boot: the flock is already held by `prepare`'s `OwnedFleet`,
            // so the post-lock GC/migration runs here — the same early point as
            // before the #1814 Stage-2 split (behavior unchanged).
            init_daemon_services_post_lock(home)?;
            (agents, None::<crate::bootstrap::DaemonLock>, None)
        }
        FleetSource::HandoffDeferred {
            fleet_path,
            opts,
            resume_requester,
            source_id,
        } => {
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
            write_daemon_id_value(&run_dir(home), &source_id);
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
            (agents, Some(lock), resume_requester)
        }
    };

    // #2413/#2738 Shadow Observer — local plane: start the unix-socket hook-event
    // server (no-op under AGEND_SHADOW_OBSERVER=0; default-ON; observe-only side
    // channel, never blocks). Placed HERE — after the source match confirms SOLE
    // ownership (normal boot: `prepare`'s OwnedFleet flock; handoff: flock acquired
    // above) and immediately BEFORE spawn_fleet_agents. #2738: `start_unix` does a
    // destructive `remove_file(path)` + `bind(path)`; running it PRE-flock let a
    // handoff successor unlink+steal a live predecessor's socket pathname, then (if
    // it aborted before the flock) leave the predecessor orphaned — shadow plane
    // silently dead. Post-flock/pre-fleet preserves the invariant "bind after sole
    // ownership AND before this host forks its fleet" for both boot modes.
    crate::daemon::shadow::start(home);

    spawn_fleet_agents(home, &agents, &ctx);
    if let Some(requester_id) = resume_requester {
        spawn_handoff_requester_self_kick(home, &agents, &ctx, requester_id);
    }

    // #boot-orphan-live-lease: the initial full spawn is a synchronous barrier
    // (each successful agent is registered and ready before it returns). Take an
    // owned snapshot so the registry lock is dropped before binding locks, task
    // event fsyncs, and inbox writes. Do not repeat this at the recover-as-primary
    // respawn below: that path is not a fresh authoritative boot census.
    let live = crate::agent::live_agent_names(&ctx.registry);
    crate::tasks::release_inprogress_orphans_with_live(home, &live);

    crate::bootstrap::signals::install(Arc::clone(&ctx.shutdown), shutdown_tx);

    crate::event_log::log(
        home,
        "daemon_start",
        "",
        &format!("{} agents", agents.len()),
    );
    tracing::info!("running, Ctrl+C or `agend-terminal stop` to stop");

    let (keepalive, tick_rx) = build_tick_infrastructure(home, &ctx);

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

            // PR4: idle, blocked awaiting the next tick/crash/shutdown signal.
            keepalive.maintenance.enter_waiting();
            let exit_event: Option<crate::agent::AgentExitEvent>;
            crossbeam_channel::select! {
                recv(ctx.crash_rx) -> msg => { exit_event = msg.ok(); }
                recv(tick_rx) -> _ => { exit_event = None; }
                recv(shutdown_rx) -> _ => { continue; }
            }

            // #2935: dispatch maintenance only for tick wakes; crash wakes skip
            // the full pipeline and go straight to exit-event handling.
            let exit_event = serve_loop_post_select(
                &keepalive.maintenance,
                exit_event,
                home,
                &ctx.registry,
                &ctx.externals,
                &ctx.configs,
            );
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
            // AUDIT2-007: panic-isolated (helper lives in `per_tick` to keep this
            // grandfathered file under its anti-monolith ceiling).
            per_tick::dispatch_exit_event_guarded(exit_event, home, &ctx);
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
                "reason={} agents_total={} agents_killed_after_grace={} transports_cleaned={} transports_failed={} uptime_secs={}",
                metrics.reason.as_str(),
                metrics.agents_total,
                metrics.agents_killed_after_grace,
                metrics.transports_cleaned,
                metrics.transports_failed,
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
    shutdown_wake: crossbeam_channel::Sender<()>,
    event_source_id: String,
) -> anyhow::Result<DaemonContext> {
    const API_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
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
    } else if let Some(tg) = crate::channel::lookup_channel_by_name("telegram") {
        // Multi-channel-safe (t-20260703164240502572-50899-11): daemon-boot
        // twin of the app-mode fallback in `app/mod.rs` — telegram-specific
        // by naming, so `active_channel()` silently no-opping once discord
        // is also registered would leave telegram's registry attach dead.
        tg.attach_registry(Arc::clone(&registry));
    }

    let externals: crate::agent::ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (crash_tx, crash_rx) = crossbeam_channel::bounded::<crate::agent::AgentExitEvent>(64);
    crate::agent::crash_disposition::install_owner_crash_wake(crash_tx.clone());
    let configs: Arc<Mutex<HashMap<String, AgentConfig>>> = Arc::new(Mutex::new(HashMap::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    // fire-and-forget: api::serve runs the loopback TCP accept loop, which blocks
    // in accept() for the daemon's lifetime. It never polls the shutdown flag —
    // its only early exit is a persistent accept-error streak — so process exit
    // is what ends and reaps the thread. The JoinHandle is dropped because a
    // join would block until then.
    let api_reg = Arc::clone(&registry);
    let api_home = home.to_path_buf();
    let api_shutdown = Arc::clone(&shutdown);
    let api_configs = Arc::clone(&configs);
    let api_externals = Arc::clone(&externals);
    let event_hub = crate::daemon::event_hub::EventHub::new(event_source_id, 128);
    let api_event_hub = Arc::clone(&event_hub);
    let (api_ready_tx, api_ready_rx) = std::sync::mpsc::sync_channel(1);
    // fire-and-forget: the API accept loop blocks in accept() for the daemon's lifetime; process exit reaps the detached worker.
    std::thread::Builder::new()
        .name("api_server".into())
        .spawn(move || {
            crate::api::serve_with_ready_events(
                &api_home,
                api_reg,
                api_shutdown,
                api_configs,
                api_externals,
                api_event_hub,
                crate::api::RestartCapability::Daemon,
                None, // #2453 R2: no app-restart channel on the headless daemon
                api_ready_tx,
                Some(shutdown_wake),
            )
        })?;

    match api_ready_rx.recv_timeout(API_READY_TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => anyhow::bail!("API server failed to start: {error}"),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => anyhow::bail!(
            "API server did not publish a ready listener within {}s",
            API_READY_TIMEOUT.as_secs()
        ),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            anyhow::bail!("API server exited before reporting readiness")
        }
    }

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
        let dir = run_dir(home);
        if let Err(e) = crate::ready::write(&dir) {
            tracing::warn!(
                path = %dir.join(crate::ready::FILENAME).display(),
                error = %e,
                "failed to write .ready marker"
            );
        }
    });
}

/// Resume exactly the managed caller that committed this daemon handoff. The
/// requester UUID crosses the predecessor/successor boundary explicitly; a
/// cold boot has no value and therefore schedules no first turn.
fn spawn_handoff_requester_self_kick(
    home: &Path,
    agents: &[AgentDef],
    ctx: &DaemonContext,
    requester_id: crate::types::InstanceId,
) {
    let Some(name) = crate::fleet::resolve_name_by_uuid(home, &requester_id.full()) else {
        tracing::warn!(%requester_id, "restart requester absent from fleet — skipping self-kick");
        return;
    };
    let Some(def) = agents.iter().find(|def| def.0 == name) else {
        tracing::warn!(agent = %name, %requester_id, "restart requester absent from resolved fleet — skipping self-kick");
        return;
    };
    if !ctx.registry.lock().contains_key(&requester_id) {
        tracing::warn!(agent = %name, %requester_id, "restart requester did not spawn — skipping self-kick");
        return;
    }
    let ready_timeout = def
        .6
        .as_ref()
        .map(|backend| backend.preset().ready_timeout_secs)
        .unwrap_or(60)
        .saturating_add(15);
    let _ = crate::agent::spawn_self_kick_bootstrap(
        Arc::clone(&ctx.registry),
        requester_id,
        name,
        home.to_path_buf(),
        std::time::Duration::from_secs(ready_timeout),
        crate::agent::BootstrapRegistrationState::AlreadyRegistered,
        Some(Arc::clone(&ctx.shutdown)),
    );
}

/// #2935: post-select dispatch for the serve loop. Runs the periodic maintenance
/// pipeline only for tick wakes (exit_event is None); crash wakes (Some) skip it.
/// Returns the exit event unchanged so the caller can dispatch it.
fn serve_loop_post_select(
    maintenance: &crate::daemon::owned_maintenance::OwnedMaintenanceCycle,
    exit_event: Option<crate::agent::AgentExitEvent>,
    home: &Path,
    registry: &AgentRegistry,
    externals: &crate::agent::ExternalRegistry,
    configs: &crate::api::ConfigRegistry,
) -> Option<crate::agent::AgentExitEvent> {
    if exit_event.is_none() {
        maintenance.run_once(home, registry, externals, configs);
    }
    exit_event
}

/// Opaque bag of daemon-lifetime handles that must not be dropped
/// until the main loop exits.
struct TickKeepalive {
    _task_sweep: crate::daemon::task_sweep::TaskSweep,
    maintenance: crate::daemon::owned_maintenance::OwnedMaintenanceCycle,
    _tick_driver: crate::daemon::ticker::MaintenanceTickDriver,
}

fn build_tick_infrastructure(
    home: &Path,
    ctx: &DaemonContext,
) -> (TickKeepalive, crossbeam_channel::Receiver<()>) {
    let _task_sweep =
        crate::daemon::task_sweep::TaskSweep::spawn(home.to_path_buf(), Arc::clone(&ctx.shutdown));

    #[cfg(unix)]
    {
        supervisor::spawn(home.to_path_buf(), Arc::clone(&ctx.registry));
    }
    router::spawn(home.to_path_buf(), Arc::clone(&ctx.registry));
    // #2453 Stage 1a / #2737: owner monitoring (instance_monitor +
    // api_activity_probe) via the typed phase-1 seam — identical position/args to
    // owned `run_app`. The daemon host always owns the fleet (no attached concept),
    // so role = Owned. The returned token is required by phase 2 (compile-enforced
    // order).
    let monitoring = crate::daemon::owner_services::start_owner_monitoring(
        crate::daemon::owner_services::OwnerRole::Owned,
        home,
        &ctx.registry,
        &crate::daemon::owner_services::OwnerMonitoringStarters::real(),
    );
    // #2453 Stage 1a / #2737: the three Shadow Observer stream planes (rollout +
    // opencode + kiro) via the typed phase-2 seam — identical call in owned
    // `run_app`. No-op under AGEND_SHADOW_OBSERVER=0 (default-ON). shadow::start
    // (the socket-ingest plane) stays host-local (separate fork).
    let owner_services = crate::daemon::owner_services::start_owner_stream_observers(
        crate::daemon::owner_services::OwnerRole::Owned,
        &monitoring,
        home,
        &ctx.registry,
        &crate::daemon::owner_services::OwnerStreamStarters::real(),
    );

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

    // `run_core` is headless (no TUI), so the `DaemonBinaryStale` flag the
    // `mcp_registry` handler flips is a throwaway here — nothing surfaces it,
    // exactly as the pre-W1.1 supervisor-side flag was in run_core.
    let daemon_binary_stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale =
        Arc::new(AtomicBool::new(false));
    let handlers = build_default_handlers(daemon_binary_stale);
    let maintenance = crate::daemon::owned_maintenance::OwnedMaintenanceCycle::new(
        handlers,
        owner_services,
        "daemon-tick",
        home,
    );

    let (_tick_driver, tick_rx) = crate::daemon::ticker::MaintenanceTickDriver::spawn(
        "daemon_tick",
        std::time::Duration::from_secs(10),
    );

    (
        TickKeepalive {
            _task_sweep,
            maintenance,
            _tick_driver,
        },
        tick_rx,
    )
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
///
/// #3508: extended with managed-transport cleanup counts so the
/// `daemon_stop` event distinguishes "accepted" (API SHUTDOWN) from
/// "completed" (all daemon-owned children + structured backends torn
/// down, or an audited failure receipt exists).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ShutdownMetrics {
    pub reason: ShutdownReason,
    pub agents_total: usize,
    pub agents_killed_after_grace: usize,
    pub uptime_secs: u64,
    pub transports_cleaned: usize,
    pub transports_failed: usize,
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
    tracing::info!(
        reason = reason.as_str(),
        event = "shutdown_initiated",
        "cleaning up..."
    );
    // #3508: capture instance names BEFORE draining so the transport
    // cleanup that follows knows exactly which daemon-owned backends to
    // tear down (in-memory + persisted locator dual path, PID safety,
    // process_group kill, ChannelBridge join). The drain below is still
    // the first mutation so PTY close handlers see "gone" and don't emit
    // crash events.
    let instance_names: Vec<String> = {
        let reg = agent::lock_registry(registry);
        reg.values().map(|handle| handle.name.to_string()).collect()
    };

    let (codex_checkpointed, codex_checkpoint_failed) =
        shutdown_cleanup::checkpoint_codex_sessions(home, &instance_names);
    tracing::info!(
        codex_checkpointed,
        codex_checkpoint_failed,
        event = "shutdown_codex_checkpoint_complete",
        "Codex thread checkpoint phase complete"
    );

    // Drain registry FIRST, then kill. PTY close handlers check the
    // registry — if the agent is gone, they return silently instead of
    // sending crash events. This eliminates all shutdown race conditions.
    let agents_to_kill: Vec<_> = {
        let mut reg = agent::lock_registry(registry);
        reg.drain()
            .map(|(_id, handle)| (handle.name.to_string(), handle.child))
            .collect()
    };
    let agents_total = agents_to_kill.len();

    // Parallel SIGTERM → single grace → SIGKILL/reap holdouts (shared core).
    // The per-agent `reap_ms` gap-instrument (#t-41673) lives inside the helper
    // so it covers app-mode teardown too.
    let agents_killed_after_grace = terminate_agents_parallel(agents_to_kill);

    // #3508: daemon-owned structured backends (Codex app-server via
    // process_group(0), OpenCode serve, ChannelBridge worker + state dir,
    // MCP bridge, durable latches/receipts) are NOT covered by the
    // agent-child SIGTERM above. They are torn down here via the single
    // audited transport cleanup entry that already handles in-memory
    // + persisted locator, PID-reuse safety, and namespace-bound
    // filesystem removal. This is the shutdown analogue of the
    // instance-deletion flow (daemon/lifecycle.rs, agent_ops.rs) which
    // previously was the ONLY caller.
    tracing::info!(
        instances = instance_names.len(),
        event = "shutdown_transport_cleanup_started",
        "tearing down managed transports for drained instances"
    );
    let (transports_cleaned, transports_failed) =
        shutdown_cleanup::cleanup_managed_transports(home, &instance_names);
    // Sweep any residual transport state that has no live instance
    // (e.g. a prior unclean shutdown left a session file behind).
    // This is best-effort and does not guess arbitrary orphans via
    // argv/cwd — it only cleans daemon-owned state files that
    // `remove_instance_delivery_state` is authoritative for.
    let (residual_cleaned, residual_failed) =
        shutdown_cleanup::sweep_residual_transports(home, &instance_names);
    let transports_cleaned = transports_cleaned + residual_cleaned;
    let transports_failed = transports_failed + residual_failed;
    tracing::info!(
        transports_cleaned,
        transports_failed,
        event = "shutdown_transport_cleanup_complete",
        "managed transport teardown complete"
    );

    let uptime_secs = started_at.elapsed().as_secs();
    let metrics = ShutdownMetrics {
        reason,
        agents_total,
        agents_killed_after_grace,
        uptime_secs,
        transports_cleaned,
        transports_failed,
    };
    tracing::info!(
        reason = metrics.reason.as_str(),
        agents_total = metrics.agents_total,
        agents_killed_after_grace = metrics.agents_killed_after_grace,
        transports_cleaned = metrics.transports_cleaned,
        transports_failed = metrics.transports_failed,
        uptime_secs = metrics.uptime_secs,
        shutdown_elapsed_ms = shutdown_started.elapsed().as_millis() as u64,
        event = "shutdown_complete",
        "daemon shutdown sequence complete"
    );
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
    // #2991: accumulate the advisory events and flush the run in one
    // append/sync cycle. The inbox enqueue below stays per-item, so
    // notification order and count are unchanged — only the audit write moves
    // to the end of the loop.
    let mut ask_events = Vec::with_capacity(asks.len());
    for a in &asks {
        ask_events.push(crate::event_log::event(
            "dispatch_stuck_ask",
            &a.to,
            format!(
                "no report_result after {}min, querying assignee",
                crate::dispatch_tracking::DISPATCH_ASK_MINUTES
            ),
        ));
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
    crate::event_log::log_many(home, &ask_events);
    // 24h orphan sweep
    let orphan_events: Vec<_> = crate::dispatch_tracking::sweep_orphans(home)
        .into_iter()
        .map(|orphan| {
            let tid = orphan.task_id.as_deref().unwrap_or("unknown");
            crate::event_log::event(
                "dispatch_orphaned",
                &orphan.to,
                format!("task_id={tid} dispatched_at={}", orphan.delegated_at),
            )
        })
        .collect();
    crate::event_log::log_many(home, &orphan_events);
    // #3536: the sweeps above are both blind to a task whose dispatch a report
    // already settled — the entry is gone, and `sweep_overdue_claimed` only
    // covers tasks with a `due_at`. Remind BOTH parties who can close the loop,
    // because the two exits belong to different roles.
    notify_unsettled_after_report(home);
    // M3: 30-day TTL cleanup for terminal dispatch entries
    crate::dispatch_tracking::gc_old_entries(home);
}

/// #3536: tell the assignee and the orchestrator that a reported task is still
/// sitting open, and name both exits. The task is NEVER auto-closed here — the
/// #3526 completion guard owns settlement and its three-state semantics are
/// deliberate; this only ends the silence.
fn notify_unsettled_after_report(home: &Path) {
    for task in crate::tasks::sweep_unsettled_after_report(home) {
        let assignee = task.assignee.as_deref().unwrap_or("(unassigned)");
        crate::event_log::log(
            home,
            "task_unsettled_after_report",
            assignee,
            &format!(
                "task_id={} status={} reported {}min ago, still unsettled",
                task.task_id, task.status, task.age_minutes
            ),
        );
        // One text for both recipients: each needs to know the OTHER exit exists,
        // otherwise both wait for the other to act — which is how the 40-hour
        // stall in #3536 happened.
        let text = format!(
            "task not settled: a report for task_id={} landed {}min ago but the task is still `{}` on the board. \
             Close the loop — assignee: send the report again with `terminal: true` (correlation_id={}); \
             orchestrator: `task action=done id={}`. \
             Still working? Ignore this; the next report resets the timer.",
            task.task_id, task.age_minutes, task.status, task.task_id, task.task_id
        );
        // `update`, not `query`: the message says "ignore this if you are still
        // working", so it must not read as a question owed an answer.
        let mut targets = Vec::new();
        if let Some(a) = task.assignee.as_deref() {
            targets.push(a.to_string());
        }
        if !targets.iter().any(|t| t == &task.created_by) {
            targets.push(task.created_by.clone());
        }
        for target in targets {
            persist_or_log!(
                crate::inbox::enqueue_with_idle_hint(
                    home,
                    &target,
                    crate::inbox::InboxMessage::new_system("system:task", "update", text.clone()),
                ),
                "task_unsettled_after_report",
                target
            );
        }
    }
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
    let (name, command, args, env, working_dir, submit_key, backend) = def;
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

    // #46776 fail-closed (root finding 5): whole-registry uniqueness admission at
    // BOOT/spawn. A pre-existing duplicate fleet.yaml (legacy / hand-edited — the
    // create path now prevents new ones) must not boot two instances into one
    // workspace. If an EARLIER-sorting instance shares this instance's canonical
    // workspace identity, THAT instance is the single deterministic owner that
    // boots; skip this one with a loud audit rather than resurrect a split-brain.
    let boot_wd = working_dir
        .clone()
        .unwrap_or_else(|| crate::paths::workspace_dir(home).join(name));
    if let Some(owner) = crate::fleet::duplicate_identity_owner_before(home, name, &boot_wd) {
        tracing::error!(
            agent = %name, %owner,
            "skipping boot spawn — duplicate workspace identity; earlier instance owns it (fail-closed)"
        );
        return Ok(());
    }

    configs.lock().insert(
        name.clone(),
        AgentConfig {
            name: name.clone(),
            backend: backend.clone(),
            backend_command: command.clone(),
            args: args.clone(),
            env: env.clone(),
            working_dir: working_dir.clone(),
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
        let backend_skill = backend
            .clone()
            .or_else(|| crate::backend::Backend::from_command(command))
            .and_then(|b| b.skill_dir_name());
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
            backend: backend.as_ref(),
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
