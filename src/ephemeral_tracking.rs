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
    /// The requested `backend` is not on the allowlist (a path, or a basename
    /// that doesn't map to a known backend) — an arbitrary-exec guard. Carries the
    /// rejected input for the operator-facing error.
    UnsupportedBackend(String),
    /// The hard max-live concurrency cap would be exceeded.
    CapExceeded { live: usize, cap: usize },
    /// The durable cap reservation could not be persisted (store write failed) —
    /// admission fails CLOSED rather than spawning with no durable reservation.
    Reserve(std::io::Error),
    /// The headless process failed to launch (or finalize).
    Spawn(std::io::Error),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::UnsupportedBackend(b) => write!(
                f,
                "ephemeral spawn: unsupported backend '{b}' — must be a bare backend command \
                 (no path separators), one of: claude, codex, opencode, kiro-cli, agy"
            ),
            SpawnError::CapExceeded { live, cap } => write!(
                f,
                "ephemeral worker cap reached ({live}/{cap} live) — reap or wait before spawning more"
            ),
            SpawnError::Reserve(e) => write!(
                f,
                "ephemeral spawn: failed to persist the cap reservation (admission fails closed, no process spawned): {e}"
            ),
            SpawnError::Spawn(e) => write!(f, "failed to spawn ephemeral worker: {e}"),
        }
    }
}

/// Validate a requested `backend` string against the allowlist and resolve it to
/// the CANONICAL backend command (security keystone — closes the arbitrary-exec
/// vector that the raw `backend` → `build_command` → `which::which` path opened).
///
/// Rejects (returns `Err`):
/// - any string containing a path separator (`/` or `\`) — a path is an
///   arbitrary-exec vector (`/tmp/evil.sh`), never a backend command;
/// - any basename that [`crate::backend::Backend::from_command`] does not map to a
///   known backend (`from_command` returns `None`; it never returns Shell/Raw).
///
/// On accept, returns the backend's CANONICAL command
/// ([`crate::backend::BackendPreset::command`] — `claude`/`codex`/`opencode`/
/// `kiro-cli`/`agy`), NEVER the caller's raw input — so a `claude-evil` basename
/// (which `from_command` maps to ClaudeCode by prefix) is launched as the real
/// `claude` binary with its canonical preset, not the attacker's name.
pub fn resolve_supported_backend(backend: &str) -> Result<&'static str, String> {
    if backend.contains('/') || backend.contains('\\') {
        return Err(format!(
            "backend '{backend}' looks like a path — only a bare backend command is allowed"
        ));
    }
    match crate::backend::Backend::from_command(backend) {
        Some(b) => Ok(b.preset().command),
        None => Err(format!("backend '{backend}' is not a supported backend")),
    }
}

/// Why a slot reservation failed.
#[derive(Debug)]
enum ReserveError {
    /// The hard max-live concurrency cap is full (carries the live count).
    CapExceeded(usize),
    /// The store write failed — admission must fail CLOSED (no spawn).
    Persist(std::io::Error),
}

/// Atomic admission RESERVE (r4 PR1 note: admission happens BEFORE spawning the
/// real process). In ONE locked read-modify-write: if the live count is below
/// [`max_live_workers`], insert `reserving` (a [`STATUS_RESERVING`] row, pid=0)
/// and return `Ok`; else `Err(ReserveError::CapExceeded(live))` WITHOUT inserting —
/// so an over-cap spawn rejects before any OS process exists. Single RMW under the
/// store flock → concurrent reserves can never push the count past the cap (no
/// TOCTOU).
///
/// A STORE WRITE FAILURE is NOT swallowed — it surfaces as
/// `Err(ReserveError::Persist(_))` so admission can fail CLOSED. Swallowing it
/// (the old `persist_or_log!` path) would return `Ok` with NO durable reservation
/// on disk, then OS-spawn anyway — a worker that the cap can no longer see (it
/// would re-admit over the cap, and a crash before finalize leaves no row to reap).
fn try_reserve_slot(home: &Path, reserving: EphemeralWorker) -> Result<(), ReserveError> {
    let mut rejected_at = None;
    crate::store::mutate_versioned(&store_path(home), |s: &mut EphemeralStore| {
        if s.workers.len() >= max_live_workers() {
            rejected_at = Some(s.workers.len());
            return Ok(());
        }
        s.workers.push(reserving.clone());
        Ok(())
    })
    .map_err(|e| ReserveError::Persist(std::io::Error::other(e.to_string())))?;
    match rejected_at {
        Some(live) => Err(ReserveError::CapExceeded(live)),
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
    // 0) BACKEND ALLOWLIST (security keystone) — validate BEFORE reserving any slot
    //    or spawning any process. `spawn_and_track` is the SHARED SINK every spawn
    //    funnels through (the MCP handler AND any direct caller), so gating here —
    //    not in the handler — protects every entry. A path or an unknown basename
    //    is rejected; the canonical command is used for the spawn + persisted row
    //    (NEVER the raw input — `claude-evil` → real `claude`).
    let backend_command = match resolve_supported_backend(&spec.backend) {
        Ok(cmd) => cmd,
        Err(_) => return Err(SpawnError::UnsupportedBackend(spec.backend.clone())),
    };

    let ttl_secs = resolve_ttl(spec.ttl_secs);
    let seq = WORKER_SEQ.fetch_add(1, Ordering::Relaxed);
    let worker_id = format!("eph-{}-{}", std::process::id(), seq);

    // 1) ADMISSION BEFORE SPAWN — reserve the cap slot atomically. Over cap →
    //    reject here, before any OS process exists (r4 PR1 NON-BLOCKING note). A
    //    persist failure → fail CLOSED (return, no spawn). Persist the CANONICAL
    //    backend command so list/reap show the resolved binary, not the raw input.
    let reserving = EphemeralWorker {
        worker_id: worker_id.clone(),
        workflow_id: spec.workflow_id.clone(),
        parent: spec.parent.clone(),
        backend: backend_command.to_string(),
        pid: 0,
        process_start_token: None,
        spawned_at: chrono::Utc::now().to_rfc3339(),
        ttl_secs,
        token_budget: spec.token_budget,
        phase: STATUS_RESERVING.to_string(),
        status: STATUS_RESERVING.to_string(),
    };
    match try_reserve_slot(home, reserving) {
        Ok(()) => {}
        Err(ReserveError::CapExceeded(live)) => {
            return Err(SpawnError::CapExceeded {
                live,
                cap: max_live_workers(),
            });
        }
        Err(ReserveError::Persist(e)) => return Err(SpawnError::Reserve(e)),
    }

    // 2) SPAWN the real backend headlessly — only now that a slot is reserved.
    //    Per-worker cwd under $AGEND_HOME/backend-data/ephemeral/<worker_id> so
    //    `build_command`'s two-pass cwd validation resolves inside AGEND_HOME.
    //    create_dir_all err is ignored — `build_command` re-creates + re-validates.
    let cwd = home.join("backend-data").join("ephemeral").join(&worker_id);
    std::fs::create_dir_all(&cwd).ok();
    let config = crate::agent::SpawnConfig {
        name: &worker_id,
        backend_command, // canonical (allowlist-resolved) — never the raw input
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
/// daemon life), the PRIMARY live path: the whole worker tree is killed and the
/// leader's zombie reaped — **unix** = pgid group-kill via
/// [`crate::process::kill_process_tree`] (a PTY child is its own session leader, so
/// the PGID covers every grandchild the backend forked); **windows** = the
/// EphemeralPtyHandle owns a Job Object created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
/// and the worker + all descendants are assigned to it at spawn, so DROPPING the
/// handle closes the job handle and the OS kills the entire tree (a real tree-kill,
/// unlike bare `TerminateProcess` which only kills the leader). On windows we also
/// call `kill_process_tree(pid)` first as a belt-and-suspenders leader kill, but the
/// Job-Object close is the tree mechanism. Then `wait()` collects the leader's exit
/// (no zombie) and the handle drops.
///
/// Otherwise (a restart orphan — no in-memory handle, so no Job Object either):
/// a start-token-guarded `kill_process_tree`, FAIL-CLOSED. We signal the orphan
/// ONLY when its start-token identity is fully confirmed: a stored token is
/// present AND the live pid's current token is present AND they match. If the
/// stored token is `None`, or the pid's current token can't be read, or they
/// differ, we do NOT signal — a recycled pid belonging to an innocent process is
/// never killed (the prior fail-OPEN logic killed when the stored token was
/// `None`). On windows this restart-orphan path is leader-only (`TerminateProcess`
/// — bare `kill_process_tree` on windows has no Job Object to close); restart
/// orphans are rare (the primary live path uses the job), so a leader-only
/// fallback here is an accepted edge.
///
/// PR3a: group-kill is now DONE (the PR2 "group-kill DEFERRED" note is resolved) —
/// unix=pgid group-kill, windows=Job Object KILL_ON_JOB_CLOSE. Route B has NO
/// protocol cancel (confirmed for opencode in the ACP sub-spike), so cancel stays a
/// hard process-tree kill; a PTY-level graceful quit (the backend's `quit_command`)
/// before the kill is an optional later refinement, not a protocol step.
fn terminate_worker(w: &EphemeralWorker) {
    if let Some(handle) = LIVE_CHILDREN.lock().remove(&w.worker_id) {
        // PRIMARY live path. unix: pgid group-kill. windows: the leader kill plus
        // the Job-Object close (on handle drop below) takes down the whole tree.
        crate::process::kill_process_tree(handle.pid());
        let _ = handle.child.lock().wait();
        // `handle` drops here → on windows the Job Object handle closes →
        // KILL_ON_JOB_CLOSE terminates any descendants the leader-kill missed.
        return;
    }
    if w.pid == 0 || !crate::process::is_pid_alive(w.pid) {
        return;
    }
    // No handle (restart orphan): FAIL-CLOSED — only terminate when the start-token
    // identity is FULLY confirmed (stored Some, current Some, equal). A missing
    // stored token, an unreadable current token, or a mismatch all mean "cannot
    // prove this pid is still our worker" → do NOT signal a possibly-recycled pid.
    match (
        w.process_start_token,
        crate::process::process_start_token(w.pid),
    ) {
        (Some(stored), Some(current)) if stored == current => {
            crate::process::kill_process_tree(w.pid);
        }
        _ => {}
    }
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
        assert!(
            matches!(res, Err(ReserveError::CapExceeded(live)) if live == cap),
            "at cap must reject with CapExceeded({cap})"
        );
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

    // ───────────────────────── Fix 1: backend allowlist (SECURITY) ──────────────

    /// The security keystone: `resolve_supported_backend` rejects a PATH (arbitrary-
    /// exec vector) and an unknown basename, accepts a known backend basename, and
    /// — critically — returns the CANONICAL command, never the raw input (so a
    /// `claude-evil` basename launches the real `claude`, not the attacker's name).
    #[test]
    fn resolve_supported_backend_rejects_paths_and_canonicalizes() {
        // Paths (forward + back slash) are rejected — arbitrary-exec vector.
        assert!(resolve_supported_backend("/tmp/evil.sh").is_err());
        assert!(resolve_supported_backend("./x").is_err());
        assert!(resolve_supported_backend("..\\evil.exe").is_err());
        assert!(
            resolve_supported_backend("/usr/local/bin/claude").is_err(),
            "even a path whose basename IS a backend is rejected — paths are never allowed"
        );
        // Unknown bare basenames are rejected.
        assert!(resolve_supported_backend("bogus").is_err());
        assert!(resolve_supported_backend("").is_err());
        // Known backends resolve to their CANONICAL command.
        assert_eq!(resolve_supported_backend("opencode"), Ok("opencode"));
        assert_eq!(resolve_supported_backend("claude"), Ok("claude"));
        // `claude-*` prefix maps to ClaudeCode but is canonicalized to `claude`
        // (NEVER the raw `claude-2.1` — no attacker-controlled name reaches spawn).
        assert_eq!(resolve_supported_backend("claude-2.1"), Ok("claude"));
        assert_eq!(
            resolve_supported_backend("claude-evil"),
            Ok("claude"),
            "a hostile claude-prefixed basename must launch the canonical `claude`"
        );
        // `kiro` alias canonicalizes to `kiro-cli`.
        assert_eq!(resolve_supported_backend("kiro"), Ok("kiro-cli"));
        assert_eq!(resolve_supported_backend("codex"), Ok("codex"));
        assert_eq!(resolve_supported_backend("agy"), Ok("agy"));
    }

    /// `spawn_and_track` with a PATH backend is rejected as `UnsupportedBackend`
    /// BEFORE any slot is reserved and BEFORE any OS process is spawned — the
    /// shared-sink gate (so a direct caller, not just the MCP handler, is protected).
    #[test]
    fn spawn_and_track_rejects_path_backend_no_reserve_no_process() {
        let home = tmp_home("evil-backend");
        let spec = SpawnSpec {
            workflow_id: "wf".to_string(),
            parent: None,
            backend: "/tmp/evil.sh".to_string(),
            ttl_secs: Some(60),
            token_budget: None,
        };
        let res = spawn_and_track(&home, spec);
        assert!(
            matches!(&res, Err(SpawnError::UnsupportedBackend(b)) if b == "/tmp/evil.sh"),
            "a path backend must be rejected as UnsupportedBackend: {res:?}"
        );
        assert_eq!(
            list(&home, None).len(),
            0,
            "rejection happens before reserve — NO row is created (and no process)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// `spawn_and_track` with an unknown basename is also rejected, no reservation.
    #[test]
    fn spawn_and_track_rejects_unknown_backend() {
        let home = tmp_home("unknown-backend");
        let spec = SpawnSpec {
            workflow_id: "wf".to_string(),
            parent: None,
            backend: "bogus".to_string(),
            ttl_secs: Some(60),
            token_budget: None,
        };
        assert!(matches!(
            spawn_and_track(&home, spec),
            Err(SpawnError::UnsupportedBackend(_))
        ));
        assert_eq!(list(&home, None).len(), 0);
        std::fs::remove_dir_all(&home).ok();
    }

    // ───────────────────── Fix 4: PID identity fail-CLOSED on reap ──────────────

    /// `terminate_worker`'s NO-HANDLE (restart-orphan) path must FAIL CLOSED: a
    /// worker whose stored start-token is `None` must NOT be killed (the old
    /// fail-OPEN logic killed it, risking an innocent recycled pid). We point the
    /// row at a LIVE pid we own (this test process) with no stored token and assert
    /// it survives the terminate call — there is no in-memory handle, so the
    /// no-handle branch runs.
    #[test]
    fn terminate_no_handle_fails_closed_when_token_none() {
        let self_pid = std::process::id();
        let w = EphemeralWorker {
            worker_id: "orphan-no-token".to_string(),
            pid: self_pid,
            process_start_token: None, // unknown identity → must NOT kill
            status: STATUS_RUNNING.to_string(),
            spawned_at: chrono::Utc::now().to_rfc3339(),
            ttl_secs: 100,
            ..Default::default()
        };
        // No LIVE_CHILDREN entry for this worker_id → no-handle branch.
        terminate_worker(&w);
        assert!(
            crate::process::is_pid_alive(self_pid),
            "fail-closed: a None stored token must NOT signal the pid"
        );
    }

    /// Fail-CLOSED when the stored token is Some but the live pid's CURRENT token
    /// is unavailable/mismatched: still must NOT kill. We use a deliberately bogus
    /// stored token against our own live pid; the current token won't match, so the
    /// guard must refuse to signal.
    #[test]
    fn terminate_no_handle_fails_closed_on_token_mismatch() {
        let self_pid = std::process::id();
        // A stored token that cannot equal the live process's real start token.
        let w = EphemeralWorker {
            worker_id: "orphan-mismatch".to_string(),
            pid: self_pid,
            process_start_token: Some(0xDEAD_BEEF_DEAD_BEEF),
            status: STATUS_RUNNING.to_string(),
            spawned_at: chrono::Utc::now().to_rfc3339(),
            ttl_secs: 100,
            ..Default::default()
        };
        terminate_worker(&w);
        assert!(
            crate::process::is_pid_alive(self_pid),
            "fail-closed: a mismatched stored token must NOT signal the pid"
        );
    }

    /// Fail-CLOSED inverse: when the stored token MATCHES the live pid's current
    /// token (full identity confirmed), the no-handle path DOES kill. Spawn a real
    /// throwaway process, read its real start-token, and assert terminate reaps it.
    #[test]
    fn terminate_no_handle_kills_on_token_match() {
        let (program, args) = long_lived_cmd();
        let mut cmd = std::process::Command::new(&program);
        cmd.args(&args);
        // The no-handle path calls `kill_process_tree`, which SIGKILLs the target's
        // whole PROCESS GROUP. The child must be in its OWN group (setsid) or it
        // would take the test runner down with it. (The spawned-via-PTY workers in
        // the other tests are session leaders already, so this only matters here.)
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        let mut child = cmd.spawn().expect("spawn throwaway long-lived process");
        let pid = child.id();
        let token = crate::process::process_start_token(pid);
        assert!(
            token.is_some(),
            "the platform must report a start token for the matched-kill case"
        );
        let w = EphemeralWorker {
            worker_id: "orphan-match".to_string(),
            pid,
            process_start_token: token, // stored == current → confirmed identity
            status: STATUS_RUNNING.to_string(),
            spawned_at: chrono::Utc::now().to_rfc3339(),
            ttl_secs: 100,
            ..Default::default()
        };
        terminate_worker(&w);
        let _ = child.wait(); // reap our own spawned child's zombie
        assert_dead_within(pid, "confirmed-identity orphan must be killed");
    }

    // ─────────────────── Fix 5: durable cap reservation fail-closed ─────────────

    /// `try_reserve_slot` must NOT swallow a store-write failure: it must surface
    /// `ReserveError::Persist`. We induce a write failure by pointing the store at a
    /// path whose parent is a FILE (so `mutate_versioned`'s `create_dir_all(parent)`
    /// errors). `spawn_and_track` is then driven against the same broken home and
    /// must reject with `SpawnError::Reserve` WITHOUT OS-spawning.
    #[test]
    fn reserve_persist_failure_fails_closed_no_spawn() {
        // home is a regular FILE, so store_path(home) = <file>/ephemeral_workers.json
        // and mutate_versioned's create_dir_all(parent==<file>) fails.
        let file_home = std::env::temp_dir().join(format!(
            "agend-eph-unwritable-{}-{}",
            std::process::id(),
            WORKER_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&file_home, b"x").expect("create the blocking file");

        // try_reserve_slot surfaces the persist error (does not swallow → Ok).
        let r = try_reserve_slot(&file_home, reserving_row("r", 0));
        assert!(
            matches!(r, Err(ReserveError::Persist(_))),
            "a store-write failure must surface as ReserveError::Persist, not be swallowed: {r:?}"
        );

        // spawn_and_track maps it to SpawnError::Reserve and does NOT spawn.
        let spec = SpawnSpec {
            workflow_id: "wf".to_string(),
            parent: None,
            backend: "claude".to_string(), // valid backend so we reach the reserve step
            ttl_secs: Some(60),
            token_budget: None,
        };
        let res = spawn_and_track(&file_home, spec);
        assert!(
            matches!(res, Err(SpawnError::Reserve(_))),
            "a persist failure must fail closed (reject, no spawn): {res:?}"
        );
        std::fs::remove_file(&file_home).ok();
    }

    /// Fix 3 (production-shaped tree-kill): spawn a LEADER that itself backgrounds a
    /// long-lived GRANDCHILD, capture BOTH pids, reap through the real
    /// `terminate_worker` (handle path), and assert BOTH are dead. This replaces the
    /// old childless-process coverage: on unix the pgid group-kill must reach the
    /// grandchild; on windows the Job Object's KILL_ON_JOB_CLOSE must (the assigned
    /// tree dies on handle drop).
    #[test]
    fn reap_kills_leader_and_grandchild_tree() {
        let home = tmp_home("tree-kill");
        let seq = WORKER_SEQ.fetch_add(1, Ordering::Relaxed);
        let worker_id = format!("eph-tree-{}-{}", std::process::id(), seq);

        // A leader that backgrounds a real grandchild and prints the grandchild pid,
        // then waits — so the leader stays alive holding the tree open.
        #[cfg(unix)]
        let (program, args) = (
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "sleep 300 & echo $!; wait".to_string()],
        );
        #[cfg(windows)]
        let (program, args) = (
            "cmd".to_string(),
            vec![
                "/c".to_string(),
                // `timeout` is a transient delay child; `ping -t` is the LONG-LIVED
                // descendant we capture + assert dies via KILL_ON_JOB_CLOSE. The 2s
                // delay makes `ping -t` spawn AFTER the Job Object is assigned to the
                // leader — otherwise a descendant created in the ~ms spawn→assign
                // window would escape the job (the known assign-after-spawn race, see
                // create_kill_on_close_job). This delay is a TEST artifact to prove
                // the kill deterministically, not a production requirement.
                "timeout /t 2 /nobreak >NUL && ping -t 127.0.0.1 >NUL".to_string(),
            ],
        );

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
        let handle = crate::agent::spawn_ephemeral_worker(&config).expect("spawn leader");
        let leader_pid = handle.pid();
        assert!(crate::process::is_pid_alive(leader_pid), "leader is alive");

        // On unix, read the grandchild pid the leader echoed to its PTY by draining
        // a writer-side… simpler: the read-loop drains the PTY, so we instead spawn
        // our OWN observation: re-run the same shell isn't representative. Use the
        // leader's process group to find a real grandchild via the OS.
        #[cfg(unix)]
        let grandchild_pid: Option<u32> = {
            // Poll for a child process of the leader (the backgrounded `sleep`).
            // `pgrep -P <leader>` lists direct children; the sleep is a grandchild
            // of sh's subshell but a child of the leader's group — pgrep -g covers
            // the group. We accept either path: any pid in the leader's group that
            // is NOT the leader.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut found = None;
            while std::time::Instant::now() < deadline {
                let out = std::process::Command::new("pgrep")
                    .args(["-g", &leader_pid.to_string()])
                    .output();
                if let Ok(o) = out {
                    for line in String::from_utf8_lossy(&o.stdout).lines() {
                        if let Ok(p) = line.trim().parse::<u32>() {
                            if p != leader_pid && crate::process::is_pid_alive(p) {
                                found = Some(p);
                            }
                        }
                    }
                }
                if found.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            found
        };
        #[cfg(unix)]
        assert!(
            grandchild_pid.is_some(),
            "a real grandchild (backgrounded sleep) must exist in the leader's group"
        );

        // Windows: capture the LONG-LIVED descendant (the leader's `ping -t` child)
        // and assert IT dies on reap — proving the Job Object reaps DESCENDANTS, not
        // just the leader (a leader-only check would pass even with a broken job).
        #[cfg(windows)]
        let grandchild_pid: Option<u32> = {
            // Sleep past the leader's 2s delay so `ping -t` has spawned AFTER the job
            // assignment, then find it as a child of the leader via WMI/CIM.
            std::thread::sleep(std::time::Duration::from_secs(3));
            let mut found = None;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
            while std::time::Instant::now() < deadline {
                let out = std::process::Command::new("powershell")
                    .args([
                        "-NoProfile",
                        "-Command",
                        &format!(
                            "Get-CimInstance Win32_Process -Filter \"ParentProcessId={leader_pid}\" | ForEach-Object {{ $_.ProcessId }}"
                        ),
                    ])
                    .output();
                if let Ok(o) = out {
                    for line in String::from_utf8_lossy(&o.stdout).lines() {
                        if let Ok(p) = line.trim().parse::<u32>() {
                            if p != leader_pid && crate::process::is_pid_alive(p) {
                                found = Some(p);
                            }
                        }
                    }
                }
                if found.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            found
        };
        #[cfg(windows)]
        assert!(
            grandchild_pid.is_some(),
            "a long-lived descendant (ping -t, spawned post-assignment) must exist under the leader"
        );

        // Track the worker so the reap goes through the REAL terminate_worker
        // (handle path: unix pgid group-kill / windows Job Object close).
        let token = crate::process::process_start_token(leader_pid);
        let mut reserving = reserving_row(&worker_id, 0);
        reserving.backend = "sleep".to_string();
        try_reserve_slot(&home, reserving).expect("reserve");
        finalize_spawn(&home, &worker_id, leader_pid, token).expect("finalize");
        LIVE_CHILDREN.lock().insert(worker_id.clone(), handle);

        let reaped = reap_one(&home, &worker_id).expect("reap_one");
        assert_eq!(reaped.worker_id, worker_id);

        assert_dead_within(leader_pid, "leader must be killed by the tree reap");
        #[cfg(unix)]
        if let Some(gc) = grandchild_pid {
            assert_dead_within(gc, "the grandchild must ALSO be killed (pgid group-kill)");
        }
        #[cfg(windows)]
        if let Some(gc) = grandchild_pid {
            assert_dead_within(
                gc,
                "the descendant must ALSO be killed (Job Object KILL_ON_JOB_CLOSE)",
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }
}
