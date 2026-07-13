//! #2764 R9/R10: durable child-lifecycle ledger — `runtime/child-pids/<uuid>.json`.
//!
//! Registry absence alone is NOT lifecycle proof: a lost handle (daemon
//! restart, registry bug) leaves a live child that a pinned delete would then
//! vacuously convert into "terminally absent" destructive authority.
//!
//! R10 typed, fail-closed contract:
//! - [`record_running`] is written ATOMICALLY (tmp + rename) BEFORE the handle
//!   is exposed in the registry, and a write failure ABORTS the spawn — so
//!   `Absent` is a provable durable negative (the generation was never
//!   exposed), not an inference from a missing best-effort file.
//! - Terminal is EXPLICIT: [`record_exited`] rewrites the entry to the exited
//!   state (proven exit only: reaper-observed, stop-transaction-observed, or
//!   OS-probe ESRCH). File absence is never used as terminal evidence for a
//!   generation that has a `Running` record.
//! - [`lookup`] is typed: `Running{pid}` / `Exited` / `Absent` / `Corrupt`.
//!   `Corrupt` (unreadable/malformed) is UNKNOWN — the stop path fails closed
//!   on it, never treating it as terminal.
//! - Never kill an unverified recorded pid: a reused pid would hit an innocent
//!   process; UNKNOWN keeps the retry loud instead.

use crate::types::InstanceId;
use std::path::{Path, PathBuf};

fn dir(home: &Path) -> PathBuf {
    home.join("runtime").join("child-pids")
}

fn path(home: &Path, id: &InstanceId) -> PathBuf {
    dir(home).join(format!("{}.json", id.full()))
}

/// Typed lifecycle evidence for one exact generation.
#[derive(Debug, PartialEq, Eq)]
pub enum LedgerState {
    /// A spawn for this generation was exposed with this child pid and no
    /// terminal proof has been recorded yet.
    Running { pid: u32 },
    /// Terminal absence PROVEN (observed exit or OS-probe ESRCH).
    Exited,
    /// No record — with the Running-before-exposure invariant this is a
    /// durable negative: the generation was never exposed in the registry.
    Absent,
    /// Unreadable/malformed record — UNKNOWN; callers must fail closed.
    Corrupt,
}

fn write_atomic(home: &Path, id: &InstanceId, body: &str) -> Result<(), String> {
    let d = dir(home);
    std::fs::create_dir_all(&d).map_err(|e| format!("create {}: {e}", d.display()))?;
    let final_path = path(home, id);
    let tmp = d.join(format!(".{}.tmp", id.full()));
    std::fs::write(&tmp, body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &final_path)
        .map_err(|e| format!("rename {} → {}: {e}", tmp.display(), final_path.display()))
}

/// Record the spawned child's pid for `id` ATOMICALLY. #2764 R10: callers MUST
/// abort the spawn (kill the child, no registry exposure) on `Err` — a
/// best-effort write here would make `Absent` fail-open.
pub fn record_running(home: &Path, id: &InstanceId, pid: u32) -> Result<(), String> {
    write_atomic(
        home,
        id,
        &serde_json::json!({ "state": "running", "pid": pid }).to_string(),
    )
}

/// Record PROVEN terminal absence for `id`. Failure is loud (the exit itself
/// cannot be undone, but a stale `Running` record must not linger silently).
pub fn record_exited(home: &Path, id: &InstanceId) {
    if let Err(e) = write_atomic(
        home,
        id,
        &serde_json::json!({ "state": "exited" }).to_string(),
    ) {
        tracing::error!(id = %id.full(), error = %e,
            "#2764 child ledger: FAILED to record proven exit — a later pinned stop will fail closed on the stale Running entry");
    }
}

/// #2764 R11: erase the ledger record for one EXACT generation. ONLY sound
/// past a proven stop disposition + a Clean `full_delete` destructive commit —
/// there the WHOLE generation is erased, so dropping its terminal record
/// cannot create a fail-open (`Absent` stays a durable negative via the
/// Running-before-exposure invariant). Never call this on an ambiguous or
/// failed delete: the record is the retry's lifecycle evidence.
pub fn purge(home: &Path, id: &InstanceId) {
    let _ = std::fs::remove_file(path(home, id));
}

/// Whether ANY ledger record (running/exited/corrupt) exists for `id` — the
/// residual-audit mirror of [`purge`] (file-level, state-agnostic).
pub fn record_exists(home: &Path, id: &InstanceId) -> bool {
    path(home, id).exists()
}

/// Typed lookup — see [`LedgerState`].
pub fn lookup(home: &Path, id: &InstanceId) -> LedgerState {
    let p = path(home, id);
    let raw = match std::fs::read_to_string(&p) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LedgerState::Absent,
        Err(_) => return LedgerState::Corrupt,
    };
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return LedgerState::Corrupt,
    };
    match v.get("state").and_then(|s| s.as_str()) {
        Some("exited") => LedgerState::Exited,
        Some("running") => match v.get("pid").and_then(|p| p.as_u64()) {
            Some(pid) => LedgerState::Running { pid: pid as u32 },
            None => LedgerState::Corrupt,
        },
        // Legacy R9 shape ({"pid": N} with no state) or anything else:
        // UNKNOWN, fail closed.
        _ => LedgerState::Corrupt,
    }
}

/// OS liveness probe: `kill(pid, 0)`. `ESRCH` → provably gone; anything else
/// (delivered, `EPERM`, unexpected errno) → treat as alive (fail closed).
#[cfg(unix)]
pub fn os_pid_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
pub fn os_pid_alive(_pid: u32) -> bool {
    // No portable probe — fail closed (UNKNOWN reads as alive).
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "agend-ledger-{}-{}-{}",
            std::process::id(),
            tag,
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).ok();
        d
    }

    #[test]
    fn typed_round_trip_running_exited_absent() {
        let home = tmp("roundtrip");
        let id = InstanceId::new();
        assert_eq!(lookup(&home, &id), LedgerState::Absent);
        record_running(&home, &id, 4242).expect("record running");
        assert_eq!(lookup(&home, &id), LedgerState::Running { pid: 4242 });
        record_exited(&home, &id);
        assert_eq!(lookup(&home, &id), LedgerState::Exited);
        std::fs::remove_dir_all(&home).ok();
    }

    /// Malformed / legacy-shape records are UNKNOWN (Corrupt), never terminal.
    #[test]
    fn malformed_and_legacy_records_are_corrupt() {
        let home = tmp("corrupt");
        let id = InstanceId::new();
        std::fs::create_dir_all(super::dir(&home)).unwrap();
        std::fs::write(super::path(&home, &id), "not json").unwrap();
        assert_eq!(lookup(&home, &id), LedgerState::Corrupt);
        // Legacy R9 shape: pid without a typed state.
        std::fs::write(super::path(&home, &id), r#"{"pid": 99}"#).unwrap();
        assert_eq!(lookup(&home, &id), LedgerState::Corrupt);
        std::fs::remove_dir_all(&home).ok();
    }

    /// A write failure surfaces as Err (spawn must bail) — forced by a FILE
    /// squatting on the ledger DIRECTORY path.
    #[test]
    fn record_running_write_failure_is_err() {
        let home = tmp("writefail");
        std::fs::create_dir_all(home.join("runtime")).unwrap();
        std::fs::write(home.join("runtime").join("child-pids"), "squatter").unwrap();
        let id = InstanceId::new();
        assert!(
            record_running(&home, &id, 1).is_err(),
            "a blocked ledger write must be an Err so the spawn bails"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
