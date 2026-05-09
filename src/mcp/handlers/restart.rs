//! Sprint 60 W1 PR-3 (#P0-3 operator restart MCP tool) — Wave 1
//! closeout. Closes the operator-restart-required SPOF that drove
//! the Sprint 59 PR-4 (P3) abandon path: chicken-and-egg requiring
//! a daemon restart with no programmatic alternative meant
//! operator-not-at-computer became a critical SPOF.
//!
//! This MCP tool gives any agent (or operator over MCP) a
//! programmatic restart capability with minimal-state-preservation
//! semantics. After the call:
//!
//! 1. The handler records `ShutdownReason::OperatorRestart` and sets
//!    the process-wide [`crate::daemon::RESTART_PENDING`] flag.
//! 2. The handler returns `{"ok": true, "restart": "pending"}` to
//!    the caller — the response is written before the API loop
//!    notices the flag, so the caller sees a successful return
//!    before the connection drops.
//! 3. The next iteration of the API session loop bridges
//!    `RESTART_PENDING` to the local `shutdown` flag, breaking the
//!    main loop.
//! 4. `shutdown_sequence` drains agents (existing per-tick
//!    invariants).
//! 5. After `run_core` returns, the bootstrap layer detects
//!    `RESTART_PENDING` + `OperatorRestart` reason and re-execs
//!    self via `std::os::unix::process::CommandExt::exec` (Unix)
//!    or spawn-and-exit (Windows).
//!
//! ## State preservation contract (MVP)
//!
//! - **Binding metadata** (`runtime/<agent>/binding.json`): already
//!   on disk; restart picks up exactly the same bindings.
//! - **Topic registry / fleet.yaml**: already on disk; same.
//! - **In-flight MCP calls**: drained by the API loop between the
//!   response write and the shutdown break (one-iteration window).
//! - **PTY agents**: terminated by `shutdown_sequence`. Operator
//!   re-attaches post-restart (out-of-scope for this MVP per
//!   dispatch m-20260509211234305660-372).
//! - **Telegram channel state**: reconnect via existing daemon-
//!   startup logic; no special handling needed.

use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

/// MCP tool: `restart_daemon`.
///
/// Required args: none.
///
/// Returns:
/// - On success: `{"ok": true, "restart": "pending", "reason":
///   "operator_restart"}`. The connection will drop shortly after
///   the response is written.
/// - On failure: `{"error": "...", "code": "..."}` — currently only
///   the `already_pending` code surfaces, when a prior call has
///   already set the flag.
pub(crate) fn handle_restart_daemon(
    _home: &Path,
    _args: &Value,
    _sender: &Option<Sender>,
) -> Value {
    use std::sync::atomic::Ordering;

    // Atomic compare-exchange so concurrent calls produce a single
    // restart cycle, not a thundering herd.
    let prior = crate::daemon::RESTART_PENDING.swap(true, Ordering::AcqRel);
    if prior {
        return json!({
            "error": "restart already pending — earlier call has set the flag",
            "code": "already_pending"
        });
    }

    crate::daemon::record_shutdown_reason(crate::daemon::ShutdownReason::OperatorRestart);
    tracing::info!("restart_daemon: operator-initiated restart pending");

    json!({
        "ok": true,
        "restart": "pending",
        "reason": "operator_restart",
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// Process-global env mutex — RESTART_PENDING is a static, so
    /// tests must serialize to avoid cross-test interference.
    fn restart_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn reset_state() {
        crate::daemon::RESTART_PENDING.store(false, Ordering::Release);
        crate::daemon::SHUTDOWN_REASON.store(0, Ordering::Release);
    }

    #[test]
    fn restart_daemon_sets_flag_and_records_reason() {
        let _g = restart_lock();
        reset_state();

        let home = std::path::PathBuf::from("/tmp");
        let result = handle_restart_daemon(&home, &json!({}), &None);

        assert_eq!(result["ok"].as_bool(), Some(true));
        assert_eq!(result["restart"].as_str(), Some("pending"));
        assert_eq!(result["reason"].as_str(), Some("operator_restart"));
        assert!(crate::daemon::RESTART_PENDING.load(Ordering::Acquire));
        assert_eq!(
            crate::daemon::SHUTDOWN_REASON.load(Ordering::Acquire),
            crate::daemon::ShutdownReason::OperatorRestart as u8
        );

        reset_state();
    }

    #[test]
    fn restart_daemon_idempotent_returns_already_pending_on_second_call() {
        let _g = restart_lock();
        reset_state();

        let home = std::path::PathBuf::from("/tmp");
        let first = handle_restart_daemon(&home, &json!({}), &None);
        assert_eq!(first["ok"].as_bool(), Some(true));

        let second = handle_restart_daemon(&home, &json!({}), &None);
        assert!(
            second.get("ok").is_none() || second["ok"].as_bool() == Some(false),
            "second call must not return ok=true: {second}"
        );
        assert_eq!(second["code"].as_str(), Some("already_pending"));
        // Flag stays set; reason stays operator_restart.
        assert!(crate::daemon::RESTART_PENDING.load(Ordering::Acquire));

        reset_state();
    }

    #[test]
    fn restart_daemon_records_only_first_shutdown_reason() {
        // record_shutdown_reason is first-write-wins (compare_exchange
        // gated on Unknown). If a prior shutdown reason was already
        // recorded (e.g. SIGTERM arrived just before restart_daemon
        // was called), the restart call must not clobber it. This
        // matches the existing taxonomy invariant.
        let _g = restart_lock();
        reset_state();

        crate::daemon::record_shutdown_reason(crate::daemon::ShutdownReason::Signal);
        let home = std::path::PathBuf::from("/tmp");
        let _ = handle_restart_daemon(&home, &json!({}), &None);

        // Reason stays Signal (first-write-wins) even though restart
        // attempted to record OperatorRestart.
        assert_eq!(
            crate::daemon::SHUTDOWN_REASON.load(Ordering::Acquire),
            crate::daemon::ShutdownReason::Signal as u8
        );
        // RESTART_PENDING flag still set — restart will still happen,
        // just with the prior shutdown reason in the audit trail.
        assert!(crate::daemon::RESTART_PENDING.load(Ordering::Acquire));

        reset_state();
    }
}
