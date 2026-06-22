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
    pub phase: String, // "reserving" | "spawned" | "running" | "prompting" | "done"
    pub status: String, // "reserving" | "running" | "done"
    /// PR3b: the driver's captured turn transcript slice (the worker's "answer").
    /// `None` until the one-shot turn completes; absent on workers spawned without
    /// a prompt (PR3a lifecycle-only spawn). Coarse — durable telemetry is PR4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    /// PR3b: the success oracle's verdict for the turn (terminal Idle ∧ ¬error-class
    /// ∧ transcript grew). `None` until the turn completes (or no prompt was given).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
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
    /// PR3b: the prompt the driver injects for the worker's one-shot turn. EMPTY =
    /// PR3a lifecycle-only spawn (no driver thread launched — spawn + reap only).
    pub prompt: String,
    /// PR3b: optional model override (e.g. `provider/model` for opencode), passed to
    /// the backend via its spawn argv. `None` = the backend's configured default.
    pub model: Option<String>,
}

/// Why a spawn failed.
#[derive(Debug)]
pub enum SpawnError {
    /// The requested `backend` is not on the allowlist (a path, or a basename
    /// that doesn't map to a known backend) — an arbitrary-exec guard. Carries the
    /// rejected input for the operator-facing error.
    UnsupportedBackend(String),
    /// PR3b: a `prompt` was given for a backend whose one-shot driver is not yet
    /// smoke-validated. Slice-1 ships the opencode driver ONLY; other backends are
    /// Slice-2 (each gated on a §5 per-backend smoke per the confirm-first iron rule).
    /// Carries the rejected backend. A prompt-less spawn of any backend is unaffected
    /// (the PR3a lifecycle-only spawn path).
    DriverUnsupported(String),
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
            SpawnError::DriverUnsupported(b) => write!(
                f,
                "ephemeral spawn: a prompt-driven one-shot turn is only supported for 'opencode' \
                 (PR3b Slice-1); backend '{b}' is Slice-2 (pending a per-backend turn-detection \
                 smoke). Spawn it without a prompt for a lifecycle-only worker."
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

/// Why `finalize_spawn` failed. Both variants MUST trigger the orphan-kill +
/// reject in `spawn_and_track` (no handle inserted, no success, no stale running
/// row): the reserving row is gone, OR the finalize write could not be persisted.
#[derive(Debug)]
enum FinalizeError {
    /// The reserving row vanished before finalize (e.g. concurrently reaped as
    /// stale) — there is nothing to flip to `running`.
    RowVanished,
    /// The store write failed (FAIL-CLOSED, mirroring the reserve-side fix): the
    /// on-disk row would stay a stale `reserving` (pid=0) with no durable PID to
    /// reap after a crash, so finalize must NOT return `Ok` — it surfaces the error
    /// and the caller kills the just-spawned orphan.
    Persist(std::io::Error),
}

/// Finalize a reserved slot once the real process is spawned: stamp the real pid
/// and start-token, then flip `STATUS_RESERVING` → `STATUS_RUNNING` (the wall-TTL
/// clock restarts at the real spawn time). Returns the finalized worker, or `Err`:
/// `RowVanished` if the reserving row is gone, or `Persist` if the store write fails
/// (FAIL-CLOSED — mirrors `try_reserve_slot`). The caller kills the just-spawned
/// orphan on EITHER error so a persist failure can never leave a stale `reserving`
/// row (pid=0, unreapable after a crash) plus a live unattached process.
fn finalize_spawn(
    home: &Path,
    worker_id: &str,
    pid: u32,
    token: Option<u64>,
) -> Result<EphemeralWorker, FinalizeError> {
    // Fix 2 (failure-seam, TEST-ONLY): a test arms this to force a finalize PERSIST
    // failure on a REAL `spawn_and_track`, proving the orphan-kill + reject branch.
    // Zero-cost / absent in production builds (`#[cfg(test)]`).
    #[cfg(test)]
    if finalize_test_seam::force_persist_fail() {
        return Err(FinalizeError::Persist(std::io::Error::other(
            "test: forced finalize persist failure",
        )));
    }
    let mut finalized = None;
    // Direct `mutate_versioned` (NOT `persist_or_log!`, which would SWALLOW the
    // write error and let us return `Ok` with a stale on-disk `reserving` row).
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
    })
    .map_err(|e| FinalizeError::Persist(std::io::Error::other(e.to_string())))?;
    finalized.ok_or(FinalizeError::RowVanished)
}

/// Fix 2 (failure-seam, TEST-ONLY): a process-local toggle the finalize-persist
/// fail-closed test arms so `finalize_spawn` returns `FinalizeError::Persist` on a
/// REAL `spawn_and_track`. Entirely `#[cfg(test)]` — absent in production builds.
#[cfg(test)]
mod finalize_test_seam {
    use std::sync::atomic::{AtomicBool, Ordering};

    static FORCE_PERSIST_FAIL: AtomicBool = AtomicBool::new(false);

    pub(super) fn force_persist_fail() -> bool {
        FORCE_PERSIST_FAIL.load(Ordering::SeqCst)
    }

    // `#[cfg(unix)]`: the only arm site is the unix-only finalize fail-closed test
    // (Windows ephemeral spawn is fail-closed → no real finalize to drive there).
    #[cfg(unix)]
    pub(super) fn set_force_persist_fail(on: bool) {
        FORCE_PERSIST_FAIL.store(on, Ordering::SeqCst);
    }
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

/// PR3b: the driver flips a worker's phase to `"prompting"` just before it injects the
/// prompt, so `ephemeral list` reflects the mid-turn state. Best-effort (log-on-fail) —
/// a lost write only loses an observability nuance, not correctness. A vanished row
/// (concurrently reaped) is a no-op.
pub(crate) fn mark_prompting(home: &Path, worker_id: &str) {
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |s: &mut EphemeralStore| {
            if let Some(w) = s.workers.iter_mut().find(|w| w.worker_id == worker_id) {
                w.phase = "prompting".to_string();
            }
            Ok(())
        }),
        "ephemeral_mark_prompting"
    );
}

/// PR3b: the driver writes the one-shot turn's outcome to the worker row and marks it
/// terminal (`phase` + `status` = `"done"`), so the next [`reap_sweep`] terminates the
/// now-idle worker process ([`is_due`] fires on `STATUS_DONE`) and frees the cap slot.
/// Best-effort persist (log-on-fail) — a lost write only delays reap to the wall-TTL
/// backstop. A vanished row (concurrently reaped) is a no-op.
pub(crate) fn record_result(home: &Path, worker_id: &str, result_summary: String, success: bool) {
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |s: &mut EphemeralStore| {
            if let Some(w) = s.workers.iter_mut().find(|w| w.worker_id == worker_id) {
                w.result_summary = Some(result_summary.clone());
                w.success = Some(success);
                w.phase = STATUS_DONE.to_string();
                w.status = STATUS_DONE.to_string();
            }
            Ok(())
        }),
        "ephemeral_record_result"
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

    // 0b) DRIVER GATE (PR3b) — a `prompt` launches the one-shot driver (inject →
    //     turn-end → capture → oracle), which is smoke-validated for opencode ONLY
    //     (Slice-1). Reject a prompt for any other backend BEFORE reserving/spawning
    //     (confirm-first iron rule: never drive a backend whose turn-end detection
    //     isn't proven on a real turn). A prompt-LESS spawn of any backend is the PR3a
    //     lifecycle-only path and is unaffected. Gated in the SHARED SINK so a direct
    //     caller, not just the MCP handler, is protected.
    let drive_turn = !spec.prompt.is_empty();
    if drive_turn && backend_command != crate::backend::Backend::OpenCode.preset().command {
        return Err(SpawnError::DriverUnsupported(spec.backend.clone()));
    }

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
        result_summary: None,
        success: None,
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
    // PR3b: an optional model override goes into the spawn argv (opencode gets a
    // provider-prefixed `--model`; see `Backend::push_model_arg`). Empty/None = no-op,
    // so a model-less spawn is byte-identical to the PR3a `args: &[]`.
    let mut spawn_args: Vec<String> = Vec::new();
    if let Some(model) = spec.model.as_deref() {
        crate::backend::Backend::push_model_arg(&mut spawn_args, backend_command, model);
    }
    let config = crate::agent::SpawnConfig {
        name: &worker_id,
        backend_command, // canonical (allowlist-resolved) — never the raw input
        args: &spawn_args,
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
            // PR3b: launch the one-shot driver BEFORE moving the handle into
            // LIVE_CHILDREN, capturing the inject target (Arc clones of the retained
            // pty_writer + core) so the driver is decoupled from the handle the reaper
            // owns. Only when a prompt was given (opencode, gated in step 0b) — else
            // this is the PR3a lifecycle-only spawn (no driver thread).
            if drive_turn {
                let inject_target = crate::agent::InjectTarget::from_ephemeral(
                    &handle,
                    crate::backend::Backend::OpenCode,
                );
                crate::ephemeral_driver::spawn_driver(crate::ephemeral_driver::DriverConfig {
                    home: home.to_path_buf(),
                    worker_id: worker_id.clone(),
                    prompt: spec.prompt.clone(),
                    inject_target,
                    wall_ttl: std::time::Duration::from_secs(ttl_secs),
                });
            }
            LIVE_CHILDREN.lock().insert(worker_id, handle);
            Ok(worker)
        }
        // BOTH finalize errors fail CLOSED through the SAME orphan-kill + reject
        // branch: kill the just-spawned tree, DO NOT insert the handle into
        // LIVE_CHILDREN, return a `SpawnError` (no success, no leaked process, no
        // leaked handle, no stale `running` row). A PTY child is its own session
        // leader, so kill_process_tree reaps any children it forked too.
        Err(e) => {
            crate::process::kill_process_tree(pid);
            let _ = handle.child.lock().wait();
            let msg = match e {
                FinalizeError::RowVanished => {
                    "ephemeral finalize: reserving slot vanished before finalize".to_string()
                }
                // FAIL-CLOSED: a persist failure during finalize would otherwise
                // leave a stale `reserving` row + a live unattached process.
                FinalizeError::Persist(e) => {
                    // Best-effort: also try to drop the stale reserving row so the
                    // cap slot is freed (release itself is log-on-fail).
                    release_reservation(home, &worker_id);
                    format!("ephemeral finalize: failed to persist the running row (fail closed, orphan killed): {e}")
                }
            };
            Err(SpawnError::Spawn(std::io::Error::other(msg)))
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
/// **Platform scope:** ephemeral real-backend spawn is UNSUPPORTED on Windows —
/// [`crate::agent::spawn_ephemeral_worker`] fails closed there before creating any
/// process, so no live ephemeral worker ever exists on Windows and this fn's
/// live-handle path is unix-only in practice. The tree-kill mechanism is the unix
/// pgid group-kill; there is no Windows Job Object (the spawn→assign race made it
/// unsound, see `spawn_ephemeral_worker`).
///
/// If we still hold the live [`crate::agent::EphemeralPtyHandle`] (spawned this
/// daemon life), the PRIMARY live path: the whole worker tree is killed and the
/// leader's zombie reaped via a **unix pgid group-kill**
/// ([`crate::process::kill_process_tree`]) — a PTY child is its own session leader,
/// so the PGID covers every grandchild the backend forked. Then `wait()` collects
/// the leader's exit (no zombie) and the handle drops.
///
/// Otherwise (a restart orphan — no in-memory handle): a start-token-guarded
/// `kill_process_tree`, FAIL-CLOSED. We signal the orphan ONLY when its start-token
/// identity is fully confirmed: a stored token is present AND the live pid's current
/// token is present AND they match. If the stored token is `None`, or the pid's
/// current token can't be read, or they differ, we do NOT signal — a recycled pid
/// belonging to an innocent process is never killed (the prior fail-OPEN logic
/// killed when the stored token was `None`).
///
/// PR3a: group-kill is now DONE (the PR2 "group-kill DEFERRED" note is resolved) —
/// unix=pgid group-kill, Windows=spawn fail-closed (no live ephemeral workers to
/// reap). Route B has NO protocol cancel (confirmed for opencode in the ACP
/// sub-spike), so cancel stays a hard process-tree kill; a PTY-level graceful quit
/// (the backend's `quit_command`) before the kill is an optional later refinement,
/// not a protocol step.
fn terminate_worker(w: &EphemeralWorker) {
    if let Some(handle) = LIVE_CHILDREN.lock().remove(&w.worker_id) {
        // PRIMARY live path: unix pgid group-kill — a PTY child is its own session
        // leader, so killing the group reaps every grandchild the backend forked.
        // (Windows never reaches here: spawn is fail-closed, so no handle exists.)
        crate::process::kill_process_tree(handle.pid());
        let _ = handle.child.lock().wait();
        // `handle` drops here, closing the retained PTY master.
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

    /// Serializes every test that calls `spawn_ephemeral_worker` / `finalize_spawn`.
    /// The Fix 2 + Fix 3 failure SEAMS are process-global (`AtomicBool`s in
    /// `finalize_test_seam` / `crate::agent::test_seam`); cargo runs tests in
    /// parallel, so a spawn/finalize in one test could observe a flag another test
    /// armed. Holding this lock for the whole arm→spawn→disarm window (and in every
    /// other spawn-touching test) makes the seam observation deterministic.
    fn seam_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

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

    /// A real long-lived backend command (program + args) so the spawn→reap
    /// lifecycle + termination are exercised. Points `spawn_ephemeral_worker` at a
    /// stand-in long-lived binary (`/bin/sleep`), which resolves to
    /// `Backend::from_command` → `None` (no preset args / spawn flags), so the test
    /// needs no installed backend. `#[cfg(unix)]`: every caller is unix-only now that
    /// ephemeral spawn is fail-closed on Windows.
    #[cfg(unix)]
    fn long_lived_cmd() -> (String, Vec<String>) {
        ("/bin/sleep".to_string(), vec!["30".to_string()])
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
    /// object cleanup can lag briefly, so a death check must NOT assert immediately.
    /// The caller must have dropped the `Child` handle first. `#[cfg(unix)]`: every
    /// caller is unix-only now that ephemeral spawn is fail-closed on Windows.
    #[cfg(unix)]
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

    /// Track a REAL ephemeral PTY worker through the SAME
    /// reserve→spawn→finalize→insert pipeline `spawn_and_track` runs, but with a
    /// long-lived stand-in command (`/bin/sleep`) so the test needs no installed
    /// backend. `spawn_and_track` hardcodes `args:&[]` (a real backend needs none),
    /// so the test drives `spawn_ephemeral_worker` directly (where it controls
    /// args) and then finalizes + inserts into `LIVE_CHILDREN` identically — so the
    /// reap path under test goes through the real `terminate_worker`. `#[cfg(unix)]`:
    /// `spawn_ephemeral_worker` is fail-closed on Windows (no live worker to track).
    #[cfg(unix)]
    fn track_real_worker(home: &Path, workflow_id: &str, ttl_secs: u64) -> EphemeralWorker {
        // Hold the seam lock across spawn+finalize so a concurrently-armed failure
        // seam (Fix 2 / Fix 3) cannot make this otherwise-happy spawn/finalize fail.
        let _seam = seam_lock();
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

    /// Full lifecycle against a REAL ephemeral PTY worker:
    /// reserve→spawn→finalize → list → reap, and the reap genuinely terminates the
    /// process tree (is_pid_alive → false). `#[cfg(unix)]`: ephemeral spawn is
    /// fail-closed on Windows.
    #[cfg(unix)]
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
    /// `#[cfg(unix)]`: ephemeral spawn is fail-closed on Windows.
    #[cfg(unix)]
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
            prompt: String::new(),
            model: None,
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

    /// PR3a Route-B core: `spawn_ephemeral_worker` spawns a REAL long-lived process
    /// via a PTY, reports its live pid, and is NOT in any agent registry — the fn
    /// takes NO `AgentRegistry` argument, so by construction there is zero roster
    /// involvement (no pane, no router subscriber). A `kill_process_tree(pid)` then
    /// reaps the whole PTY-session tree. `#[cfg(unix)]`: ephemeral spawn is
    /// fail-closed on Windows.
    #[cfg(unix)]
    #[test]
    fn spawn_ephemeral_worker_no_registry_then_kill_tree_reaps() {
        let _seam = seam_lock();
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
            prompt: String::new(),
            model: None,
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
            prompt: String::new(),
            model: None,
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
    /// `#[cfg(unix)]`: uses the unix-only `long_lived_cmd` + setsid group semantics.
    #[cfg(unix)]
    #[test]
    fn terminate_no_handle_kills_on_token_match() {
        let (program, args) = long_lived_cmd();
        let mut cmd = std::process::Command::new(&program);
        cmd.args(&args);
        // The no-handle path calls `kill_process_tree`, which SIGKILLs the target's
        // whole PROCESS GROUP. The child must be in its OWN group (setsid) or it
        // would take the test runner down with it. (The spawned-via-PTY workers in
        // the other tests are session leaders already, so this only matters here.)
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
            prompt: String::new(),
            model: None,
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
    /// `terminate_worker` (handle path), and assert BOTH are dead — the unix pgid
    /// group-kill must reach the grandchild. `#[cfg(unix)]`: ephemeral spawn is
    /// fail-closed on Windows (no live worker → no live-handle reap path to exercise).
    #[cfg(unix)]
    #[test]
    fn reap_kills_leader_and_grandchild_tree() {
        let _seam = seam_lock();
        let home = tmp_home("tree-kill");
        let seq = WORKER_SEQ.fetch_add(1, Ordering::Relaxed);
        let worker_id = format!("eph-tree-{}-{}", std::process::id(), seq);

        // A leader that backgrounds a real grandchild and prints the grandchild pid,
        // then waits — so the leader stays alive holding the tree open.
        let (program, args) = (
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "sleep 300 & echo $!; wait".to_string()],
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

        // Read the grandchild pid via the leader's process group: the read-loop
        // drains the PTY, so we find the backgrounded `sleep` through the OS instead.
        // `pgrep -g <leader>` lists the leader's group; the sleep is a grandchild of
        // sh's subshell but in the leader's group, so any group pid that is NOT the
        // leader is our grandchild.
        let grandchild_pid: Option<u32> = {
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
        assert!(
            grandchild_pid.is_some(),
            "a real grandchild (backgrounded sleep) must exist in the leader's group"
        );

        // Track the worker so the reap goes through the REAL terminate_worker
        // (handle path: unix pgid group-kill).
        let token = crate::process::process_start_token(leader_pid);
        let mut reserving = reserving_row(&worker_id, 0);
        reserving.backend = "sleep".to_string();
        try_reserve_slot(&home, reserving).expect("reserve");
        finalize_spawn(&home, &worker_id, leader_pid, token).expect("finalize");
        LIVE_CHILDREN.lock().insert(worker_id.clone(), handle);

        let reaped = reap_one(&home, &worker_id).expect("reap_one");
        assert_eq!(reaped.worker_id, worker_id);

        assert_dead_within(leader_pid, "leader must be killed by the tree reap");
        if let Some(gc) = grandchild_pid {
            assert_dead_within(gc, "the grandchild must ALSO be killed (pgid group-kill)");
        }
        std::fs::remove_dir_all(&home).ok();
    }

    // ─────────────── Fix 1: Windows ephemeral spawn is FAIL-CLOSED ───────────────

    /// `spawn_ephemeral_worker` is UNSUPPORTED on Windows: it must return `Err`
    /// BEFORE openpty/spawn (no process is created), with a message naming the
    /// platform. The unix tree-reap (pgid group-kill) has no race-free Windows
    /// analogue (Job Object can only be assigned post-spawn), so Windows fails
    /// closed rather than ship a tree-reap with an escape hole.
    #[cfg(windows)]
    #[test]
    fn ephemeral_spawn_unsupported_on_windows() {
        let worker_id = format!("eph-win-failclosed-{}", std::process::id());
        // A real backend command ("cmd" exists on Windows) so we are NOT rejected by
        // some earlier validation — the fail-closed return is what we assert. If a
        // process WERE spawned it would be a `cmd` we'd have to reap; the early
        // return guarantees none is, so there is nothing to clean up.
        let config = crate::agent::SpawnConfig {
            name: &worker_id,
            backend_command: "cmd",
            args: &[],
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
        let res = crate::agent::spawn_ephemeral_worker(&config);
        let err = res.expect_err("ephemeral spawn must fail closed on Windows");
        assert!(
            err.to_string().contains("unsupported on Windows"),
            "the error must name the platform: {err}"
        );
    }

    // ───────────── Fix 2: finalize-persist FAIL-CLOSED (orphan-kill) ─────────────

    /// `finalize_spawn` must NOT swallow a store-write failure: it surfaces
    /// `FinalizeError::Persist` (mirroring the reserve-side fix). We induce a write
    /// failure by pointing the store at a path whose parent is a FILE (so
    /// `mutate_versioned`'s `create_dir_all(parent)` errors) — cross-platform, no
    /// spawn needed.
    #[test]
    fn finalize_persist_failure_surfaces_error_not_ok() {
        // Lock so a concurrently-armed finalize seam can't change the FAILURE CAUSE
        // (the assertion holds either way, but this keeps the cause = the real write).
        let _seam = seam_lock();
        let file_home = std::env::temp_dir().join(format!(
            "agend-eph-finalize-unwritable-{}-{}",
            std::process::id(),
            WORKER_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&file_home, b"x").expect("create the blocking file");
        let r = finalize_spawn(&file_home, "any-id", 1234, Some(99));
        assert!(
            matches!(r, Err(FinalizeError::Persist(_))),
            "a finalize store-write failure must surface as FinalizeError::Persist, not Ok or RowVanished: {r:?}"
        );
        std::fs::remove_file(&file_home).ok();
    }

    /// End-to-end FAIL-CLOSED: reserve → spawn a REAL child → `finalize_spawn` forced
    /// (test seam) to fail its PERSIST step, then run `spawn_and_track`'s finalize-Err
    /// branch verbatim and assert it fails closed — `finalize_spawn` returns
    /// `Persist`, the just-spawned child is orphan-KILLED (kill_tree + wait), NO
    /// handle leaks into `LIVE_CHILDREN`, and `list()` shows NO `running` row (the
    /// stale reserving row is released too). This shares the EXACT branch
    /// `spawn_and_track` takes on a persist failure (driving `spawn_and_track` itself
    /// would need an installed backend; reproducing its branch keeps the test
    /// hermetic with `/bin/sleep`). `#[cfg(unix)]`: ephemeral spawn is fail-closed on
    /// Windows.
    #[cfg(unix)]
    #[test]
    fn finalize_persist_failure_fails_closed_kills_orphan() {
        let _seam = seam_lock();
        let home = tmp_home("finalize-failclosed");
        let seq = WORKER_SEQ.fetch_add(1, Ordering::Relaxed);
        let worker_id = format!("eph-finalize-{}-{}", std::process::id(), seq);
        let mut reserving = reserving_row(&worker_id, 0);
        reserving.backend = "sleep".to_string();
        try_reserve_slot(&home, reserving).expect("reserve");

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
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        let handle = crate::agent::spawn_ephemeral_worker(&config).expect("spawn");
        let pid = handle.pid();
        assert!(
            crate::process::is_pid_alive(pid),
            "child is alive pre-finalize"
        );

        // finalize_spawn returns Persist (seam armed) → run the production
        // finalize-Err branch verbatim: kill the orphan, drop the handle (NO
        // LIVE_CHILDREN insert), release the stale reserving row.
        finalize_test_seam::set_force_persist_fail(true);
        let fin = finalize_spawn(
            &home,
            &worker_id,
            pid,
            crate::process::process_start_token(pid),
        );
        finalize_test_seam::set_force_persist_fail(false);
        assert!(
            matches!(fin, Err(FinalizeError::Persist(_))),
            "seam must force a persist failure: {fin:?}"
        );
        crate::process::kill_process_tree(pid);
        let _ = handle.child.lock().wait();
        release_reservation(&home, &worker_id);

        assert!(
            !LIVE_CHILDREN.lock().contains_key(&worker_id),
            "fail-closed: no handle leaks into LIVE_CHILDREN"
        );
        assert_eq!(
            list(&home, None)
                .iter()
                .filter(|w| w.status == STATUS_RUNNING)
                .count(),
            0,
            "fail-closed: no running row persists after a finalize persist failure"
        );
        assert_dead_within(pid, "fail-closed: the just-spawned child is orphan-killed");
        std::fs::remove_dir_all(&home).ok();
    }

    // ───────── Fix 3: SpawnKillGuard kills the child on post-spawn failure ────────

    /// §3.10 failure-seam repro: drive the REAL `spawn_ephemeral_worker`, force the
    /// `#[cfg(test)]` post-spawn step to FAIL (after the child spawns + the
    /// `SpawnKillGuard` is armed), and assert the ARMED guard's Drop killed + reaped
    /// the spawned child — zero orphan. Proves the guard is not merely well-shaped
    /// but actually reaps a real running child on the error path.
    /// `#[cfg(unix)]`: ephemeral spawn is fail-closed on Windows.
    #[cfg(unix)]
    #[test]
    fn spawn_ephemeral_worker_armed_guard_kills_child_on_postspawn_failure() {
        // Hold the lock across arm→spawn→read-pid→disarm so the global seam +
        // last-pid observable can't be clobbered by a concurrent spawn test.
        let _seam = seam_lock();
        let worker_id = format!("eph-guard-{}", std::process::id());
        let (program, args) = long_lived_cmd(); // /bin/sleep 30 — a real long-lived child
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
        crate::agent::test_seam::set_force_postspawn_fail(true);
        let res = crate::agent::spawn_ephemeral_worker(&config);
        crate::agent::test_seam::set_force_postspawn_fail(false);

        let err = res.expect_err("forced post-spawn failure must return Err");
        assert!(
            err.to_string().contains("forced post-spawn failure"),
            "the forced failure must surface: {err}"
        );
        let pid = crate::agent::test_seam::last_pid();
        assert_ne!(
            pid, 0,
            "the forced-failure spawn must have recorded a real pid"
        );
        // The armed SpawnKillGuard's Drop ran on the `?`/return → kill_tree + wait.
        assert_dead_within(
            pid,
            "the ARMED SpawnKillGuard must have killed + reaped the spawned child (zero orphan)",
        );
    }

    /// Disarmed-path sanity: with the seam OFF, the guard disarms on the success
    /// path and the worker is alive after spawn (the failure-seam only fires the
    /// kill when armed). Reaps the worker afterward so the test leaves no process.
    #[cfg(unix)]
    #[test]
    fn spawn_ephemeral_worker_disarmed_guard_keeps_child_alive() {
        let _seam = seam_lock();
        let worker_id = format!("eph-guard-ok-{}", std::process::id());
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
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        // seam OFF (default) → success path → guard disarms.
        let handle = crate::agent::spawn_ephemeral_worker(&config).expect("spawn");
        let pid = handle.pid();
        assert!(
            crate::process::is_pid_alive(pid),
            "disarmed guard: the worker is alive after a successful spawn"
        );
        crate::process::kill_process_tree(pid);
        let _ = handle.child.lock().wait();
        drop(handle);
        assert_dead_within(
            pid,
            "cleanup: reap the worker spawned by the disarmed-path test",
        );
    }

    // ─────────────────── PR3b: driver gate + result recording ───────────────────

    /// PR3b driver GATE: a `prompt` for a NON-opencode backend is rejected as
    /// `DriverUnsupported` BEFORE any reserve/spawn (confirm-first iron rule — only
    /// opencode's turn-end detection is smoke-validated in Slice-1). Gated in the
    /// SHARED SINK so a direct caller, not just the MCP handler, is protected. A
    /// prompt-LESS spawn of any backend is unaffected (the PR3a lifecycle path).
    #[test]
    fn spawn_and_track_rejects_prompt_for_non_opencode_backend() {
        let home = tmp_home("driver-gate");
        let spec = SpawnSpec {
            workflow_id: "wf".to_string(),
            backend: "claude".to_string(),
            prompt: "do the thing".to_string(),
            ..Default::default()
        };
        let res = spawn_and_track(&home, spec);
        assert!(
            matches!(&res, Err(SpawnError::DriverUnsupported(b)) if b == "claude"),
            "a prompt for a non-opencode backend must be DriverUnsupported: {res:?}"
        );
        assert_eq!(
            list(&home, None).len(),
            0,
            "the driver gate rejects before reserve — no row, no process"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// PR3b: `mark_prompting` flips phase to `prompting`; `record_result` writes the
    /// outcome and marks the worker terminal (phase + status = `done`) so the next
    /// reap sweep ([`is_due`] on `STATUS_DONE`) terminates it + frees the cap slot.
    #[test]
    fn record_result_marks_done_with_outcome() {
        let home = tmp_home("record-result");
        // Seed a running worker row (bypass the cap gate for a pure store test).
        persist_or_log!(
            crate::store::mutate_versioned(&store_path(&home), |s: &mut EphemeralStore| {
                s.workers.push(EphemeralWorker {
                    worker_id: "w1".to_string(),
                    pid: 1234,
                    spawned_at: chrono::Utc::now().to_rfc3339(),
                    ttl_secs: 100,
                    phase: "spawned".to_string(),
                    status: STATUS_RUNNING.to_string(),
                    ..Default::default()
                });
                Ok(())
            }),
            "seed"
        );
        mark_prompting(&home, "w1");
        assert_eq!(list(&home, None)[0].phase, "prompting");

        record_result(&home, "w1", "the answer".to_string(), true);
        let w = list(&home, None);
        let w = &w[0];
        assert_eq!(w.result_summary.as_deref(), Some("the answer"));
        assert_eq!(w.success, Some(true));
        assert_eq!(w.phase, STATUS_DONE);
        assert_eq!(
            w.status, STATUS_DONE,
            "done status → reap sweep terminates it"
        );
        assert!(
            is_due(w, chrono::Utc::now()),
            "a done worker is due for the next reap sweep"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// REAL opencode end-to-end (`#[ignore]`: no binary/creds in CI). Drives a real
    /// headless opencode worker through `spawn_and_track`'s driver path — inject →
    /// Idle-debounce turn-end → capture → oracle — and asserts the worker records
    /// success with a non-empty transcript. Run locally:
    /// `cargo test --bin agend-terminal --features tray -- --ignored ephemeral_opencode_driver_e2e`.
    /// Uses a FREE model; avoid the `omlx` stub provider (it wedges opencode).
    /// `#[cfg(unix)]`: ephemeral spawn is fail-closed on Windows.
    #[cfg(unix)]
    #[test]
    #[ignore = "requires a real opencode install + auth; run locally"]
    fn ephemeral_opencode_driver_e2e() {
        let home = tmp_home("opencode-e2e");
        let spec = SpawnSpec {
            workflow_id: "e2e".to_string(),
            backend: "opencode".to_string(),
            prompt: "Reply with exactly the word: pong".to_string(),
            model: Some("opencode/deepseek-v4-flash-free".to_string()),
            ttl_secs: Some(180),
            ..Default::default()
        };
        let w = spawn_and_track(&home, spec).expect("spawn opencode worker");
        assert_ne!(w.pid, 0, "a real pid was stamped");

        // The driver runs ASYNC (matches the MCP contract). Poll the row until it
        // records a result, or a generous cap. No reap sweep runs in-test, so the
        // done row persists until we reap it explicitly.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(150);
        let outcome = loop {
            match list(&home, None)
                .into_iter()
                .find(|r| r.worker_id == w.worker_id)
            {
                Some(row) => {
                    if let Some(success) = row.success {
                        break Some((success, row.result_summary));
                    }
                }
                None => break None, // unexpectedly reaped
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the driver did not record a result within the deadline"
            );
            std::thread::sleep(std::time::Duration::from_millis(500));
        };
        reap_one(&home, &w.worker_id); // terminate the worker process regardless

        let (success, summary) = outcome.expect("the worker row must carry a result before reap");
        assert!(
            success,
            "a simple prompt turn must succeed; summary={summary:?}"
        );
        assert!(
            summary.map(|s| !s.trim().is_empty()).unwrap_or(false),
            "result_summary must be non-empty on success"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
