//! Filesystem layout for supervised installs.
//!
//! ```text
//! $AGEND_HOME/
//!   bin/
//!     current          → symlink to the active daemon binary
//!     prev             → symlink to the previous binary (rollback target)
//!     store/
//!       <sha256>       ← content-addressed binaries staged by `upgrade`
//!     supervisor       ← the supervisor binary itself (also symlink in
//!                        practice, but kept separate so daemon upgrades
//!                        never touch it)
//!   run/
//!     <pid>/           ← per-daemon run dir (owned by daemon, see daemon::run_dir)
//!     supervisor.sock  ← supervisor IPC socket (unix)
//!     supervisor.pid   ← supervisor's PID (written on startup)
//!     upgrade-marker   ← written by supervisor pre-upgrade, read by new
//!                        daemon to emit the "daemon upgraded" system message
//! ```
//!
//! All paths are derived from `$AGEND_HOME` (or the user-home fallback); this
//! module never touches the environment itself — callers pass `home` in so
//! tests can use temp dirs.

use std::path::{Path, PathBuf};

pub fn bin_dir(home: &Path) -> PathBuf {
    home.join("bin")
}

pub fn bin_store_dir(home: &Path) -> PathBuf {
    bin_dir(home).join("store")
}

/// Symlink pointing to the currently-active daemon binary.
/// Swapping this symlink (atomic via rename) is the moment of "upgrade".
pub fn current_link(home: &Path) -> PathBuf {
    bin_dir(home).join("current")
}

/// Symlink pointing to the previously-active daemon binary. Target of
/// rollback. Populated by `upgrade` before swapping `current`.
pub fn prev_link(home: &Path) -> PathBuf {
    bin_dir(home).join("prev")
}

/// Supervisor binary path. Kept as a symlink to the installed binary so the
/// supervisor can self-upgrade in the future without touching the daemon
/// layout. For now, the supervisor never self-upgrades — just a convention.
pub fn supervisor_link(home: &Path) -> PathBuf {
    bin_dir(home).join("supervisor")
}

/// Per-daemon run dir root (shared with `daemon::run_dir`; we only use the
/// directory, not the PID-scoped subdir).
pub fn run_root(home: &Path) -> PathBuf {
    home.join("run")
}

/// Unix domain socket the supervisor listens on. Windows installs stub this
/// out; upgrade on Windows is rejected at CLI parse time.
pub fn supervisor_sock(home: &Path) -> PathBuf {
    run_root(home).join("supervisor.sock")
}

/// Supervisor PID file — written on startup, used by the CLI to check
/// whether a supervisor is alive without opening the socket.
pub fn supervisor_pid_file(home: &Path) -> PathBuf {
    run_root(home).join("supervisor.pid")
}

/// Upgrade marker file — written by supervisor before launching the new
/// daemon. Carries `from_version`/`to_version`. The freshly-started daemon
/// reads (and consumes) this on boot and uses it to tailor the system
/// message it injects into respawned agents.
pub fn upgrade_marker(home: &Path) -> PathBuf {
    run_root(home).join("upgrade-marker")
}

/// Path inside `bin/store/` for a binary addressed by its hex sha256.
pub fn stored_binary(home: &Path, hash: &str) -> PathBuf {
    bin_store_dir(home).join(hash)
}
