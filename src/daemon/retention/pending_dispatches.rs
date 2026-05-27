//! Pending-dispatches retention — sweep resolved/expired sidecars.

use std::path::Path;

const RETENTION_DAYS: i64 = 14;

/// Sweep pending-dispatch sidecars older than 14 days.
/// `cutover` gates the feature — production passes the env-var value,
/// tests pass the bool directly to avoid process-wide env-var races.
pub(super) fn sweep(home: &Path, cutover: bool) -> usize {
    if !cutover {
        return 0;
    }
    let dir = crate::daemon::dispatch_idle::pending_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let cutoff =
        chrono::Utc::now() - chrono::TimeDelta::try_days(RETENTION_DAYS).expect("14 fits in i64");
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

    fn write_dispatch(home: &Path, dispatch_id: &str, age_days: i64) {
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
            "status": "pending",
            "nudge_sent_at": null,
        });
        std::fs::write(
            dir.join(format!("{dispatch_id}.json")),
            serde_json::to_string_pretty(&dispatch).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn sweep_removes_old_dispatches() {
        let home = tmp_home("sweep-old");
        write_dispatch(&home, "disp-old", 20);
        write_dispatch(&home, "disp-recent", 3);

        let swept = sweep(&home, true);

        assert_eq!(swept, 1);
        let dir = crate::daemon::dispatch_idle::pending_dir(&home);
        assert!(!dir.join("disp-old.json").exists());
        assert!(dir.join("disp-recent.json").exists());

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn sweep_skipped_without_cutover() {
        let home = tmp_home("sweep-noenv");
        write_dispatch(&home, "disp-old2", 20);

        let swept = sweep(&home, false);

        assert_eq!(swept, 0);
        let dir = crate::daemon::dispatch_idle::pending_dir(&home);
        assert!(dir.join("disp-old2.json").exists());

        std::fs::remove_dir_all(&home).ok();
    }
}
