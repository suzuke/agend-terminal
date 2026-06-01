//! Watchdog: classify PTY output into BlockedReason per daemon tick.

use crate::backend::Backend;
use crate::health::{BlockedReason, HealthTracker};
use crate::state::AgentState;
use std::path::Path;

/// bughunt2: the AgentStates that mean the agent is actively working / ready
/// again — i.e. recovered from a transient rate-limit. Used to auto-clear
/// the set-only RateLimit/QuotaExceeded health latch (mirrors the way the
/// underlying AgentState self-expires when the throttle banner clears).
fn recovered_from_rate_limit(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::Ready | AgentState::Idle | AgentState::Thinking | AgentState::ToolUse
    )
}

/// Parse `AGEND_WATCHDOG_DRY_RUN` env var. Returns true for "1"/"true"/"TRUE"/"True".
pub fn watchdog_dry_run_from_env() -> bool {
    std::env::var("AGEND_WATCHDOG_DRY_RUN")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "True"))
        .unwrap_or(false)
}

/// Run one watchdog pass for a single agent. Called from the daemon tick loop.
///
/// - Classifies `screen` text against backend-specific error patterns.
/// - `dry_run=true`: logs to event_log only, does not mutate health state.
/// - `dry_run=false`: sets `BlockedReason` on the health tracker.
pub fn run_watchdog_pass(
    home: &Path,
    agent_name: &str,
    backend: &Backend,
    screen: &str,
    health: &mut HealthTracker,
    dry_run: bool,
    current_state: AgentState,
) {
    let reason = crate::state::classify_pty_output(backend, screen);

    if dry_run {
        // Observability only — never mutate health (no set, no clear).
        if let Some(reason) = reason {
            crate::event_log::log(home, "watchdog_dry_run", agent_name, &format!("{reason:?}"));
        }
        return;
    }

    // bughunt2 auto-clear: the RateLimit/QuotaExceeded BlockedReason is a
    // SET-ONLY latch — `classify_pty_output` returns `None` once the
    // throttle banner scrolls off, so the old code never cleared it. That
    // latch permanently suppresses hang detection AND blocks task delivery,
    // so an agent stays silently "blocked" forever after a TRANSIENT limit.
    // The underlying AgentState self-expires when the limit lifts; mirror
    // that here by clearing the latch once the agent is actively working /
    // ready again. GUARDED to ONLY the rate-limit/quota latch — never
    // AwaitingOperator / PermissionPrompt / Crash / Hang (operator- or
    // crash-action-required reasons must NOT be auto-cleared; cf. the #1564
    // blocked-reason guard). The set below re-latches if the agent is in
    // fact still throttled (classify still matches).
    if recovered_from_rate_limit(current_state)
        && matches!(
            health.current_reason,
            Some(BlockedReason::RateLimit { .. } | BlockedReason::QuotaExceeded)
        )
    {
        health.clear_blocked_reason();
    }

    if let Some(reason) = reason {
        health.set_blocked_reason(reason);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::health::BlockedReason;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-watchdog-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn read_event_log(home: &Path) -> String {
        std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default()
    }

    #[test]
    fn test_watchdog_dry_run_env_logs_to_event_log() {
        let home = tmp_home("dry-run");
        let mut health = HealthTracker::new();
        let backend = Backend::KiroCli;

        run_watchdog_pass(
            &home,
            "test-agent",
            &backend,
            "ThrottlingError: Too Many Requests",
            &mut health,
            true, // dry_run
            AgentState::RateLimit,
        );

        assert!(
            health.current_reason.is_none(),
            "dry-run must not set current_reason"
        );
        let log = read_event_log(&home);
        assert!(
            log.contains("watchdog_dry_run"),
            "must log dry-run entry, got: {log}"
        );
        assert!(log.contains("RateLimit"), "must log reason, got: {log}");
        assert!(
            log.contains("test-agent"),
            "must log agent name, got: {log}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_watchdog_live_env_unset_sets_reason() {
        let home = tmp_home("live");
        let mut health = HealthTracker::new();
        let backend = Backend::KiroCli;

        run_watchdog_pass(
            &home,
            "test-agent",
            &backend,
            "ServiceQuotaExceededException: You have exceeded your quota",
            &mut health,
            false, // live
            // Agent still showing the quota banner → not recovered, so the
            // bughunt2 auto-clear stays inert and the set is observed.
            AgentState::RateLimit,
        );

        assert!(
            matches!(health.current_reason, Some(BlockedReason::QuotaExceeded)),
            "live mode must set current_reason, got: {:?}",
            health.current_reason
        );
        let log = read_event_log(&home);
        assert!(
            !log.contains("watchdog_dry_run"),
            "live mode must not write dry-run log"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_watchdog_healthy_output_no_action() {
        let home = tmp_home("healthy");
        let mut health = HealthTracker::new();
        let backend = Backend::KiroCli;

        run_watchdog_pass(
            &home,
            "test-agent",
            &backend,
            "Thinking about your request...\n● Read src/main.rs",
            &mut health,
            false,
            AgentState::Thinking,
        );

        assert!(
            health.current_reason.is_none(),
            "healthy output must not set reason"
        );
        let log = read_event_log(&home);
        assert!(
            !log.contains("watchdog"),
            "healthy output must not write watchdog log"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// bughunt2: a rate-limit / quota latch (set by a prior throttled tick)
    /// auto-clears once the agent has recovered (healthy screen + active
    /// AgentState) — restoring hang detection + task delivery.
    #[test]
    fn test_watchdog_autoclear_rate_limit_latch_on_recovery() {
        let backend = Backend::KiroCli;
        let healthy = "Thinking about your request...\n● Read src/main.rs";
        for (latch, state) in [
            (
                BlockedReason::RateLimit {
                    retry_after_secs: Some(30),
                },
                AgentState::Ready,
            ),
            (BlockedReason::QuotaExceeded, AgentState::Idle),
            (
                BlockedReason::RateLimit {
                    retry_after_secs: None,
                },
                AgentState::ToolUse,
            ),
        ] {
            let home = tmp_home("autoclear-rl");
            let mut health = HealthTracker::new();
            health.set_blocked_reason(latch.clone());
            run_watchdog_pass(
                &home,
                "test-agent",
                &backend,
                healthy,
                &mut health,
                false,
                state,
            );
            assert!(
                health.current_reason.is_none(),
                "bughunt2: recovered agent ({state:?}) must auto-clear {latch:?}, got: {:?}",
                health.current_reason
            );
            std::fs::remove_dir_all(&home).ok();
        }
    }

    /// A genuinely still-limited agent (throttle banner present, state
    /// still RateLimit → not recovered) stays latched.
    #[test]
    fn test_watchdog_still_limited_stays_latched() {
        let home = tmp_home("still-limited");
        let mut health = HealthTracker::new();
        let backend = Backend::KiroCli;
        health.set_blocked_reason(BlockedReason::RateLimit {
            retry_after_secs: None,
        });
        run_watchdog_pass(
            &home,
            "test-agent",
            &backend,
            "ThrottlingError: Too Many Requests",
            &mut health,
            false,
            AgentState::RateLimit,
        );
        assert!(
            matches!(health.current_reason, Some(BlockedReason::RateLimit { .. })),
            "still-limited agent must stay latched, got: {:?}",
            health.current_reason
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Guard: the auto-clear is scoped to RateLimit/QuotaExceeded. An
    /// operator-action-required reason (AwaitingOperator) must NOT be
    /// cleared even when the agent looks recovered on screen.
    #[test]
    fn test_watchdog_autoclear_guard_spares_non_rate_limit_reasons() {
        let home = tmp_home("guard");
        let mut health = HealthTracker::new();
        let backend = Backend::KiroCli;
        health.set_blocked_reason(BlockedReason::AwaitingOperator);
        run_watchdog_pass(
            &home,
            "test-agent",
            &backend,
            "Thinking about your request...\n● Read src/main.rs",
            &mut health,
            false,
            AgentState::Ready,
        );
        assert!(
            matches!(health.current_reason, Some(BlockedReason::AwaitingOperator)),
            "bughunt2 guard: non-rate-limit reasons must NOT auto-clear, got: {:?}",
            health.current_reason
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// dry-run never mutates health — it must NOT auto-clear a latch
    /// either (observability mode is read-only).
    #[test]
    fn test_watchdog_dry_run_does_not_autoclear() {
        let home = tmp_home("dry-noclear");
        let mut health = HealthTracker::new();
        let backend = Backend::KiroCli;
        health.set_blocked_reason(BlockedReason::RateLimit {
            retry_after_secs: None,
        });
        run_watchdog_pass(
            &home,
            "test-agent",
            &backend,
            "Thinking about your request...\n● Read src/main.rs",
            &mut health,
            true, // dry_run
            AgentState::Ready,
        );
        assert!(
            matches!(health.current_reason, Some(BlockedReason::RateLimit { .. })),
            "dry-run must not mutate (clear) health, got: {:?}",
            health.current_reason
        );
        std::fs::remove_dir_all(&home).ok();
    }

    static ENV_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn test_watchdog_env_true_returns_true() {
        let _guard = ENV_LOCK.lock();
        for val in ["1", "true", "TRUE", "True"] {
            std::env::set_var("AGEND_WATCHDOG_DRY_RUN", val);
            assert!(
                super::watchdog_dry_run_from_env(),
                "AGEND_WATCHDOG_DRY_RUN={val} should return true"
            );
        }
        std::env::remove_var("AGEND_WATCHDOG_DRY_RUN");
    }

    #[test]
    fn test_watchdog_env_false_returns_false() {
        let _guard = ENV_LOCK.lock();
        for val in ["0", "false", "FALSE", "no", ""] {
            std::env::set_var("AGEND_WATCHDOG_DRY_RUN", val);
            assert!(
                !super::watchdog_dry_run_from_env(),
                "AGEND_WATCHDOG_DRY_RUN={val} should return false"
            );
        }
        std::env::remove_var("AGEND_WATCHDOG_DRY_RUN");
    }

    #[test]
    fn test_watchdog_env_unset_returns_false() {
        let _guard = ENV_LOCK.lock();
        std::env::remove_var("AGEND_WATCHDOG_DRY_RUN");
        assert!(
            !super::watchdog_dry_run_from_env(),
            "unset AGEND_WATCHDOG_DRY_RUN should return false"
        );
    }
}
