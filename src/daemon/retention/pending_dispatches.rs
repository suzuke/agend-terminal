//! Pending-dispatches retention — sweep resolved/expired sidecars.

use std::path::Path;

/// t-dispatchidle-clear-on-report (3): terminal-status sidecars (`Resolved` /
/// `Exceeded`) are DONE — a resolved dispatch had its report, an exceeded one
/// already fired its nudge — so they have no further use and are swept after a
/// SHORT age rather than lingering the old 14 days (the observed backlog: 786
/// sidecars, 558 resolved + 228 exceeded, oldest 2026-05-22). A `Pending` sidecar
/// is ACTIVE dispatch tracking and is NEVER swept here (a real pending resolves in
/// minutes or transitions to `Exceeded`; we do not time-GC live tracking).
const TERMINAL_RETENTION_DAYS: i64 = 2;

/// Sweep terminal-status (Resolved/Exceeded) pending-dispatch sidecars older than
/// [`TERMINAL_RETENTION_DAYS`]. `cutover` gates the feature — production passes the
/// env-var value (defaults on), tests pass the bool directly to avoid
/// process-wide env-var races.
pub(super) fn sweep(home: &Path, cutover: bool) -> usize {
    if !cutover {
        return 0;
    }
    let dir = crate::daemon::dispatch_idle::pending_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let cutoff = chrono::Utc::now()
        - chrono::TimeDelta::try_days(TERMINAL_RETENTION_DAYS).expect("2 fits in i64");
    let mut swept = 0;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(dispatch): Result<crate::daemon::dispatch_idle::PendingDispatch, _> =
            serde_json::from_str(&content)
        else {
            continue;
        };
        // Only sweep TERMINAL-status sidecars; a `Pending` dispatch is live
        // tracking and must never be time-GC'd (t-dispatchidle-clear-on-report §3).
        if !matches!(
            dispatch.status,
            crate::daemon::dispatch_idle::DispatchStatus::Resolved
                | crate::daemon::dispatch_idle::DispatchStatus::Exceeded
        ) {
            continue;
        }
        let Ok(issued) = chrono::DateTime::parse_from_rfc3339(&dispatch.issued_at) else {
            continue;
        };
        let issued_utc = issued.with_timezone(&chrono::Utc);
        if issued_utc < cutoff && std::fs::remove_file(&path).is_ok() {
            swept += 1;
            tracing::info!(
                dispatch_id = %dispatch.dispatch_id,
                age_days = (chrono::Utc::now() - issued_utc).num_days(),
                "retention: pending-dispatch swept"
            );
            crate::event_log::log(
                home,
                "retention_dispatch_swept",
                &dispatch.dispatch_id,
                &format!("issued_at={}", dispatch.issued_at),
            );
        }
    }
    swept
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-retention-dispatch-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn write_dispatch(home: &Path, dispatch_id: &str, age_days: i64, status: &str) {
        let dir = crate::daemon::dispatch_idle::pending_dir(home);
        std::fs::create_dir_all(&dir).unwrap();
        let issued_at =
            (chrono::Utc::now() - chrono::TimeDelta::try_days(age_days).unwrap()).to_rfc3339();
        let dispatch = serde_json::json!({
            "schema_version": 1,
            "dispatch_id": dispatch_id,
            "dispatcher": "lead",
            "target": "dev",
            "correlation_id": null,
            "expected_kind": "task",
            "threshold_secs": 600,
            "issued_at": issued_at,
            "status": status,
            "nudge_sent_at": null,
        });
        std::fs::write(
            dir.join(format!("{dispatch_id}.json")),
            serde_json::to_string_pretty(&dispatch).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn sweep_removes_old_terminal_keeps_recent_and_pending() {
        let home = tmp_home("sweep-old");
        // old TERMINAL (exceeded/resolved) → swept past TERMINAL_RETENTION_DAYS.
        write_dispatch(&home, "disp-exceeded-old", 20, "exceeded");
        write_dispatch(&home, "disp-resolved-old", 20, "resolved");
        // recent terminal (< 2d) → kept.
        write_dispatch(&home, "disp-exceeded-recent", 1, "exceeded");
        // old PENDING → KEPT (active tracking is never time-GC'd, §3).
        write_dispatch(&home, "disp-pending-old", 20, "pending");

        let swept = sweep(&home, true);

        assert_eq!(swept, 2, "both old terminal sidecars swept");
        let dir = crate::daemon::dispatch_idle::pending_dir(&home);
        assert!(!dir.join("disp-exceeded-old.json").exists());
        assert!(!dir.join("disp-resolved-old.json").exists());
        assert!(
            dir.join("disp-exceeded-recent.json").exists(),
            "recent terminal (< age) kept"
        );
        assert!(
            dir.join("disp-pending-old.json").exists(),
            "old PENDING must be KEPT — active dispatch tracking is never swept"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn sweep_skipped_without_cutover() {
        let home = tmp_home("sweep-noenv");
        write_dispatch(&home, "disp-old2", 20, "exceeded");

        let swept = sweep(&home, false);

        assert_eq!(swept, 0);
        let dir = crate::daemon::dispatch_idle::pending_dir(&home);
        assert!(dir.join("disp-old2.json").exists());

        std::fs::remove_dir_all(&home).ok();
    }
}
