//! #851 restart-supervisor detection.
//!
//! `restart_daemon` MCP triggers a graceful shutdown + `exit(42)`. The
//! exit code is meaningful ONLY when something supervises the daemon
//! process and respawns on exit (launchd's `KeepAlive`, systemd's
//! `Restart=on-failure`, Windows Task Scheduler, or the bash wrapper
//! script at `scripts/agend-wrapper.sh`). When the daemon was started
//! bare (`agend-terminal start` from a shell, no supervisor in the
//! parent chain), `exit(42)` just kills the process — operators then
//! see `Resource temporarily unavailable (os error 35)` on every
//! subsequent MCP call until they manually restart.
//!
//! Today's incident (2026-05-16): general assumed `restart_daemon`
//! succeeded post-#849 ship; all MCP calls hung for ~5 min until
//! operator manually intervened. The handler returned `ok: true` and
//! the daemon DID exit cleanly with 42, but nothing respawned.
//!
//! Fix: detect-and-fail-closed. The handler queries
//! [`is_restart_supervised`] before setting `RESTART_PENDING`. If no
//! supervisor signal is found, the handler returns
//! `{ok: false, error: "..."}` with an actionable hint and the daemon
//! stays up.
//!
//! ## Detection signals
//!
//! Composite env-var check covering all four supervised invocation
//! paths plus an explicit marker for the bash wrapper:
//!
//! - `AGEND_WRAPPED=1` — set by `scripts/agend-wrapper.sh` before each
//!   daemon invocation. Marker for the manual / dev-mode supervisor.
//! - `XPC_SERVICE_NAME` — set by macOS launchd for every service it
//!   spawns. Reliable indicator that the daemon is running under
//!   launchd with a `KeepAlive` policy.
//! - `INVOCATION_ID` — set by systemd for every unit instance.
//!   Reliable indicator that the daemon is running under a systemd
//!   user unit with `Restart=on-failure`.
//!
//! Windows Task Scheduler does not set a comparably reliable env var.
//! r0 fails-closed on Windows bare-start (the correct behavior — an
//! operator using bare `agend-terminal start` on Windows gets the
//! same actionable error as on macOS/Linux). Adding explicit
//! Windows-side detection (parent-PID check against `taskhostw.exe`
//! / `svchost.exe`) is a deferred follow-up.

/// True iff the daemon's parent process chain includes a supervisor
/// that will respawn the daemon on `exit(42)`. Returns false (fail-
/// closed) when no supervisor is detected — `restart_daemon` should
/// then refuse the request rather than silently killing the daemon.
///
/// Composite env-var check covering all four supervised invocation
/// paths. See module doc for the rationale + Windows fail-closed
/// note. Pure helper — no globals, no side effects, safe to call from
/// any thread.
pub fn is_restart_supervised() -> bool {
    has_env("AGEND_WRAPPED") || has_env("XPC_SERVICE_NAME") || has_env("INVOCATION_ID")
}

/// Returns true iff env var `name` is set (any value, including
/// empty string — presence is the signal). Distinct from
/// `std::env::var(name).is_ok()` because we don't care about UTF-8
/// validity for the indicator role.
fn has_env(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// Tests touch process-global env state — serialise via a
    /// function-scoped mutex so parallel test threads don't race.
    /// Mirror of the `with_f9_gate` pattern in `src/health.rs`.
    fn with_env<R>(set: &[(&str, &str)], unset: &[&str], f: impl FnOnce() -> R) -> R {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());

        let all_keys: Vec<&str> = set
            .iter()
            .map(|(k, _)| *k)
            .chain(unset.iter().copied())
            .collect();
        let prior: Vec<(String, Option<String>)> = all_keys
            .iter()
            .map(|k| (k.to_string(), std::env::var(k).ok()))
            .collect();

        // SAFETY: test-only mutation, serialised via the mutex above.
        // Rust 1.84+ requires unsafe for env mutations; older toolchains
        // treat the unsafe block as a no-op.
        unsafe {
            for k in unset {
                std::env::remove_var(k);
            }
            for (k, v) in set {
                std::env::set_var(k, v);
            }
        }

        let result = f();

        unsafe {
            for (k, v) in &prior {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }

        result
    }

    const ALL_SIGNAL_VARS: &[&str] = &["AGEND_WRAPPED", "XPC_SERVICE_NAME", "INVOCATION_ID"];

    /// Bare `agend-terminal start` — no supervisor in parent chain,
    /// none of the signal env vars set. Detection must fail-closed.
    #[test]
    fn detect_returns_false_when_no_supervisor_env_set() {
        with_env(&[], ALL_SIGNAL_VARS, || {
            assert!(
                !is_restart_supervised(),
                "bare daemon invocation (no signal env) must fail-closed"
            );
        });
    }

    /// `scripts/agend-wrapper.sh` sets `AGEND_WRAPPED=1` before each
    /// daemon invocation (added by this PR's C2c). Detection must
    /// recognize the explicit marker.
    #[test]
    fn detect_returns_true_when_agend_wrapped_set() {
        with_env(
            &[("AGEND_WRAPPED", "1")],
            &["XPC_SERVICE_NAME", "INVOCATION_ID"],
            || {
                assert!(
                    is_restart_supervised(),
                    "AGEND_WRAPPED=1 must be recognized as a supervisor signal"
                );
            },
        );
    }

    /// macOS launchd sets `XPC_SERVICE_NAME` for every service it
    /// spawns. Used by the `service install` plist that ships
    /// `KeepAlive=true`.
    #[test]
    fn detect_returns_true_when_launchd_xpc_service_name_set() {
        with_env(
            &[("XPC_SERVICE_NAME", "com.agend-terminal.daemon")],
            &["AGEND_WRAPPED", "INVOCATION_ID"],
            || {
                assert!(
                    is_restart_supervised(),
                    "launchd XPC_SERVICE_NAME must be recognized as a supervisor signal"
                );
            },
        );
    }

    /// Linux systemd sets `INVOCATION_ID` for every unit instance.
    /// Used by the `service install` systemd user unit that ships
    /// `Restart=on-failure`.
    #[test]
    fn detect_returns_true_when_systemd_invocation_id_set() {
        with_env(
            &[("INVOCATION_ID", "abc123def456")],
            &["AGEND_WRAPPED", "XPC_SERVICE_NAME"],
            || {
                assert!(
                    is_restart_supervised(),
                    "systemd INVOCATION_ID must be recognized as a supervisor signal"
                );
            },
        );
    }
}
