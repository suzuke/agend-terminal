//! #1228: Auto-close task when assignee sends kind=report with matching correlation_id.

use std::path::Path;

/// Auto-close a task when the assignee sends a terminal report.
/// Returns `Ok(true)` if the task was auto-closed, `Ok(false)` if skipped.
pub fn auto_close_on_report(
    home: &Path,
    kind: &str,
    correlation_id: &str,
    reporter: &str,
    report_text: &str,
    terminal: bool,
) -> anyhow::Result<bool> {
    if !terminal {
        return Ok(false);
    }
    if kind != "report" {
        return Ok(false);
    }
    if !correlation_id.starts_with("t-") {
        return Ok(false);
    }
    let state = crate::task_events::replay(home).unwrap_or_default();
    let tid = crate::task_events::TaskId(correlation_id.to_string());
    let Some(record) = state.tasks.get(&tid) else {
        return Ok(false);
    };
    use crate::task_events::TaskStatus;
    if !matches!(
        record.status,
        TaskStatus::Open | TaskStatus::Claimed | TaskStatus::InProgress | TaskStatus::Blocked
    ) {
        return Ok(false);
    }
    let assignee = record.owner.as_ref().map(|o| o.0.as_str()).unwrap_or("");
    if assignee != reporter {
        return Ok(false);
    }
    let summary = if report_text.chars().count() > 200 {
        let truncated: String = report_text.chars().take(200).collect();
        format!("{truncated}…")
    } else {
        report_text.to_string()
    };
    let event = crate::task_events::TaskEvent::Done {
        task_id: tid,
        by: crate::task_events::InstanceName(reporter.to_string()),
        source: crate::task_events::DoneSource::ReportAutoClose {
            report_summary: summary,
            closed_at: chrono::Utc::now().to_rfc3339(),
        },
    };
    let emitter = crate::task_events::InstanceName::from("system:auto-close");
    crate::task_events::append(home, &emitter, event)?;
    let _ = crate::daemon::dispatch_idle::cleanup_pending_for_task_id(home, correlation_id);
    Ok(true)
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
                    tags: vec![],
                    parent_id: None,
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
                    tags: vec![],
                    parent_id: None,
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
                    tags: vec![],
                    parent_id: None,
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
        state.tasks.get(&TaskId(task_id.into())).map(|r| r.status)
    }

    #[test]
    fn assignee_terminal_report_auto_closes_claimed_task() {
        let home = tmp_home("assignee_close");
        seed_claimed_task(&home, "t-1228-001", "dev-agent");
        let closed = auto_close_on_report(
            &home,
            "report",
            "t-1228-001",
            "dev-agent",
            "Task completed successfully",
            true,
        )
        .unwrap();
        assert!(closed, "terminal assignee report should auto-close");
        assert_eq!(
            task_status(&home, "t-1228-001"),
            Some(crate::task_events::TaskStatus::Done),
            "task status should be Done after auto-close"
        );
    }

    #[test]
    fn non_terminal_report_does_not_close() {
        let home = tmp_home("non_terminal");
        seed_claimed_task(&home, "t-1228-007", "dev-agent");
        let closed = auto_close_on_report(
            &home,
            "report",
            "t-1228-007",
            "dev-agent",
            "progress update — 60% done",
            false,
        )
        .unwrap();
        assert!(!closed, "terminal=false should NOT auto-close");
        assert_eq!(
            task_status(&home, "t-1228-007"),
            Some(crate::task_events::TaskStatus::Claimed),
            "task should remain Claimed when terminal=false"
        );
    }

    #[test]
    fn kind_update_does_not_close() {
        let home = tmp_home("kind_update");
        seed_claimed_task(&home, "t-1228-002", "dev-agent");
        let closed = auto_close_on_report(
            &home,
            "update",
            "t-1228-002",
            "dev-agent",
            "progress update",
            true,
        )
        .unwrap();
        assert!(
            !closed,
            "kind=update should NOT auto-close even if terminal"
        );
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
            &home,
            "report",
            "t-1228-003",
            "reviewer-agent",
            "VERIFIED",
            true,
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
            &home,
            "report",
            "qcorr-20260525",
            "dev-agent",
            "some report",
            true,
        )
        .unwrap();
        assert!(!closed, "non-t- correlation_id should skip");
    }

    #[test]
    fn already_done_is_idempotent() {
        let home = tmp_home("already_done");
        seed_done_task(&home, "t-1228-005", "dev-agent");
        let closed = auto_close_on_report(
            &home,
            "report",
            "t-1228-005",
            "dev-agent",
            "duplicate report",
            true,
        )
        .unwrap();
        assert!(
            !closed,
            "already-done task should return false (idempotent)"
        );
    }

    #[test]
    fn already_cancelled_is_idempotent() {
        let home = tmp_home("already_cancelled");
        seed_cancelled_task(&home, "t-1228-006");
        let closed = auto_close_on_report(
            &home,
            "report",
            "t-1228-006",
            "dev-agent",
            "late report",
            true,
        )
        .unwrap();
        assert!(
            !closed,
            "already-cancelled task should return false (idempotent)"
        );
    }
}
