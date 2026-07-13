//! #2764 R9: durable child-lifecycle ledger — `runtime/child-pids/<uuid>.json`.
//!
//! Registry absence alone is NOT lifecycle proof: a lost handle (daemon
//! restart, registry bug) leaves a live child that a pinned delete would then
//! vacuously convert into "terminally absent" destructive authority. The
//! ledger is written at spawn registration (managed agents only) and cleared
//! only on PROVEN terminal exit; the pinned stop path consults it plus an OS
//! liveness probe before treating a missing registry handle as terminal:
//! - no ledger entry → durable negative (never spawned / already proven
//!   exited) → terminal absence proven;
//! - ledger entry + OS says the pid is gone → terminal; ledger cleared;
//! - ledger entry + pid alive (or probe inconclusive, incl. pid reuse —
//!   fail-closed either way) → UNKNOWN: the stop reports false and nothing
//!   name-keyed is released. Never kill an unverified recorded pid — a reused
//!   pid would hit an innocent process; the retained ledger keeps the retry
//!   loud instead.

use crate::types::InstanceId;
use std::path::{Path, PathBuf};

fn dir(home: &Path) -> PathBuf {
    home.join("runtime").join("child-pids")
}

fn path(home: &Path, id: &InstanceId) -> PathBuf {
    dir(home).join(format!("{}.json", id.full()))
}

/// Record the spawned child's pid for `id`. Best-effort (a failed write only
/// costs fail-closed strictness later, never destructive laxity).
pub fn record(home: &Path, id: &InstanceId, pid: u32) {
    if std::fs::create_dir_all(dir(home)).is_ok() {
        let _ = std::fs::write(
            path(home, id),
            serde_json::json!({ "pid": pid }).to_string(),
        );
    }
}

/// Clear the ledger entry after PROVEN terminal exit.
pub fn clear(home: &Path, id: &InstanceId) {
    let _ = std::fs::remove_file(path(home, id));
}

/// The recorded pid for `id`, if a spawn was recorded and not yet proven exited.
pub fn lookup(home: &Path, id: &InstanceId) -> Option<u32> {
    let raw = std::fs::read_to_string(path(home, id)).ok()?;
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()?
        .get("pid")?
        .as_u64()
        .map(|p| p as u32)
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
