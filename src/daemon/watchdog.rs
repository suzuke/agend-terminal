//! Watchdog: classify PTY output into BlockedReason per daemon tick.

use crate::backend::Backend;
use crate::health::HealthTracker;
use std::path::Path;

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
) {
    let reason = match crate::state::classify_pty_output(backend, screen) {
        Some(r) => r,
        None => return,
    };
    if dry_run {
        crate::event_log::log(home, "watchdog_dry_run", agent_name, &format!("{reason:?}"));
    } else {
        health.set_blocked_reason(reason);
    }
}

#[cfg(test)]
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

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_watchdog_env_true_returns_true() {
        let _guard = ENV_LOCK.lock().unwrap();
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
        let _guard = ENV_LOCK.lock().unwrap();
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
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGEND_WATCHDOG_DRY_RUN");
        assert!(
            !super::watchdog_dry_run_from_env(),
            "unset AGEND_WATCHDOG_DRY_RUN should return false"
        );
    }
}
