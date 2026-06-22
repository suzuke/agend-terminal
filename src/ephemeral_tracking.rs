//! #1967 Phase-1: ephemeral worker tracking + lifecycle.
//!
//! Short-lived, cross-backend "ephemeral" workers live OUTSIDE the managed-agent
//! bookkeeping — they are NOT inserted into the agent registry, fleet.yaml,
//! binding, or the worktree pool. They are tracked only in a small JSON sidecar
//! (`$AGEND_HOME/ephemeral_workers.json`, mirroring [`crate::dispatch_tracking`])
//! plus an in-memory [`crate::agent::EphemeralPtyHandle`] map for zombie reaping.
//!
//! PR3a (#1967): [`spawn_and_track`] launches a REAL backend HEADLESSLY via a PTY
//! (Route B) — [`crate::agent::spawn_ephemeral_worker`], which reuses the
//! managed-agent spawn path's security (`build_command`) WITHOUT a visible pane
//! and WITHOUT entering the roster. Admission is BEFORE the spawn (reserve → spawn
//! → finalize). Scope = SAFE spawn + reap only; no prompt injection / turn
//! detection / capture yet (PR3b). This replaces PR2's std::process
//! `StdioTransport` path. See `docs/design/1967-ephemeral-phase1.md`.
//!
//! ## Cost guards (lead vet condition — day-1)
//! - **Hard max-live concurrency cap** ([`max_live_workers`]): admission is an
//!   atomic check-and-add under the store flock ([`try_reserve_slot`], BEFORE the
//!   spawn), so the cap can never be exceeded even under concurrent spawns and an
//!   over-cap spawn creates no process.
//! - **Max-wall-TTL** ([`DEFAULT_WALL_TTL_SECS`], clamped to [`MAX_WALL_TTL_SECS`]):
//!   the reap sweep terminates any worker whose wall clock exceeds its `ttl_secs`,
//!   so a wedged/forgotten worker cannot run (and bill) unbounded.

use crate::store::SchemaVersioned;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, OnceLock};

/// Cost-guard default: hard ceiling on simultaneously-live ephemeral workers when
/// `AGEND_EPHEMERAL_MAX_LIVE` is unset/invalid. Spawn admission rejects once the
/// live count would exceed [`max_live_workers`]. Conservative day-1 value against
/// the "workers burning money overnight" fear — a deterministic backstop in place
/// BEFORE real spawn (PR3a) and token metering (PR6). PR3a lowered the default to
/// 3 (a real backend per worker is far heavier than PR2's stub process).
pub const DEFAULT_MAX_LIVE_WORKERS: usize = 3;

/// The resolved hard max-live cap: `AGEND_EPHEMERAL_MAX_LIVE` (a positive integer)
/// if set/valid, else [`DEFAULT_MAX_LIVE_WORKERS`]. Read once and cached — the
/// cap is a process-lifetime constant, so an operator changing it requires a
/// daemon restart (matching every other env-tunable). A zero/negative/garbage
/// value falls back to the default (the cap can never be disabled to 0).
pub fn max_live_workers() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("AGEND_EPHEMERAL_MAX_LIVE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_LIVE_WORKERS)
    })
}

/// Cost-guard: default max wall-clock TTL for a worker (seconds). 30 min —
/// generous for a bounded sub-task, tight enough to cap runaway spend.
pub const DEFAULT_WALL_TTL_SECS: u64 = 30 * 60;

/// Cost-guard: absolute ceiling a per-spawn `ttl_secs` is clamped to, so a caller
/// cannot disable the wall-TTL guard by requesting an enormous value.
pub const MAX_WALL_TTL_SECS: u64 = 2 * 60 * 60;

/// Cost-guard / liveness: a worker stuck in the `"reserving"` state (admission
/// slot taken but `finalize` never ran — daemon crashed between reserve and
/// finalize) longer than this is reaped as stale. Conservative: WELL above the
/// worst-case spawn+finalize latency (milliseconds) so the reap never races an
/// in-flight finalize.
const RESERVE_STALE_SECS: i64 = 60;

/// Worker status: admission slot taken, real process not yet spawned/finalized.
const STATUS_RESERVING: &str = "reserving";
/// Worker status: real process spawned + finalized (live).
const STATUS_RUNNING: &str = "running";
/// Worker status: terminal (workflow signalled done) — reaped on the next sweep.
const STATUS_DONE: &str = "done";

/// One tracked ephemeral worker. Persisted as a JSON row; the live
/// [`crate::agent::EphemeralPtyHandle`] (for zombie reaping + PR3b inject) is held
/// separately in [`LIVE_CHILDREN`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EphemeralWorker {
    pub worker_id: String,
    pub workflow_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub backend: String,
    pub pid: u32,
    /// Start-time identity token ([`crate::process::process_start_token`]) — guards
    /// against terminating a RECYCLED pid on reap. `None` if unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_start_token: Option<u64>,
    pub spawned_at: String, // RFC3339 UTC
    pub ttl_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    pub phase: String,  // "reserving" | "spawned" | "running" | "done"
    pub status: String, // "reserving" | "running" | "done"
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct EphemeralStore {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    workers: Vec<EphemeralWorker>,
}

impl crate::store::SchemaVersioned for EphemeralStore {
    const CURRENT: u32 = 1;
    fn version_mut(&mut self) -> &mut u32 {
        &mut self.schema_version
    }
}

/// In-memory map of `worker_id → EphemeralPtyHandle` for workers spawned THIS
/// daemon life, so reap can `wait()` the terminated child (never leak a zombie)
/// and PR3b can reach the retained PTY writer + core. Empty after a restart
/// (handles don't survive) — restart orphans are reaped by [`reap_on_boot`] via
/// pid + start-token.
static LIVE_CHILDREN: LazyLock<Mutex<HashMap<String, crate::agent::EphemeralPtyHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Process-local monotonic counter for unique worker ids within a daemon life.
static WORKER_SEQ: AtomicU64 = AtomicU64::new(0);

fn store_path(home: &Path) -> PathBuf {
    crate::store::store_path(home, "ephemeral_workers.json")
}

fn load(home: &Path) -> EphemeralStore {
    crate::store::load_versioned(&store_path(home), EphemeralStore::CURRENT)
}

/// Clamp a requested TTL to `[1, MAX_WALL_TTL_SECS]`; `None`/`0` → default.
pub fn resolve_ttl(requested: Option<u64>) -> u64 {
    match requested {
        None | Some(0) => DEFAULT_WALL_TTL_SECS,
        Some(n) => n.min(MAX_WALL_TTL_SECS),
    }
}

/// List tracked workers, optionally filtered to one workflow.
pub fn list(home: &Path, workflow_id: Option<&str>) -> Vec<EphemeralWorker> {
    let mut ws = load(home).workers;
    if let Some(wf) = workflow_id {
        ws.retain(|w| w.workflow_id == wf);
    }
    ws
}

/// Parameters for a spawn (the MCP `ephemeral spawn` handler fills this).
#[derive(Debug, Clone, Default)]
pub struct SpawnSpec {
    pub workflow_id: String,
    pub parent: Option<String>,
    pub backend: String,
    pub ttl_secs: Option<u64>,
    pub token_budget: Option<u64>,
}

/// Why a spawn failed.
#[derive(Debug)]
pub enum SpawnError {
    /// The hard max-live concurrency cap would be exceeded.
    CapExceeded { live: usize, cap: usize },
    /// The headless process failed to launch (or finalize).
    Spawn(std::io::Error),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::CapExceeded { live, cap } => write!(
                f,
                "ephemeral worker cap reached ({live}/{cap} live) — reap or wait before spawning more"
            ),
            SpawnError::Spawn(e) => write!(f, "failed to spawn ephemeral worker: {e}"),
        }
    }
}

/// Atomic admission RESERVE (r4 PR1 note: admission happens BEFORE spawning the
/// real process). In ONE locked read-modify-write: if the live count is below
/// [`max_live_workers`], insert `reserving` (a [`STATUS_RESERVING`] row, pid=0)
/// and return `Ok`; else `Err(live)` WITHOUT inserting — so an over-cap spawn
/// rejects before any OS process exists. Single RMW under the store flock →
/// concurrent reserves can never push the count past the cap (no TOCTOU).
fn try_reserve_slot(home: &Path, reserving: EphemeralWorker) -> Result<(), usize> {
    let mut rejected_at = None;
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |s: &mut EphemeralStore| {
            if s.workers.len() >= max_live_workers() {
                rejected_at = Some(s.workers.len());
                return Ok(());
            }
            s.workers.push(reserving.clone());
            Ok(())
        }),
        "ephemeral_reserve"
    );
    match rejected_at {
        Some(live) => Err(live),
        None => Ok(()),
    }
}

/// Finalize a reserved slot once the real process is spawned: stamp the real pid
/// and start-token, then flip `STATUS_RESERVING` → `STATUS_RUNNING` (the wall-TTL
/// clock restarts at the real spawn time). Returns the finalized worker, or `Err`
/// if the reserving row is gone (e.g. concurrently reaped as stale — caller then
/// kills the just-spawned orphan).
fn finalize_spawn(
    home: &Path,
    worker_id: &str,
    pid: u32,
    token: Option<u64>,
) -> Result<EphemeralWorker, ()> {
    let mut finalized = None;
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |s: &mut EphemeralStore| {
            if let Some(w) = s.workers.iter_mut().find(|w| w.worker_id == worker_id) {
                w.pid = pid;
                w.process_start_token = token;
                w.status = STATUS_RUNNING.to_string();
                w.phase = "spawned".to_string();
                w.spawned_at = chrono::Utc::now().to_rfc3339();
                finalized = Some(w.clone());
            }
            Ok(())
        }),
        "ephemeral_finalize"
    );
    finalized.ok_or(())
}

/// Remove a reserved slot (spawn or finalize failed) so a failed admission frees
/// the cap and leaves no orphan row.
fn release_reservation(home: &Path, worker_id: &str) {
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |s: &mut EphemeralStore| {
            s.workers.retain(|w| w.worker_id != worker_id);
            Ok(())
        }),
        "ephemeral_release_reservation"
    );
}

/// Spawn a real backend HEADLESSLY (Route B — a PTY child with no pane / no
/// roster, via [`crate::agent::spawn_ephemeral_worker`]) under the hard max-live
/// cap. Admission is BEFORE the spawn (r4 PR1 note): **reserve → spawn →
/// finalize**. An over-cap spawn rejects WITHOUT creating any process; a
/// spawn/finalize failure releases the reservation (no leaked slot, no leaked
/// process). The worker inherits the managed-agent spawn security identically
/// (`spawn_ephemeral_worker` calls the SAME `build_command`). Returns the live
/// [`EphemeralWorker`].
pub fn spawn_and_track(home: &Path, spec: SpawnSpec) -> Result<EphemeralWorker, SpawnError> {
    let ttl_secs = resolve_ttl(spec.ttl_secs);
    let seq = WORKER_SEQ.fetch_add(1, Ordering::Relaxed);
    let worker_id = format!("eph-{}-{}", std::process::id(), seq);

    // 1) ADMISSION BEFORE SPAWN — reserve the cap slot atomically. Over cap →
    //    reject here, before any OS process exists (r4 PR1 NON-BLOCKING note).
    let reserving = EphemeralWorker {
        worker_id: worker_id.clone(),
        workflow_id: spec.workflow_id.clone(),
        parent: spec.parent.clone(),
        backend: spec.backend.clone(),
        pid: 0,
        process_start_token: None,
        spawned_at: chrono::Utc::now().to_rfc3339(),
        ttl_secs,
        token_budget: spec.token_budget,
        phase: STATUS_RESERVING.to_string(),
        status: STATUS_RESERVING.to_string(),
    };
    if let Err(live) = try_reserve_slot(home, reserving) {
        return Err(SpawnError::CapExceeded {
            live,
            cap: max_live_workers(),
        });
    }

    // 2) SPAWN the real backend headlessly — only now that a slot is reserved.
    //    Per-worker cwd under $AGEND_HOME/backend-data/ephemeral/<worker_id> so
    //    `build_command`'s two-pass cwd validation resolves inside AGEND_HOME.
    //    create_dir_all err is ignored — `build_command` re-creates + re-validates.
    let cwd = home.join("backend-data").join("ephemeral").join(&worker_id);
    std::fs::create_dir_all(&cwd).ok();
    let config = crate::agent::SpawnConfig {
        name: &worker_id,
        backend_command: &spec.backend,
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh, // ephemeral workers are always fresh
        cols: 200,
        rows: 50,
        env: None,
        working_dir: Some(&cwd),
        submit_key: "\r",
        home: Some(home),
        crash_tx: None, // no crash-respawn for ephemeral
        shutdown: None,
    };
    let handle = match crate::agent::spawn_ephemeral_worker(&config) {
        Ok(h) => h,
        Err(e) => {
            release_reservation(home, &worker_id);
            return Err(SpawnError::Spawn(std::io::Error::other(e.to_string())));
        }
    };
    let pid = handle.pid();
    let token = crate::process::process_start_token(pid);

    // 3) FINALIZE — stamp the real pid/token + flip reserving → running.
    match finalize_spawn(home, &worker_id, pid, token) {
        Ok(worker) => {
            LIVE_CHILDREN.lock().insert(worker_id, handle);
            Ok(worker)
        }
        Err(()) => {
            // Reserving row vanished (concurrently reaped as stale) — kill the
            // orphan tree so no process leaks; the slot is already gone. A PTY
            // child is its own session leader, so kill_process_tree reaps any
            // children it forked too.
            crate::process::kill_process_tree(pid);
            let _ = handle.child.lock().wait();
            Err(SpawnError::Spawn(std::io::Error::other(
                "ephemeral finalize: reserving slot vanished before finalize",
            )))
        }
    }
}

/// Seconds since `w.spawned_at` at `now` (the row's age). A corrupt/unparseable
/// timestamp returns `i64::MAX` so the row is treated as overdue (never kept
/// forever).
fn age_secs(w: &EphemeralWorker, now: chrono::DateTime<chrono::Utc>) -> i64 {
    match chrono::DateTime::parse_from_rfc3339(&w.spawned_at) {
        Ok(t) => (now - t.with_timezone(&chrono::Utc)).num_seconds(),
        Err(_) => i64::MAX,
    }
}

/// Pure reap decision for a RUNNING worker: terminal status, or its max-wall-TTL
/// has elapsed (the cost guard). Reserving rows are NOT judged here (see
/// [`is_stale_reserving`]); pid-liveness is handled separately by the sweep.
fn is_due(w: &EphemeralWorker, now: chrono::DateTime<chrono::Utc>) -> bool {
    w.status == STATUS_DONE || age_secs(w, now) >= w.ttl_secs as i64
}

/// A `STATUS_RESERVING` row is stale iff older than [`RESERVE_STALE_SECS`] — the
/// daemon crashed between reserve and finalize. A FRESH reserving row (spawn/
/// finalize in flight, pid still 0) must NOT be reaped, so the sweep gates
/// reserving rows on this (NOT on pid-liveness, which would see pid=0 as dead).
fn is_stale_reserving(w: &EphemeralWorker, now: chrono::DateTime<chrono::Utc>) -> bool {
    w.status == STATUS_RESERVING && age_secs(w, now) >= RESERVE_STALE_SECS
}

/// Periodic reap sweep (run from the per-tick handler). Removes every worker that
/// is terminal, past its max-wall-TTL, or whose process is already gone, and
/// terminates any that are still alive. Returns the reaped workers.
pub fn reap_sweep(home: &Path) -> Vec<EphemeralWorker> {
    reap_sweep_at(home, chrono::Utc::now())
}

fn reap_sweep_at(home: &Path, now: chrono::DateTime<chrono::Utc>) -> Vec<EphemeralWorker> {
    let mut reaped = Vec::new();
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |s: &mut EphemeralStore| {
            let mut keep = Vec::with_capacity(s.workers.len());
            for w in std::mem::take(&mut s.workers) {
                // Reserving rows (pid=0, spawn in flight) are judged ONLY by the
                // stale timeout — NOT by pid-liveness (pid=0 reads as dead and
                // would race an in-flight finalize). Running rows: terminal /
                // TTL-expired / process gone.
                let drop = if w.status == STATUS_RESERVING {
                    is_stale_reserving(&w, now)
                } else {
                    is_due(&w, now) || !crate::process::is_pid_alive(w.pid)
                };
                if drop {
                    reaped.push(w);
                } else {
                    keep.push(w);
                }
            }
            s.workers = keep;
            Ok(())
        }),
        "ephemeral_reap_sweep"
    );
    for w in &reaped {
        terminate_worker(w);
    }
    reaped
}

/// Explicitly reap one worker by id (terminate + remove). Returns it if present.
pub fn reap_one(home: &Path, worker_id: &str) -> Option<EphemeralWorker> {
    let mut taken = None;
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |s: &mut EphemeralStore| {
            if let Some(pos) = s.workers.iter().position(|w| w.worker_id == worker_id) {
                taken = Some(s.workers.remove(pos));
            }
            Ok(())
        }),
        "ephemeral_reap_one"
    );
    if let Some(w) = &taken {
        terminate_worker(w);
    }
    taken
}

/// Reap every worker of a workflow (terminate + remove). Returns the reaped set.
pub fn reap_workflow(home: &Path, workflow_id: &str) -> Vec<EphemeralWorker> {
    let mut reaped = Vec::new();
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |s: &mut EphemeralStore| {
            let mut keep = Vec::with_capacity(s.workers.len());
            for w in std::mem::take(&mut s.workers) {
                if w.workflow_id == workflow_id {
                    reaped.push(w);
                } else {
                    keep.push(w);
                }
            }
            s.workers = keep;
            Ok(())
        }),
        "ephemeral_reap_workflow"
    );
    for w in &reaped {
        terminate_worker(w);
    }
    reaped
}

/// Boot-time sweep: every tracked worker predates THIS daemon boot, so it is an
/// orphan of a previous (possibly crashed) daemon. Terminate any still-alive
/// (start-token-guarded) and clear the store. Returns the count cleared.
pub fn reap_on_boot(home: &Path) -> usize {
    let mut orphans = Vec::new();
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |s: &mut EphemeralStore| {
            orphans = std::mem::take(&mut s.workers);
            Ok(())
        }),
        "ephemeral_reap_on_boot"
    );
    for w in &orphans {
        terminate_worker(w);
    }
    if !orphans.is_empty() {
        tracing::info!(
            target: "ephemeral",
            cleared = orphans.len(),
            "reaped orphaned ephemeral workers on boot"
        );
    }
    orphans.len()
}

/// Terminate a reaped worker's process TREE and reap its zombie.
///
/// If we still hold the live [`crate::agent::EphemeralPtyHandle`] (spawned this
/// daemon life): group-kill the whole PTY-session tree
/// ([`crate::process::kill_process_tree`]) then `wait()` to collect the exit (no
/// zombie). Otherwise (e.g. a restart orphan, no in-memory handle): a
/// start-token-guarded group-kill — a recycled pid is never killed, and init
/// reaps the orphan's zombie.
///
/// PR3a: group-kill is now DONE (the PR2 "group-kill DEFERRED" note is resolved).
/// A PTY child is its own session leader, so `kill_process_tree` reaps the worker
/// AND any children the real backend forked — no explicit `process_group(0)` is
/// needed (the PTY already establishes the session). Route B has NO protocol cancel
/// (confirmed for opencode in the ACP sub-spike), so cancel stays a hard
/// process-tree kill; a PTY-level graceful quit (the backend's `quit_command`)
/// before the kill is an optional later refinement, not a protocol step.
fn terminate_worker(w: &EphemeralWorker) {
    if let Some(handle) = LIVE_CHILDREN.lock().remove(&w.worker_id) {
        // Group-kill the PTY-session tree, then reap the leader's zombie.
        crate::process::kill_process_tree(handle.pid());
        let _ = handle.child.lock().wait();
        return;
    }
    if w.pid == 0 || !crate::process::is_pid_alive(w.pid) {
        return;
    }
    // No handle (restart orphan): only terminate if the start-token still matches,
    // so a recycled pid belonging to an innocent process is never signalled.
    if let Some(tok) = w.process_start_token {
        if crate::process::process_start_token(w.pid) != Some(tok) {
            return;
        }
    }
    crate::process::kill_process_tree(w.pid);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-ephemeral-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A real, CROSS-PLATFORM long-lived backend command (program + args) so the
    /// spawn→reap lifecycle + termination are exercised on EVERY CI platform incl.
    /// windows-latest (Windows posture must-carry: don't let a `cfg(unix)` gate
    /// hide a cross-platform prod gap — xwin compile-green ≠ runtime-pass). Points
    /// `spawn_ephemeral_worker` at a stand-in long-lived binary (sleep/ping), which
    /// resolves to `Backend::from_command` → `None` (no preset args / spawn flags),
    /// so the test needs no installed backend.
    fn long_lived_cmd() -> (String, Vec<String>) {
        #[cfg(unix)]
        {
            ("/bin/sleep".to_string(), vec!["30".to_string()])
        }
        #[cfg(windows)]
        {
            (
                "ping".to_string(),
                vec!["-n".to_string(), "31".to_string(), "127.0.0.1".to_string()],
            )
        }
    }

    fn reserving_row(id: &str, age_secs: i64) -> EphemeralWorker {
        EphemeralWorker {
            worker_id: id.to_string(),
            pid: 0,
            spawned_at: (chrono::Utc::now() - chrono::Duration::seconds(age_secs)).to_rfc3339(),
            ttl_secs: 100,
            status: STATUS_RESERVING.to_string(),
            phase: STATUS_RESERVING.to_string(),
            ..Default::default()
        }
    }

    /// Poll until `pid` is dead or a short deadline. Termination + OS process-
    /// object cleanup can lag briefly (notably on Windows: an exited process stays
    /// queryable until its last handle closes), so a death check must NOT assert
    /// immediately. The caller must have dropped the `Child` handle first.
    fn assert_dead_within(pid: u32, label: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while crate::process::is_pid_alive(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "{label}: pid {pid} still alive after deadline"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn resolve_ttl_defaults_and_clamps() {
        assert_eq!(resolve_ttl(None), DEFAULT_WALL_TTL_SECS);
        assert_eq!(resolve_ttl(Some(0)), DEFAULT_WALL_TTL_SECS);
        assert_eq!(resolve_ttl(Some(120)), 120);
        assert_eq!(resolve_ttl(Some(u64::MAX)), MAX_WALL_TTL_SECS);
    }

    /// is_due fires on terminal status or elapsed TTL — the max-wall-TTL guard.
    #[test]
    fn is_due_on_done_or_expired_ttl() {
        let now = chrono::Utc::now();
        let mut w = EphemeralWorker {
            spawned_at: now.to_rfc3339(),
            ttl_secs: 100,
            status: "running".to_string(),
            ..Default::default()
        };
        assert!(!is_due(&w, now), "fresh running worker is not due");
        // status=done → due regardless of time.
        w.status = "done".to_string();
        assert!(is_due(&w, now));
        // running but TTL elapsed → due (the wall-TTL cost guard).
        w.status = "running".to_string();
        w.spawned_at = (now - chrono::Duration::seconds(101)).to_rfc3339();
        assert!(
            is_due(&w, now),
            "a worker past its max-wall-TTL must be due"
        );
        // unparseable timestamp → due (don't keep a corrupt row forever).
        w.spawned_at = "not-a-timestamp".to_string();
        assert!(is_due(&w, now));
    }

    /// Atomic RMW: many tracks accumulate without loss; list/count reflect them.
    #[test]
    fn track_and_list_atomic_rmw() {
        let home = tmp_home("rmw");
        for i in 0..5 {
            let w = EphemeralWorker {
                worker_id: format!("w{i}"),
                workflow_id: if i < 3 { "wfA" } else { "wfB" }.to_string(),
                pid: 0,
                spawned_at: chrono::Utc::now().to_rfc3339(),
                ttl_secs: 100,
                phase: "spawned".to_string(),
                status: "running".to_string(),
                ..Default::default()
            };
            // bypass the cap gate for a pure store test
            persist_or_log!(
                crate::store::mutate_versioned(&store_path(&home), |s: &mut EphemeralStore| {
                    s.workers.push(w);
                    Ok(())
                }),
                "test_push"
            );
        }
        assert_eq!(list(&home, None).len(), 5);
        assert_eq!(list(&home, Some("wfA")).len(), 3);
        assert_eq!(list(&home, Some("wfB")).len(), 2);
        assert_eq!(list(&home, None).len(), 5);
        std::fs::remove_dir_all(&home).ok();
    }

    /// Hard max-live cap: admission rejects (and does NOT add) once full.
    #[test]
    fn max_live_cap_rejects_over_limit() {
        let home = tmp_home("cap");
        let cap = max_live_workers();
        for i in 0..cap {
            let w = EphemeralWorker {
                worker_id: format!("w{i}"),
                pid: 0,
                spawned_at: chrono::Utc::now().to_rfc3339(),
                ttl_secs: 100,
                status: "running".to_string(),
                ..Default::default()
            };
            assert!(try_reserve_slot(&home, w).is_ok(), "under cap must accept");
        }
        assert_eq!(list(&home, None).len(), cap);
        let over = EphemeralWorker {
            worker_id: "over".to_string(),
            pid: 0,
            spawned_at: chrono::Utc::now().to_rfc3339(),
            ttl_secs: 100,
            status: "running".to_string(),
            ..Default::default()
        };
        let res = try_reserve_slot(&home, over);
        assert_eq!(res, Err(cap), "at cap must reject");
        assert_eq!(
            list(&home, None).len(),
            cap,
            "rejected worker must NOT be added"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// `max_live_workers` defaults to [`DEFAULT_MAX_LIVE_WORKERS`] absent the env
    /// override. (The override is read-once/cached so a per-test env mutation can't
    /// be asserted here without process-isolation — the default path is the one
    /// that ships and is asserted.)
    #[test]
    fn max_live_workers_defaults() {
        if std::env::var("AGEND_EPHEMERAL_MAX_LIVE").is_err() {
            assert_eq!(max_live_workers(), DEFAULT_MAX_LIVE_WORKERS);
        }
        assert!(max_live_workers() > 0, "the cap can never be 0");
    }

    /// Reap-on-boot clears every tracked worker (crash-leaked orphans). Uses
    /// pid=0 rows (no real process) so no signalling happens.
    #[test]
    fn reap_on_boot_clears_store() {
        let home = tmp_home("boot");
        for i in 0..3 {
            persist_or_log!(
                crate::store::mutate_versioned(&store_path(&home), |s: &mut EphemeralStore| {
                    s.workers.push(EphemeralWorker {
                        worker_id: format!("w{i}"),
                        pid: 0,
                        spawned_at: chrono::Utc::now().to_rfc3339(),
                        ttl_secs: 100,
                        status: "running".to_string(),
                        ..Default::default()
                    });
                    Ok(())
                }),
                "test_push"
            );
        }
        assert_eq!(list(&home, None).len(), 3);
        let cleared = reap_on_boot(&home);
        assert_eq!(cleared, 3);
        assert_eq!(list(&home, None).len(), 0, "boot reap must clear the store");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Track a REAL (cross-platform) ephemeral PTY worker through the SAME
    /// reserve→spawn→finalize→insert pipeline `spawn_and_track` runs, but with a
    /// long-lived stand-in command (sleep/ping) so the test needs no installed
    /// backend. `spawn_and_track` hardcodes `args:&[]` (a real backend needs none),
    /// so the test drives `spawn_ephemeral_worker` directly (where it controls
    /// args) and then finalizes + inserts into `LIVE_CHILDREN` identically — so the
    /// reap path under test goes through the real `terminate_worker`.
    fn track_real_worker(home: &Path, workflow_id: &str, ttl_secs: u64) -> EphemeralWorker {
        let seq = WORKER_SEQ.fetch_add(1, Ordering::Relaxed);
        let worker_id = format!("eph-test-{}-{}", std::process::id(), seq);
        let mut reserving = reserving_row(&worker_id, 0);
        reserving.workflow_id = workflow_id.to_string();
        reserving.backend = "sleep".to_string();
        reserving.ttl_secs = ttl_secs;
        try_reserve_slot(home, reserving).expect("reserve");

        let (program, args) = long_lived_cmd();
        let config = crate::agent::SpawnConfig {
            name: &worker_id,
            backend_command: &program,
            args: &args,
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 200,
            rows: 50,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None, // ad-hoc test spawn — skip the AGEND_HOME cwd-validation seam
            crash_tx: None,
            shutdown: None,
        };
        let handle = crate::agent::spawn_ephemeral_worker(&config).expect("spawn ephemeral worker");
        let pid = handle.pid();
        let token = crate::process::process_start_token(pid);
        let worker = finalize_spawn(home, &worker_id, pid, token).expect("finalize");
        LIVE_CHILDREN.lock().insert(worker_id, handle);
        worker
    }

    /// Full lifecycle against a REAL ephemeral PTY worker (cross-platform):
    /// reserve→spawn→finalize → list → reap, and the reap genuinely terminates the
    /// process tree (is_pid_alive → false). Runs on all CI platforms.
    #[test]
    fn ephemeral_lifecycle_spawn_list_reap() {
        let home = tmp_home("lifecycle");
        let w = track_real_worker(&home, "wf1", 3600);
        assert_eq!(w.status, STATUS_RUNNING, "finalized to running");
        assert_ne!(w.pid, 0, "real pid stamped at finalize");
        assert_eq!(
            list(&home, Some("wf1")).len(),
            1,
            "spawned worker is listed"
        );
        assert!(
            crate::process::is_pid_alive(w.pid),
            "real ephemeral PTY worker is alive after spawn"
        );

        let reaped = reap_one(&home, &w.worker_id).expect("reap_one returns the worker");
        assert_eq!(reaped.worker_id, w.worker_id);
        assert_eq!(
            list(&home, None).len(),
            0,
            "reaped worker removed from store"
        );
        assert_dead_within(w.pid, "reap must terminate the real ephemeral process");
        std::fs::remove_dir_all(&home).ok();
    }

    /// The reap SWEEP enforces max-wall-TTL: a real process past its TTL is
    /// terminated + removed (drive `now` past the TTL — deterministic, no waiting).
    #[test]
    fn reap_sweep_enforces_wall_ttl() {
        let home = tmp_home("ttl-sweep");
        let w = track_real_worker(&home, "wf1", 60);
        assert!(crate::process::is_pid_alive(w.pid));
        let future = chrono::Utc::now() + chrono::Duration::seconds(61);
        let reaped = reap_sweep_at(&home, future);
        assert_eq!(reaped.len(), 1, "the TTL-expired worker must be reaped");
        assert_eq!(list(&home, None).len(), 0);
        assert_dead_within(w.pid, "TTL reap must terminate the real process");
        std::fs::remove_dir_all(&home).ok();
    }

    /// r4 PR1 NON-BLOCKING note: admission is BEFORE spawn. When the cap is full,
    /// `spawn_and_track` rejects WITHOUT creating ANY process (it returns before the
    /// spawn branch) — zero orphan, and the store is left untouched.
    #[test]
    fn admission_before_spawn_rejects_over_cap_zero_orphan() {
        let home = tmp_home("cap-noorphan");
        let cap = max_live_workers();
        // Fill every slot with reservations (no processes).
        for i in 0..cap {
            try_reserve_slot(&home, reserving_row(&format!("r{i}"), 0)).unwrap();
        }
        let spec = SpawnSpec {
            workflow_id: "wf".to_string(),
            parent: None,
            backend: "claude".to_string(),
            ttl_secs: Some(60),
            token_budget: None,
        };
        let res = spawn_and_track(&home, spec);
        assert!(
            matches!(res, Err(SpawnError::CapExceeded { .. })),
            "over-cap spawn must be rejected"
        );
        // admission BEFORE spawn: the store still holds exactly the `cap`
        // reservations — the rejected spawn added (and spawned) nothing.
        assert_eq!(
            list(&home, None).len(),
            cap,
            "rejected spawn adds nothing (zero orphan process, zero orphan row)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// reserve↔finalize crash: a STALE reserving row (daemon died before finalize)
    /// is reaped by the sweep, while a FRESH reserving row (spawn in flight, pid=0)
    /// is KEPT — the sweep must gate reserving rows on the stale timeout, NOT on
    /// pid-liveness (pid=0 reads as dead and would race an in-flight finalize).
    #[test]
    fn stale_reserving_reaped_fresh_kept() {
        let home = tmp_home("stale-reserve");
        try_reserve_slot(&home, reserving_row("fresh", 0)).unwrap();
        try_reserve_slot(&home, reserving_row("stale", RESERVE_STALE_SECS + 5)).unwrap();
        let reaped = reap_sweep_at(&home, chrono::Utc::now());
        assert_eq!(reaped.len(), 1, "only the stale reserving row is reaped");
        assert_eq!(reaped[0].worker_id, "stale");
        let kept = list(&home, None);
        assert_eq!(
            kept.len(),
            1,
            "the fresh reserving row is kept (no pid=0 race)"
        );
        assert_eq!(kept[0].worker_id, "fresh");
        std::fs::remove_dir_all(&home).ok();
    }

    /// PR3a Route-B core: `spawn_ephemeral_worker` spawns a REAL (cross-platform)
    /// long-lived process via a PTY, reports its live pid, and is NOT in any agent
    /// registry — the fn takes NO `AgentRegistry` argument, so by construction
    /// there is zero roster involvement (no pane, no router subscriber). A
    /// `kill_process_tree(pid)` then reaps the whole PTY-session tree.
    #[test]
    fn spawn_ephemeral_worker_no_registry_then_kill_tree_reaps() {
        let (program, args) = long_lived_cmd();
        let worker_id = format!("eph-direct-{}", std::process::id());
        let config = crate::agent::SpawnConfig {
            name: &worker_id,
            backend_command: &program,
            args: &args,
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 200,
            rows: 50,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        let handle = crate::agent::spawn_ephemeral_worker(&config).expect("spawn ephemeral worker");
        let pid = handle.pid();
        assert_ne!(pid, 0, "a live pid was assigned");
        assert_eq!(handle.pid(), pid, "pid() accessor agrees");
        assert!(
            crate::process::is_pid_alive(pid),
            "the real PTY worker is alive after spawn"
        );

        // Reap the whole PTY-session tree, then drop the handle so the OS frees the
        // process object (Windows keeps an exited process queryable while any
        // handle is open — production's terminate_worker drops it the same way).
        crate::process::kill_process_tree(pid);
        let _ = handle.child.lock().wait();
        drop(handle);
        assert_dead_within(pid, "kill_process_tree must reap the ephemeral PTY worker");
    }
}
