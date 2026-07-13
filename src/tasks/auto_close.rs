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
    // #2760 (codex ruling m-…-1154): gate on the TYPED canonical parser (the same
    // authority dispatch-idle uses), not a raw `starts_with("t-")` string
    // convention. A non-task / query correlation parses to `None` → skip (a non-task
    // report has no task to auto-close). Production task ids are always canonical,
    // so this is behavior-preserving there (a non-canonical id would `NotFound`
    // through the strict route below anyway).
    if crate::task_events::TaskId::parse_canonical(correlation_id).is_none() {
        return Ok(false);
    }
    // #2760: resolve the task's authoritative board via the strict route. Fail
    // closed — a task whose board cannot be uniquely proven (route error / unknown
    // id) is NOT auto-closed (matches the pre-#2760 "record not found → Ok(false)").
    let routed = match super::load_routed(home, correlation_id) {
        Ok(rt) => rt,
        Err(_) => return Ok(false),
    };
    let tid = crate::task_events::TaskId(correlation_id.to_string());
    let record = routed.record();
    use crate::task_events::TaskStatus;
    // #1942: `InReview` was added in #1265 but never added to this whitelist, so
    // a terminal report on a task the lead promoted to `in_review` was silently
    // dropped → task stranded open → next dispatch blocked busy. `InReview→Done`
    // is a legal transition (`can_transition_to`), so a terminal report from the
    // assignee closes an `in_review` task just as it closes an `in_progress` one.
    if !matches!(
        record.status,
        TaskStatus::Open
            | TaskStatus::Claimed
            | TaskStatus::InProgress
            | TaskStatus::Blocked
            | TaskStatus::InReview
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
    // CR-2026-06-14: underscore form, matching `acl::SYSTEM_IDENTITIES` +
    // status_summary. The prior hyphen variant was absent from the ACL allow-list,
    // so `is_system_identity` denied it if routed through `can_mutate_record`.
    let emitter = crate::task_events::InstanceName::from("system:auto_close");
    // #1873: re-validate →Done UNDER the lock — a concurrent cancel between the
    // out-of-lock status check above and this append must not be flipped to Done.
    // #2760 items 2+3: additionally under the per-id router lock with write-time
    // route revalidation (the closure does ONLY the append; the cascade below —
    // terminal cleanup + release recompute, which may self-IPC — runs AFTER the
    // flock drops, #1629). A route change under the lock → not closed.
    let closed = match routed.with_revalidated_board(home, |board| {
        crate::task_events::append_done_if_legal_at(board, &emitter, correlation_id, vec![event])
    }) {
        Ok(inner) => inner?,
        Err(route_err) => {
            tracing::warn!(
                task_id = correlation_id, %route_err,
                "#2760 auto-close skipped: route revalidation failed under the per-id lock"
            );
            return Ok(false);
        }
    };
    if closed {
        // #1018/#78445-2 (d): terminal auto-close — shared cleanup of both stores.
        super::task_terminal_cleanup(home, correlation_id);
        // #t-…24962-7: a verdict-auto-closed task is a terminal task-done event
        // too — enqueue a release-invariant recompute so the reporter's worktree is
        // released, mirroring the MCP task-done handler (tasks/handler.rs). Without
        // this, a review task closed by a terminal verdict (which has no PR of its
        // own) never enqueues an intent → its binding leaks (immortal review
        // worktree). `assignee == reporter` was enforced above; repo="" → the
        // sweeper derives it from the binding's source_repo.
        if let Some(binding) = crate::binding::read(home, reporter) {
            if let Some(branch) = binding["branch"].as_str() {
                crate::daemon::auto_release::enqueue_release_recompute(
                    home,
                    "",
                    branch,
                    "task_done",
                );
            }
        }
    }
    Ok(closed)
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
        seed_claimed_task_on_board(task_id, assignee, home);
    }

    fn seed_claimed_task_on_board(task_id: &str, assignee: &str, board: &Path) {
        let emitter = InstanceName::from("test:seed");
        let tid = TaskId(task_id.into());
        crate::task_events::append_batch_at(
            board,
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
        task_status_on_board(task_id, home)
    }

    fn task_status_on_board(task_id: &str, board: &Path) -> Option<crate::task_events::TaskStatus> {
        let state = crate::task_events::replay_at(board).unwrap();
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

    /// #78445-2 PR-C (defect d) RED-first: closing a task must SETTLE its
    /// `dispatch_tracking` rows (matched by task_id) so the stuck-dispatch sweep
    /// stops nagging about a dispatch whose task the board already closed (the
    /// reviewer4 double "dispatch stuck check"). Isolation (lead reminder): a
    /// DIFFERENT task's row — even from the SAME dispatcher — must SURVIVE.
    /// Pre-fix: the closed task's row lingered → still active/nagged.
    #[test]
    fn task_close_settles_dispatch_tracking_rows_78445_2() {
        use crate::dispatch_tracking::{active_target_names, track_dispatch, DispatchEntry};
        let home = tmp_home("pc_settle_dispatch");
        let now = chrono::Utc::now().to_rfc3339();
        // A review dispatch (lead → rev-a) for the task we will close …
        track_dispatch(
            &home,
            DispatchEntry {
                task_id: Some("t-8001-1".into()),
                from: "lead".into(),
                to: "rev-a".into(),
                from_id: None,
                to_id: None,
                delegated_at: now.clone(),
                status: "pending".into(),
            },
        );
        // … and one for a DIFFERENT task from the SAME dispatcher (isolation guard).
        track_dispatch(
            &home,
            DispatchEntry {
                task_id: Some("t-8002-1".into()),
                from: "lead".into(),
                to: "rev-b".into(),
                from_id: None,
                to_id: None,
                delegated_at: now,
                status: "pending".into(),
            },
        );

        seed_claimed_task(&home, "t-8001-1", "dev-agent");
        let closed =
            auto_close_on_report(&home, "report", "t-8001-1", "dev-agent", "done", true).unwrap();
        assert!(
            closed,
            "precondition: assignee terminal report auto-closes the task"
        );

        let active = active_target_names(&home);
        assert!(
            !active.contains(&"rev-a".to_string()),
            "#78445-2 (d): closing the task must settle its dispatch_tracking row (rev-a): {active:?}"
        );
        assert!(
            active.contains(&"rev-b".to_string()),
            "#78445-2 (d) isolation: a DIFFERENT task's row (rev-b, same dispatcher) must SURVIVE: {active:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn assignee_terminal_report_auto_closes_non_default_board_task_2498() {
        let home = tmp_home("assignee_close_project");
        let board = crate::task_events::board_root(&home, "proj-2498");
        seed_claimed_task_on_board("t-2498-1", "dev-agent", &board);
        super::super::board_router::record_task_project(&home, "t-2498-1", "proj-2498")
            .expect("record project index");

        let closed = auto_close_on_report(
            &home,
            "report",
            "t-2498-1",
            "dev-agent",
            "Task completed successfully",
            true,
        )
        .unwrap();

        assert!(
            closed,
            "terminal assignee report should auto-close across project boards"
        );
        assert_eq!(
            task_status_on_board("t-2498-1", &board),
            Some(crate::task_events::TaskStatus::Done),
            "task status should be Done on its non-default board after auto-close"
        );
        assert_eq!(
            task_status(&home, "t-2498-1"),
            None,
            "auto-close must not create or mutate a default-board copy"
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

    /// #1873 §3.9: `append_done_if_legal` — the in-lock guard BOTH daemon →Done
    /// paths (auto_close + sweep) now use — REJECTS a →Done on a task that moved
    /// to a terminal state (a concurrent cancel landing after the out-of-lock
    /// check), leaving it Cancelled; a legal Claimed task still auto-closes. The
    /// full auto_close path's happy + already-cancelled cases are covered above.
    #[test]
    fn append_done_if_legal_rejects_cancelled_keeps_legal_1873() {
        let home = tmp_home("1873-guard");
        let emitter = InstanceName::from("system:auto-close");
        let mk_done = |id: &str| TaskEvent::Done {
            task_id: TaskId(id.into()),
            by: InstanceName::from("dev"),
            source: crate::task_events::DoneSource::ReportAutoClose {
                report_summary: "x".into(),
                closed_at: "2026-06-09T00:00:00+00:00".into(),
            },
        };

        // Concurrently-cancelled task → the →Done is SKIPPED, stays Cancelled.
        seed_cancelled_task(&home, "t-cancel");
        let closed = crate::task_events::append_done_if_legal(
            &home,
            &emitter,
            "t-cancel",
            vec![mk_done("t-cancel")],
        )
        .unwrap();
        assert!(
            !closed,
            "#1873: a daemon →Done on a Cancelled task must be SKIPPED"
        );
        assert_eq!(
            task_status(&home, "t-cancel"),
            Some(crate::task_events::TaskStatus::Cancelled),
            "task must stay Cancelled (not flipped to Done)"
        );

        // Legal Claimed task → still auto-closes.
        seed_claimed_task(&home, "t-ok", "dev");
        let closed_ok = crate::task_events::append_done_if_legal(
            &home,
            &emitter,
            "t-ok",
            vec![mk_done("t-ok")],
        )
        .unwrap();
        assert!(
            closed_ok,
            "#1873: a legal Claimed task must still auto-close"
        );
        assert_eq!(
            task_status(&home, "t-ok"),
            Some(crate::task_events::TaskStatus::Done)
        );
    }

    #[test]
    fn assignee_terminal_report_closes_in_review_task_1942() {
        // #1942: a task the lead promoted to `in_review` (code-review flow) must
        // still auto-close on the assignee's terminal report. `InReview` was
        // missing from the whitelist, so the report was silently dropped and the
        // task stranded open → next dispatch blocked busy.
        let home = tmp_home("in_review_close");
        seed_claimed_task(&home, "t-1942-1", "dev-agent");
        crate::task_events::append_batch(
            &home,
            &InstanceName::from("test:lead"),
            vec![TaskEvent::MovedToReview {
                task_id: TaskId("t-1942-1".into()),
            }],
        )
        .expect("promote to in_review");
        assert_eq!(
            task_status(&home, "t-1942-1"),
            Some(crate::task_events::TaskStatus::InReview),
            "precondition: task is in_review"
        );
        let closed = auto_close_on_report(
            &home,
            "report",
            "t-1942-1",
            "dev-agent",
            "review done",
            true,
        )
        .unwrap();
        assert!(
            closed,
            "#1942: a terminal report must auto-close an in_review task"
        );
        assert_eq!(
            task_status(&home, "t-1942-1"),
            Some(crate::task_events::TaskStatus::Done)
        );
    }

    fn task_result(home: &Path, task_id: &str) -> Option<String> {
        crate::task_events::replay(home)
            .unwrap()
            .tasks
            .get(&TaskId(task_id.into()))
            .and_then(|r| r.result.clone())
    }

    /// F1 (spike t-…19288-1): a terminal report auto-close must project the
    /// report body into the task's `result`. Pre-fix `apply_done` set `result`
    /// only from `DoneSource::OperatorManual`, so `ReportAutoClose.report_summary`
    /// was dropped and the closed task's `result` stayed null.
    #[test]
    fn report_auto_close_projects_report_into_result() {
        let home = tmp_home("f1_result");
        seed_claimed_task(&home, "t-9001-1", "dev-agent");
        let report = "RESULT: fixed the parser; PR #123 merged; all suites green.";
        let closed =
            auto_close_on_report(&home, "report", "t-9001-1", "dev-agent", report, true).unwrap();
        assert!(closed, "precondition: terminal assignee report auto-closes");
        assert_eq!(
            task_status(&home, "t-9001-1"),
            Some(crate::task_events::TaskStatus::Done),
            "precondition: task is Done"
        );
        assert_eq!(
            task_result(&home, "t-9001-1").as_deref(),
            Some(report),
            "F1: auto-close must persist the report into `result` (was null)"
        );
    }

    /// F1 guard: a `ReportAutoClose` must NOT overwrite an already-set `result`
    /// (e.g. an operator-manual result). Only a null result is backfilled.
    #[test]
    fn report_auto_close_does_not_overwrite_explicit_result() {
        let home = tmp_home("f1_guard");
        let tid = TaskId("t-f1-guard".into());
        crate::task_events::append_batch(
            &home,
            &InstanceName::from("test:seed"),
            vec![
                TaskEvent::Created {
                    task_id: tid.clone(),
                    title: "t".into(),
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
                    task_id: tid.clone(),
                    by: InstanceName::from("op"),
                    source: crate::task_events::DoneSource::OperatorManual {
                        authored_at: "2026-01-01T00:00:00Z".into(),
                        result: Some("explicit operator result".into()),
                    },
                },
                // A later ReportAutoClose on the same task must be a no-op for `result`.
                TaskEvent::Done {
                    task_id: tid,
                    by: InstanceName::from("dev"),
                    source: crate::task_events::DoneSource::ReportAutoClose {
                        report_summary: "auto summary".into(),
                        closed_at: "2026-01-02T00:00:00Z".into(),
                    },
                },
            ],
        )
        .expect("seed");
        assert_eq!(
            task_result(&home, "t-f1-guard").as_deref(),
            Some("explicit operator result"),
            "F1 guard: ReportAutoClose must not overwrite an explicit result"
        );
    }
}
