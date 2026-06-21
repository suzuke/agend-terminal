//! #1967 Phase-1: ephemeral worker tracking (PR1 scaffold).
//!
//! Short-lived, cross-backend "ephemeral" workers live OUTSIDE the managed-agent
//! bookkeeping — they are NOT inserted into the agent registry, fleet.yaml,
//! binding, or the worktree pool. They are tracked only in a small JSON sidecar
//! (`$AGEND_HOME/ephemeral_workers.json`, mirroring [`crate::dispatch_tracking`])
//! plus an in-memory `Child` handle map for zombie reaping.
//!
//! PR1 scope = lifecycle + tracking + reap + day-1 cost guards ONLY. There is NO
//! real backend transport yet: [`spawn_and_track`] launches a FAKE child
//! (`/bin/sleep`) so the spawn→track→reap lifecycle, the cost guards, and the
//! reap's real process termination are exercised against a genuine OS process.
//! The headless protocol transport (ACP) is PR2/PR3. See
//! `docs/design/1967-ephemeral-phase1.md`.
//!
//! ## Cost guards (lead vet condition — day-1, before real spawn)
//! - **Hard max-live concurrency cap** ([`MAX_LIVE_WORKERS`]): admission is an
//!   atomic check-and-add under the store flock ([`try_track_within_cap`]), so the
//!   cap can never be exceeded even under concurrent spawns.
//! - **Max-wall-TTL** ([`DEFAULT_WALL_TTL_SECS`], clamped to [`MAX_WALL_TTL_SECS`]):
//!   the reap sweep terminates any worker whose wall clock exceeds its `ttl_secs`,
//!   so a wedged/forgotten worker cannot run (and bill) unbounded.

use crate::store::SchemaVersioned;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

/// Cost-guard: hard ceiling on simultaneously-live ephemeral workers. Spawn
/// admission rejects once the live count would exceed this. Conservative day-1
/// value against the "20 workers burning money overnight" fear — a deterministic
/// backstop in place BEFORE real spawn (PR2) and token metering (PR6).
pub const MAX_LIVE_WORKERS: usize = 8;

/// Cost-guard: default max wall-clock TTL for a worker (seconds). 30 min —
/// generous for a bounded sub-task, tight enough to cap runaway spend.
pub const DEFAULT_WALL_TTL_SECS: u64 = 30 * 60;

/// Cost-guard: absolute ceiling a per-spawn `ttl_secs` is clamped to, so a caller
/// cannot disable the wall-TTL guard by requesting an enormous value.
pub const MAX_WALL_TTL_SECS: u64 = 2 * 60 * 60;

/// PR1 fake-child command — a real OS process so reap/TTL enforcement is genuine.
/// Replaced by the headless backend transport in PR2/PR3.
const FAKE_CHILD_CMD: &str = "/bin/sleep";

/// One tracked ephemeral worker. Persisted as a JSON row; the live `Child` handle
/// (for zombie reaping) is held separately in [`LIVE_CHILDREN`].
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
    pub phase: String,  // "spawned" | "running" | "done"
    pub status: String, // "running" | "done"
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

/// In-memory map of `worker_id → Child` for the PR1 fake children, so reap can
/// `wait()` the terminated child and never leak a zombie. Empty after a daemon
/// restart (handles don't survive) — restart orphans are handled by
/// [`reap_on_boot`] via pid + start-token instead. PR2's real workers go through
/// the agent spawn machinery's own reaping.
static LIVE_CHILDREN: LazyLock<Mutex<HashMap<String, Child>>> =
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
    /// The fake child failed to launch.
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

/// Atomic admission: add `worker` only if the live count is below
/// [`MAX_LIVE_WORKERS`]. The check-and-add happens in ONE locked read-modify-write
/// (the store flock), so concurrent spawns can never push the count past the cap.
/// Returns `Err(live)` (the count seen) when the cap is full.
fn try_track_within_cap(home: &Path, worker: EphemeralWorker) -> Result<(), usize> {
    let mut rejected_at = None;
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |s: &mut EphemeralStore| {
            if s.workers.len() >= MAX_LIVE_WORKERS {
                rejected_at = Some(s.workers.len());
                return Ok(());
            }
            s.workers.push(worker.clone());
            Ok(())
        }),
        "ephemeral_track"
    );
    match rejected_at {
        Some(live) => Err(live),
        None => Ok(()),
    }
}

/// Spawn a worker (PR1: a FAKE `/bin/sleep` child) and track it, enforcing the
/// hard max-live cap. On cap-exceeded the just-spawned child is terminated so no
/// process leaks. Returns the tracked [`EphemeralWorker`].
pub fn spawn_and_track(home: &Path, spec: SpawnSpec) -> Result<EphemeralWorker, SpawnError> {
    let ttl_secs = resolve_ttl(spec.ttl_secs);
    let child = spawn_fake_child(ttl_secs).map_err(SpawnError::Spawn)?;
    let pid = child.id();
    let seq = WORKER_SEQ.fetch_add(1, Ordering::Relaxed);
    let worker = EphemeralWorker {
        worker_id: format!("eph-{}-{}", std::process::id(), seq),
        workflow_id: spec.workflow_id,
        parent: spec.parent,
        backend: spec.backend,
        pid,
        process_start_token: crate::process::process_start_token(pid),
        spawned_at: chrono::Utc::now().to_rfc3339(),
        ttl_secs,
        token_budget: spec.token_budget,
        phase: "spawned".to_string(),
        status: "running".to_string(),
    };

    match try_track_within_cap(home, worker.clone()) {
        Ok(()) => {
            LIVE_CHILDREN.lock().insert(worker.worker_id.clone(), child);
            Ok(worker)
        }
        Err(live) => {
            // Admission rejected after the spawn — terminate the orphan so the cap
            // is honoured with zero leaked process.
            let mut c = child;
            crate::process::terminate(pid);
            let _ = c.wait();
            Err(SpawnError::CapExceeded {
                live,
                cap: MAX_LIVE_WORKERS,
            })
        }
    }
}

/// PR1 fake child: a real `/bin/sleep` outliving its own TTL (so the reap-by-TTL
/// path — not the child's natural exit — does the terminating). Detached stdio.
fn spawn_fake_child(ttl_secs: u64) -> std::io::Result<Child> {
    use std::process::{Command, Stdio};
    Command::new(FAKE_CHILD_CMD)
        .arg(ttl_secs.saturating_add(60).to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// Pure reap decision: is `w` due to be reaped at `now`? — terminal status, or its
/// max-wall-TTL has elapsed. (Pid-liveness is handled separately by the sweep.)
fn is_due(w: &EphemeralWorker, now: chrono::DateTime<chrono::Utc>) -> bool {
    if w.status == "done" {
        return true;
    }
    match chrono::DateTime::parse_from_rfc3339(&w.spawned_at) {
        Ok(t) => (now - t.with_timezone(&chrono::Utc)).num_seconds() >= w.ttl_secs as i64,
        Err(_) => true, // unparseable timestamp → corrupt row, don't keep forever
    }
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
                if is_due(&w, now) || !crate::process::is_pid_alive(w.pid) {
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

/// Terminate a reaped worker's process and reap its zombie.
///
/// If we still hold the live `Child` handle (spawned this daemon life): SIGTERM
/// then `wait()` to collect the exit (no zombie). Otherwise (e.g. a restart
/// orphan): a start-token-guarded SIGTERM only — a recycled pid is never killed,
/// and init reaps the orphan's zombie. PR1 sends a single SIGTERM; the
/// process-GROUP kill + protocol graceful-cancel-before-kill are PR2/PR3.
fn terminate_worker(w: &EphemeralWorker) {
    if let Some(mut child) = LIVE_CHILDREN.lock().remove(&w.worker_id) {
        crate::process::terminate(w.pid);
        let _ = child.wait();
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
    crate::process::terminate(w.pid);
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

    // Only the #[cfg(unix)] real-fake-child tests build a SpawnSpec; gate the
    // helper so Windows clippy (-D warnings) doesn't flag it as dead code.
    #[cfg(unix)]
    fn spec(workflow_id: &str, ttl_secs: Option<u64>) -> SpawnSpec {
        SpawnSpec {
            workflow_id: workflow_id.to_string(),
            parent: None,
            backend: "opencode".to_string(),
            ttl_secs,
            token_budget: None,
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
        for i in 0..MAX_LIVE_WORKERS {
            let w = EphemeralWorker {
                worker_id: format!("w{i}"),
                pid: 0,
                spawned_at: chrono::Utc::now().to_rfc3339(),
                ttl_secs: 100,
                status: "running".to_string(),
                ..Default::default()
            };
            assert!(
                try_track_within_cap(&home, w).is_ok(),
                "under cap must accept"
            );
        }
        assert_eq!(list(&home, None).len(), MAX_LIVE_WORKERS);
        let over = EphemeralWorker {
            worker_id: "over".to_string(),
            pid: 0,
            spawned_at: chrono::Utc::now().to_rfc3339(),
            ttl_secs: 100,
            status: "running".to_string(),
            ..Default::default()
        };
        let res = try_track_within_cap(&home, over);
        assert_eq!(res, Err(MAX_LIVE_WORKERS), "at cap must reject");
        assert_eq!(
            list(&home, None).len(),
            MAX_LIVE_WORKERS,
            "rejected worker must NOT be added"
        );
        std::fs::remove_dir_all(&home).ok();
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

    /// #[cfg(unix)] full lifecycle against a REAL fake child: spawn → list → reap,
    /// and the reap genuinely terminates the process (is_pid_alive → false).
    #[cfg(unix)]
    #[test]
    fn fake_child_lifecycle_spawn_list_reap() {
        let home = tmp_home("lifecycle");
        let w = spawn_and_track(&home, spec("wf1", Some(3600))).expect("spawn");
        assert_eq!(
            list(&home, Some("wf1")).len(),
            1,
            "spawned worker is listed"
        );
        assert!(
            crate::process::is_pid_alive(w.pid),
            "fake child is alive after spawn"
        );

        let reaped = reap_one(&home, &w.worker_id).expect("reap_one returns the worker");
        assert_eq!(reaped.worker_id, w.worker_id);
        assert_eq!(
            list(&home, None).len(),
            0,
            "reaped worker removed from store"
        );
        // The child was SIGTERM'd + waited → dead.
        assert!(
            !crate::process::is_pid_alive(w.pid),
            "reap must terminate the fake child"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #[cfg(unix)] the reap SWEEP enforces max-wall-TTL: a real child spawned with
    /// a 0s TTL (already expired) is terminated + removed on the next sweep.
    #[cfg(unix)]
    #[test]
    fn reap_sweep_enforces_wall_ttl() {
        let home = tmp_home("ttl-sweep");
        // ttl=0 → resolve_ttl bumps to DEFAULT; instead spawn with a tiny ttl and
        // drive the sweep with a `now` past it (deterministic, no real waiting).
        let w = spawn_and_track(&home, spec("wf1", Some(60))).expect("spawn");
        assert!(crate::process::is_pid_alive(w.pid));
        // Sweep at now+61s → the worker is past its 60s wall-TTL → reaped.
        let future = chrono::Utc::now() + chrono::Duration::seconds(61);
        let reaped = reap_sweep_at(&home, future);
        assert_eq!(reaped.len(), 1, "the TTL-expired worker must be reaped");
        assert_eq!(list(&home, None).len(), 0);
        assert!(
            !crate::process::is_pid_alive(w.pid),
            "TTL reap must terminate the child"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
