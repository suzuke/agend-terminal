//! #1228: Auto-close task when assignee sends kind=report with matching correlation_id.

use std::path::Path;

/// Auto-close a task when the assignee sends kind=report.
/// Returns `Ok(true)` if the task was auto-closed, `Ok(false)` if skipped.
pub fn auto_close_on_report(
    _home: &Path,
    _kind: &str,
    _correlation_id: &str,
    _reporter: &str,
    _report_text: &str,
) -> anyhow::Result<bool> {
    Ok(false) // stub — RED tests will assert-fail
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::task_events::{InstanceName, TaskEvent, TaskId};

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-auto-close-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn seed_claimed_task(home: &Path, task_id: &str, assignee: &str) {
        let emitter = InstanceName::from("test:seed");
        let tid = TaskId(task_id.into());
        crate::task_events::append_batch(
            home,
            &emitter,
            vec![
                TaskEvent::Created {
                    task_id: tid.clone(),
                    title: "test task".into(),
                    description: String::new(),
                    priority: "normal".into(),
                    owner: None,
                    due_at: None,
                    depends_on: Vec::new(),
                    routed_to: None,
                    branch: None,
                    bind: None,
                    eta_secs: None,
                },
                TaskEvent::Claimed {
                    task_id: tid,
                    by: InstanceName::from(assignee),
                },
            ],
        )
        .expect("seed task");
    }

    fn seed_done_task(home: &Path, task_id: &str, assignee: &str) {
        let emitter = InstanceName::from("test:seed");
        let tid = TaskId(task_id.into());
        crate::task_events::append_batch(
            home,
            &emitter,
            vec![
                TaskEvent::Created {
                    task_id: tid.clone(),
                    title: "test task".into(),
                    description: String::new(),
                    priority: "normal".into(),
                    owner: None,
                    due_at: None,
                    depends_on: Vec::new(),
                    routed_to: None,
                    branch: None,
                    bind: None,
                    eta_secs: None,
                },
                TaskEvent::Done {
                    task_id: tid,
                    by: InstanceName::from(assignee),
                    source: crate::task_events::DoneSource::OperatorManual {
                        authored_at: "2026-01-01T00:00:00Z".into(),
                        result: Some("already done".into()),
                    },
                },
            ],
        )
        .expect("seed done task");
    }

    fn seed_cancelled_task(home: &Path, task_id: &str) {
        let emitter = InstanceName::from("test:seed");
        let tid = TaskId(task_id.into());
        crate::task_events::append_batch(
            home,
            &emitter,
            vec![
                TaskEvent::Created {
                    task_id: tid.clone(),
                    title: "test task".into(),
                    description: String::new(),
                    priority: "normal".into(),
                    owner: None,
                    due_at: None,
                    depends_on: Vec::new(),
                    routed_to: None,
                    branch: None,
                    bind: None,
                    eta_secs: None,
                },
                TaskEvent::Cancelled {
                    task_id: tid,
                    by: InstanceName::from("test:cancel"),
                    reason: "test cancel".into(),
                },
            ],
        )
        .expect("seed cancelled task");
    }

    fn task_status(home: &Path, task_id: &str) -> Option<crate::task_events::TaskStatus> {
        let state = crate::task_events::replay(home).unwrap();
        state
            .tasks
            .get(&TaskId(task_id.into()))
            .map(|r| r.status)
    }

    // ── RED tests: all assert-fail on the stub ──

    #[test]
    fn assignee_report_auto_closes_claimed_task() {
        let home = tmp_home("assignee_close");
        seed_claimed_task(&home, "t-1228-001", "dev-agent");
        let closed = auto_close_on_report(
            &home, "report", "t-1228-001", "dev-agent", "Task completed successfully",
        )
        .unwrap();
        assert!(closed, "assignee report should auto-close the task");
        assert_eq!(
            task_status(&home, "t-1228-001"),
            Some(crate::task_events::TaskStatus::Done),
            "task status should be Done after auto-close"
        );
    }

    #[test]
    fn kind_update_does_not_close() {
        let home = tmp_home("kind_update");
        seed_claimed_task(&home, "t-1228-002", "dev-agent");
        let closed = auto_close_on_report(
            &home, "update", "t-1228-002", "dev-agent", "progress update",
        )
        .unwrap();
        assert!(!closed, "kind=update should NOT auto-close");
        assert_eq!(
            task_status(&home, "t-1228-002"),
            Some(crate::task_events::TaskStatus::Claimed),
            "task should remain Claimed after kind=update"
        );
    }

    #[test]
    fn non_assignee_does_not_close() {
        let home = tmp_home("non_assignee");
        seed_claimed_task(&home, "t-1228-003", "dev-agent");
        let closed = auto_close_on_report(
            &home, "report", "t-1228-003", "reviewer-agent", "VERIFIED",
        )
        .unwrap();
        assert!(!closed, "non-assignee report should NOT auto-close");
        assert_eq!(
            task_status(&home, "t-1228-003"),
            Some(crate::task_events::TaskStatus::Claimed),
            "task should remain Claimed when reporter is not assignee"
        );
    }

    #[test]
    fn non_task_correlation_id_skips() {
        let home = tmp_home("non_task_corr");
        seed_claimed_task(&home, "t-1228-004", "dev-agent");
        let closed = auto_close_on_report(
            &home, "report", "qcorr-20260525", "dev-agent", "some report",
        )
        .unwrap();
        assert!(!closed, "non-t- correlation_id should skip");
    }

    #[test]
    fn already_done_is_idempotent() {
        let home = tmp_home("already_done");
        seed_done_task(&home, "t-1228-005", "dev-agent");
        let closed = auto_close_on_report(
            &home, "report", "t-1228-005", "dev-agent", "duplicate report",
        )
        .unwrap();
        assert!(!closed, "already-done task should return false (idempotent)");
    }

    #[test]
    fn already_cancelled_is_idempotent() {
        let home = tmp_home("already_cancelled");
        seed_cancelled_task(&home, "t-1228-006");
        let closed = auto_close_on_report(
            &home, "report", "t-1228-006", "dev-agent", "late report",
        )
        .unwrap();
        assert!(!closed, "already-cancelled task should return false (idempotent)");
    }
}
