//! #1967 Phase-1: ephemeral worker REAP + tracking (spawn path decommissioned).
//!
//! Short-lived, cross-backend "ephemeral" workers live OUTSIDE the managed-agent
//! bookkeeping — they are NOT inserted into the agent registry, fleet.yaml,
//! binding, or the worktree pool. They are tracked only in a small JSON sidecar
//! (`$AGEND_HOME/ephemeral_workers.json`, mirroring [`crate::dispatch_tracking`])
//! plus an in-memory [`crate::agent::EphemeralPtyHandle`] map for zombie reaping.
//!
//! The #1967 Phase-1 SPAWN + Route-B driver path (PR #2401-#2408) was
//! DECOMMISSIONED here: its MCP entry point was removed in #2558, leaving ~2300
//! LOC dead behind `#![allow(dead_code)]`. It is archived at git tag
//! `archive/1967-ephemeral-phase1` (checkout to revive) rather than carried as
//! unmaintained dead weight. What REMAINS — the only part ever reachable from
//! production `main()` — is worker REAPING: [`reap_sweep`] terminates any tracked
//! worker whose wall clock exceeds its per-worker `ttl_secs` cost guard (or is
//! terminal / whose process is gone), [`reap_on_boot`] clears leftover rows at
//! startup, and [`live_children_snapshot`] exposes the live set to the shadow
//! observer.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

/// Cost-guard / liveness: a worker stuck in the `"reserving"` state (admission
/// slot taken but `finalize` never ran — daemon crashed between reserve and
/// finalize) longer than this is reaped as stale. Conservative: WELL above the
/// worst-case spawn+finalize latency (milliseconds) so the reap never races an
/// in-flight finalize.
const RESERVE_STALE_SECS: i64 = 60;

/// Worker status: admission slot taken, real process not yet spawned/finalized.
const STATUS_RESERVING: &str = "reserving";
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

/// #2524 P3a PR-1: one live ephemeral worker, as exposed to the shadow-observer's
/// observation plane (`daemon::per_tick::shadow_observe` + `daemon::shadow::rollout`).
/// READ-ONLY — this is an observation feed, not a registration: it doesn't add the
/// worker to `AgentRegistry`/`fleet.yaml`/binding, so it doesn't reintroduce the
/// "managed bookkeeping" #1967 deliberately avoids for ephemeral workers.
pub(crate) struct LiveEphemeralSnapshot {
    pub worker_id: String,
    pub core: Arc<crate::sync_audit::CoreMutex<crate::agent::AgentCore>>,
    pub child_alive: bool,
    pub backend: Option<crate::backend::Backend>,
    pub cwd: Option<PathBuf>,
}

/// Snapshot all live ephemeral workers under ONE brief `LIVE_CHILDREN` lock,
/// released before this returns — so a caller (e.g. `shadow_observe`'s per-tick
/// handler) never holds this lock nested with any other (the registry lock in
/// particular; see that caller's own lock-ordering note).
pub(crate) fn live_children_snapshot() -> Vec<LiveEphemeralSnapshot> {
    LIVE_CHILDREN
        .lock()
        .iter()
        .map(|(worker_id, h)| LiveEphemeralSnapshot {
            worker_id: worker_id.clone(),
            core: Arc::clone(&h.core),
            child_alive: h.child.lock().process_id().is_some(),
            backend: h.backend.clone(),
            cwd: h.cwd.clone(),
        })
        .collect()
}

fn store_path(home: &Path) -> PathBuf {
    crate::store::store_path(home, "ephemeral_workers.json")
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

    fn tmp_home(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("agend-ephem-reap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A store row with `pid: 0` — [`is_pid_alive`] treats 0 as dead and
    /// [`terminate_worker`] returns early on it, so these fixtures reap WITHOUT
    /// touching any real process (spawn-free; the real-spawn kill path lived in the
    /// decommissioned #1967 spawn suite — archived at `archive/1967-ephemeral-phase1`).
    fn row(id: &str, status: &str, age_secs: i64, ttl: u64) -> EphemeralWorker {
        EphemeralWorker {
            worker_id: id.to_string(),
            pid: 0,
            spawned_at: (chrono::Utc::now() - chrono::Duration::seconds(age_secs)).to_rfc3339(),
            ttl_secs: ttl,
            status: status.to_string(),
            ..Default::default()
        }
    }

    fn seed(home: &Path, workers: Vec<EphemeralWorker>) {
        crate::store::mutate_versioned(&store_path(home), |s: &mut EphemeralStore| {
            s.workers = workers;
            Ok(())
        })
        .expect("seed store");
    }

    fn load_workers(home: &Path) -> Vec<EphemeralWorker> {
        use crate::store::SchemaVersioned;
        crate::store::load_versioned::<EphemeralStore>(&store_path(home), EphemeralStore::CURRENT)
            .workers
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

    /// A reserving row is judged ONLY by the reserve-stale window (never pid), so a
    /// fresh reserve (spawn/finalize in flight) is not mistaken for dead.
    #[test]
    fn is_stale_reserving_only_past_reserve_window() {
        let now = chrono::Utc::now();
        assert!(!is_stale_reserving(
            &row("f", STATUS_RESERVING, 5, 100),
            now
        ));
        assert!(is_stale_reserving(
            &row("s", STATUS_RESERVING, RESERVE_STALE_SECS + 5, 100),
            now
        ));
        // a running row is never "stale reserving", however old.
        assert!(!is_stale_reserving(
            &row("r", "running", RESERVE_STALE_SECS + 100, 100),
            now
        ));
    }

    /// Boot sweep clears EVERY tracked row (all predate this boot) and counts them;
    /// idempotent on an empty store.
    #[test]
    fn reap_on_boot_clears_all_rows_and_counts() {
        let home = tmp_home("boot");
        seed(
            &home,
            vec![
                row("a", "running", 5, 100),
                row("b", STATUS_RESERVING, 5, 100),
            ],
        );
        assert_eq!(reap_on_boot(&home), 2, "clears every pre-boot orphan");
        assert!(load_workers(&home).is_empty(), "store emptied");
        assert_eq!(reap_on_boot(&home), 0, "idempotent on an empty store");
    }

    /// The sweep reaps a terminal (`done`) row regardless of pid/time, and reaps a
    /// STALE reserving row while KEEPING a fresh reserving one (reserving rows are
    /// gated on the stale window, not pid-liveness).
    #[test]
    fn reap_sweep_reaps_terminal_and_stale_reserving_keeps_fresh_reserving() {
        let home = tmp_home("sweep");
        let now = chrono::Utc::now();
        seed(
            &home,
            vec![
                row("done", STATUS_DONE, 5, 100), // terminal → reaped
                row("stale", STATUS_RESERVING, RESERVE_STALE_SECS + 5, 100), // stale → reaped
                row("fresh", STATUS_RESERVING, 5, 100), // in-flight → kept
            ],
        );
        let mut reaped: Vec<String> = reap_sweep_at(&home, now)
            .into_iter()
            .map(|w| w.worker_id)
            .collect();
        reaped.sort();
        assert_eq!(reaped, vec!["done".to_string(), "stale".to_string()]);
        let kept = load_workers(&home);
        assert_eq!(kept.len(), 1);
        assert_eq!(
            kept[0].worker_id, "fresh",
            "a fresh reserving row is not reaped"
        );
    }
}
