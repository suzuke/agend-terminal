//! Merge Train admission for branch-producing task dispatches (Arch14).
//!
//! Determines Front/Queued/Refuse positioning when multiple in-flight tasks
//! target the same repository+domain, enforcing serialized merge ordering
//! across project boards.

use std::path::Path;

// ── Public types ────────────────────────────────────────────────────────────

/// Outcome of merge-train admission evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MergeTrainAdmission {
    /// First in line — dispatch proceeds with full side effects.
    Front,
    /// Queued behind existing Front holder(s). The dispatch is recorded but
    /// MUST NOT trigger agent delivery, binding, branch creation, CI watch,
    /// review assignment, or dispatch tracking.
    Queued { ahead_of: Vec<String> },
    /// Structural refusal — metadata is incomplete, mismatching, or board
    /// state is ambiguous. Zero metadata writes on refusal.
    Refuse { reason: String },
}

/// The resolved domain key used for train locking. When the caller supplies
/// `None` for domain, the fallback `__repo__` key is used so that the
/// entire repository is treated as a single serialization domain.
pub(crate) const DOMAIN_FALLBACK: &str = "__repo__";

/// Metadata keys written to / read from task metadata for train membership.
pub(crate) const META_TRAIN_REPO: &str = "merge_train_repository";
pub(crate) const META_TRAIN_DOMAIN: &str = "merge_train_domain";
pub(crate) const META_TRAIN_POSITION: &str = "merge_train_position";

/// Evaluate merge-train admission for a task dispatch.
///
/// Scans all project boards for in-flight tasks (status ∈ {open, claimed,
/// in_progress, in_review, blocked}) whose `merge_train_repository` and
/// `merge_train_domain` metadata match the candidate. Returns:
///
/// - `Front` if no other in-flight task holds the same repo+domain lock.
/// - `Queued` if one or more tasks already hold the lock (lists their ids).
/// - `Refuse` if the candidate's metadata is incomplete/mismatching or the
///   board state is ambiguous (e.g. replay error).
pub(crate) fn evaluate(
    home: &Path,
    task_id: &str,
    repository: &str,
    domain: Option<&str>,
) -> MergeTrainAdmission {
    let _ = (home, task_id, repository, domain);
    todo!("merge train admission — GREEN phase")
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_events::{append_batch, InstanceName, TaskEvent, TaskId};
    use serde_json::json;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agend-test-merge-train-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fleet_test_guard() -> parking_lot::MutexGuard<'static, ()> {
        crate::mcp::handlers::fleet_test_guard()
    }

    fn write_fleet_yaml(home: &std::path::Path) {
        std::fs::write(
            crate::fleet::fleet_yaml_path(home),
            "instances:\n  orchestrator:\n    backend: claude\n  dev-a:\n    backend: claude\n  dev-b:\n    backend: claude\n",
        )
        .unwrap();
    }

    fn create_task_with_train_metadata(
        home: &std::path::Path,
        task_id: &str,
        assignee: &str,
        repo: &str,
        domain: Option<&str>,
        branch: &str,
    ) {
        let sender = InstanceName::from("orchestrator");
        let tid = TaskId(task_id.to_string());
        let mut events = vec![
            TaskEvent::Created {
                task_id: tid.clone(),
                title: format!("task {task_id}"),
                description: String::new(),
                priority: "normal".into(),
                owner: Some(InstanceName::from(assignee)),
                due_at: None,
                depends_on: vec![],
                routed_to: None,
                branch: Some(branch.into()),
                bind: Some(true),
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            },
            TaskEvent::MetadataSet {
                task_id: tid.clone(),
                by: sender.clone(),
                key: META_TRAIN_REPO.into(),
                value: json!(repo),
            },
        ];
        if let Some(d) = domain {
            events.push(TaskEvent::MetadataSet {
                task_id: tid.clone(),
                by: sender.clone(),
                key: META_TRAIN_DOMAIN.into(),
                value: json!(d),
            });
        }
        append_batch(home, &sender, events).unwrap();
    }

    fn create_task_on_board(
        home: &std::path::Path,
        board_project: &str,
        task_id: &str,
        assignee: &str,
        repo: &str,
        domain: Option<&str>,
        branch: &str,
    ) {
        let sender = InstanceName::from("orchestrator");
        let tid = TaskId(task_id.to_string());
        let board = crate::task_events::board_root(home, board_project);
        std::fs::create_dir_all(&board).unwrap();
        let mut events = vec![
            TaskEvent::Created {
                task_id: tid.clone(),
                title: format!("task {task_id}"),
                description: String::new(),
                priority: "normal".into(),
                owner: Some(InstanceName::from(assignee)),
                due_at: None,
                depends_on: vec![],
                routed_to: None,
                branch: Some(branch.into()),
                bind: Some(true),
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            },
            TaskEvent::MetadataSet {
                task_id: tid.clone(),
                by: sender.clone(),
                key: META_TRAIN_REPO.into(),
                value: json!(repo),
            },
        ];
        if let Some(d) = domain {
            events.push(TaskEvent::MetadataSet {
                task_id: tid.clone(),
                by: sender.clone(),
                key: META_TRAIN_DOMAIN.into(),
                value: json!(d),
            });
        }
        crate::task_events::append_batch_at(&board, &sender, events).unwrap();
    }

    // ────────────────────────────────────────────────────────────────────────
    // Group A: Concurrent same repository+domain across boards
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn a1_concurrent_same_repo_domain_one_front_one_queued() {
        let _g = fleet_test_guard();
        let home = tmp_home("a1-front-queued");
        write_fleet_yaml(&home);
        std::env::set_var("AGEND_HOME", &home);

        create_task_on_board(
            &home,
            "project-alpha",
            "t-a1-first",
            "dev-a",
            "acme/web",
            Some("auth"),
            "feat/auth-1",
        );
        create_task_on_board(
            &home,
            "project-beta",
            "t-a1-second",
            "dev-b",
            "acme/web",
            Some("auth"),
            "feat/auth-2",
        );

        let first = evaluate(&home, "t-a1-first", "acme/web", Some("auth"));
        let second = evaluate(&home, "t-a1-second", "acme/web", Some("auth"));

        let (front_count, queued_count) =
            [&first, &second]
                .iter()
                .fold((0, 0), |(f, q), adm| match adm {
                    MergeTrainAdmission::Front => (f + 1, q),
                    MergeTrainAdmission::Queued { .. } => (f, q + 1),
                    MergeTrainAdmission::Refuse { reason } => {
                        panic!("unexpected Refuse for valid metadata: {reason}");
                    }
                });
        assert_eq!(front_count, 1, "exactly one task must be Front");
        assert_eq!(queued_count, 1, "exactly one task must be Queued");

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a2_durable_after_reload() {
        let _g = fleet_test_guard();
        let home = tmp_home("a2-reload");
        write_fleet_yaml(&home);
        std::env::set_var("AGEND_HOME", &home);

        create_task_on_board(
            &home,
            "project-alpha",
            "t-a2-first",
            "dev-a",
            "acme/web",
            Some("payments"),
            "feat/pay-1",
        );
        create_task_on_board(
            &home,
            "project-beta",
            "t-a2-second",
            "dev-b",
            "acme/web",
            Some("payments"),
            "feat/pay-2",
        );

        let first_run = evaluate(&home, "t-a2-first", "acme/web", Some("payments"));
        let second_run = evaluate(&home, "t-a2-second", "acme/web", Some("payments"));

        // "Reload" — re-evaluate from cold (same on-disk state, fresh call).
        let first_reload = evaluate(&home, "t-a2-first", "acme/web", Some("payments"));
        let second_reload = evaluate(&home, "t-a2-second", "acme/web", Some("payments"));

        assert_eq!(
            first_run, first_reload,
            "Front/Queued must be durable across reload"
        );
        assert_eq!(
            second_run, second_reload,
            "Front/Queued must be durable across reload"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a3_same_task_readmission_idempotent() {
        let _g = fleet_test_guard();
        let home = tmp_home("a3-idempotent");
        write_fleet_yaml(&home);
        std::env::set_var("AGEND_HOME", &home);

        create_task_on_board(
            &home,
            "default",
            "t-a3-solo",
            "dev-a",
            "acme/api",
            Some("billing"),
            "feat/bill",
        );

        let first_eval = evaluate(&home, "t-a3-solo", "acme/api", Some("billing"));
        assert_eq!(
            first_eval,
            MergeTrainAdmission::Front,
            "solo task must be Front"
        );

        // Count events before re-admission.
        let board = crate::task_events::board_root(&home, "default");
        let event_log = board.join("event-log.jsonl");
        let lines_before = std::fs::read_to_string(&event_log)
            .unwrap_or_default()
            .lines()
            .count();

        // Re-admit — must return same result with no extra events.
        let re_eval = evaluate(&home, "t-a3-solo", "acme/api", Some("billing"));
        assert_eq!(
            re_eval,
            MergeTrainAdmission::Front,
            "re-admission must be idempotent"
        );

        let lines_after = std::fs::read_to_string(&event_log)
            .unwrap_or_default()
            .lines()
            .count();
        assert_eq!(
            lines_before, lines_after,
            "re-admission must not emit extra events (before={lines_before}, after={lines_after})"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    // ────────────────────────────────────────────────────────────────────────
    // Group B: Queued suppresses side effects + disjoint/fallback cases
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn b1_queued_admission_has_ahead_of_ids() {
        let _g = fleet_test_guard();
        let home = tmp_home("b1-ahead");
        write_fleet_yaml(&home);
        std::env::set_var("AGEND_HOME", &home);

        create_task_on_board(
            &home,
            "default",
            "t-b1-front",
            "dev-a",
            "acme/svc",
            Some("core"),
            "feat/core-1",
        );
        create_task_on_board(
            &home,
            "default",
            "t-b1-queue",
            "dev-b",
            "acme/svc",
            Some("core"),
            "feat/core-2",
        );

        let front = evaluate(&home, "t-b1-front", "acme/svc", Some("core"));
        let queued = evaluate(&home, "t-b1-queue", "acme/svc", Some("core"));

        assert_eq!(front, MergeTrainAdmission::Front);
        match queued {
            MergeTrainAdmission::Queued { ref ahead_of } => {
                assert!(
                    ahead_of.contains(&"t-b1-front".to_string()),
                    "Queued.ahead_of must contain the Front task id, got: {ahead_of:?}"
                );
            }
            other => panic!("expected Queued, got {other:?}"),
        }

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn b2_disjoint_repository_both_front() {
        let _g = fleet_test_guard();
        let home = tmp_home("b2-disjoint-repo");
        write_fleet_yaml(&home);
        std::env::set_var("AGEND_HOME", &home);

        create_task_on_board(
            &home,
            "default",
            "t-b2-repo-a",
            "dev-a",
            "acme/web",
            Some("auth"),
            "feat/web-auth",
        );
        create_task_on_board(
            &home,
            "default",
            "t-b2-repo-b",
            "dev-b",
            "acme/api",
            Some("auth"),
            "feat/api-auth",
        );

        let a = evaluate(&home, "t-b2-repo-a", "acme/web", Some("auth"));
        let b = evaluate(&home, "t-b2-repo-b", "acme/api", Some("auth"));

        assert_eq!(
            a,
            MergeTrainAdmission::Front,
            "disjoint repos must both be Front"
        );
        assert_eq!(
            b,
            MergeTrainAdmission::Front,
            "disjoint repos must both be Front"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn b3_disjoint_domain_same_repo_both_front() {
        let _g = fleet_test_guard();
        let home = tmp_home("b3-disjoint-domain");
        write_fleet_yaml(&home);
        std::env::set_var("AGEND_HOME", &home);

        create_task_on_board(
            &home,
            "default",
            "t-b3-dom-a",
            "dev-a",
            "acme/web",
            Some("auth"),
            "feat/auth",
        );
        create_task_on_board(
            &home,
            "default",
            "t-b3-dom-b",
            "dev-b",
            "acme/web",
            Some("payments"),
            "feat/pay",
        );

        let a = evaluate(&home, "t-b3-dom-a", "acme/web", Some("auth"));
        let b = evaluate(&home, "t-b3-dom-b", "acme/web", Some("payments"));

        assert_eq!(
            a,
            MergeTrainAdmission::Front,
            "disjoint domains must both be Front"
        );
        assert_eq!(
            b,
            MergeTrainAdmission::Front,
            "disjoint domains must both be Front"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn b4_absent_domain_uses_repo_fallback() {
        let _g = fleet_test_guard();
        let home = tmp_home("b4-fallback");
        write_fleet_yaml(&home);
        std::env::set_var("AGEND_HOME", &home);

        // First task: no domain (None → __repo__ fallback).
        create_task_on_board(
            &home,
            "default",
            "t-b4-no-dom-1",
            "dev-a",
            "acme/web",
            None,
            "feat/web-1",
        );
        // Second task: also no domain, same repo → same lock.
        create_task_on_board(
            &home,
            "default",
            "t-b4-no-dom-2",
            "dev-b",
            "acme/web",
            None,
            "feat/web-2",
        );

        let first = evaluate(&home, "t-b4-no-dom-1", "acme/web", None);
        let second = evaluate(&home, "t-b4-no-dom-2", "acme/web", None);

        let (front_count, queued_count) =
            [&first, &second]
                .iter()
                .fold((0, 0), |(f, q), adm| match adm {
                    MergeTrainAdmission::Front => (f + 1, q),
                    MergeTrainAdmission::Queued { .. } => (f, q + 1),
                    _ => (f, q),
                });
        assert_eq!(front_count, 1, "absent domain: one Front");
        assert_eq!(
            queued_count, 1,
            "absent domain: one Queued (same repo ⇒ __repo__ lock)"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn b5_absent_domain_disjoint_from_explicit_domain() {
        let _g = fleet_test_guard();
        let home = tmp_home("b5-fallback-disjoint");
        write_fleet_yaml(&home);
        std::env::set_var("AGEND_HOME", &home);

        // Task with explicit domain.
        create_task_on_board(
            &home,
            "default",
            "t-b5-explicit",
            "dev-a",
            "acme/web",
            Some("auth"),
            "feat/auth",
        );
        // Task with no domain (→ __repo__ fallback) — different lock key.
        create_task_on_board(
            &home,
            "default",
            "t-b5-fallback",
            "dev-b",
            "acme/web",
            None,
            "feat/web",
        );

        let explicit = evaluate(&home, "t-b5-explicit", "acme/web", Some("auth"));
        let fallback = evaluate(&home, "t-b5-fallback", "acme/web", None);

        assert_eq!(
            explicit,
            MergeTrainAdmission::Front,
            "__repo__ fallback is disjoint from named domain"
        );
        assert_eq!(
            fallback,
            MergeTrainAdmission::Front,
            "__repo__ fallback is disjoint from named domain"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    // ────────────────────────────────────────────────────────────────────────
    // Group C: Structural refusal — bad metadata, ambiguous board
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn c1_missing_train_repository_metadata_refuses() {
        let _g = fleet_test_guard();
        let home = tmp_home("c1-no-repo");
        write_fleet_yaml(&home);
        std::env::set_var("AGEND_HOME", &home);

        // Create a task with NO merge_train_repository metadata.
        let sender = InstanceName::from("orchestrator");
        let tid = TaskId("t-c1-bare".into());
        append_batch(
            &home,
            &sender,
            vec![TaskEvent::Created {
                task_id: tid,
                title: "bare task".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: Some(InstanceName::from("dev-a")),
                due_at: None,
                depends_on: vec![],
                routed_to: None,
                branch: Some("feat/bare".into()),
                bind: Some(true),
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            }],
        )
        .unwrap();

        // Evaluate with repo arg but task has no matching metadata.
        let result = evaluate(&home, "t-c1-bare", "acme/web", Some("auth"));
        match result {
            MergeTrainAdmission::Refuse { ref reason } => {
                assert!(!reason.is_empty(), "Refuse must include a non-empty reason");
            }
            other => panic!("missing metadata must Refuse, got {other:?}"),
        }

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn c2_mismatching_repository_metadata_refuses() {
        let _g = fleet_test_guard();
        let home = tmp_home("c2-mismatch");
        write_fleet_yaml(&home);
        std::env::set_var("AGEND_HOME", &home);

        // Create task with merge_train_repository = "acme/api" (different from caller's "acme/web").
        create_task_on_board(
            &home,
            "default",
            "t-c2-mismatch",
            "dev-a",
            "acme/api",
            Some("auth"),
            "feat/auth",
        );

        let result = evaluate(&home, "t-c2-mismatch", "acme/web", Some("auth"));
        match result {
            MergeTrainAdmission::Refuse { ref reason } => {
                assert!(
                    reason.contains("mismatch") || reason.contains("inconsistent"),
                    "Refuse reason must indicate mismatch, got: {reason}"
                );
            }
            other => panic!("mismatching repository must Refuse, got {other:?}"),
        }

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn c3_mismatching_domain_metadata_refuses() {
        let _g = fleet_test_guard();
        let home = tmp_home("c3-domain-mismatch");
        write_fleet_yaml(&home);
        std::env::set_var("AGEND_HOME", &home);

        // Task metadata says domain="payments", caller says domain="auth".
        create_task_on_board(
            &home,
            "default",
            "t-c3-dom",
            "dev-a",
            "acme/web",
            Some("payments"),
            "feat/pay",
        );

        let result = evaluate(&home, "t-c3-dom", "acme/web", Some("auth"));
        match result {
            MergeTrainAdmission::Refuse { ref reason } => {
                assert!(
                    reason.contains("mismatch") || reason.contains("inconsistent"),
                    "Refuse reason must indicate domain mismatch, got: {reason}"
                );
            }
            other => panic!("mismatching domain must Refuse, got {other:?}"),
        }

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn c4_refusal_writes_zero_metadata() {
        let _g = fleet_test_guard();
        let home = tmp_home("c4-zero-writes");
        write_fleet_yaml(&home);
        std::env::set_var("AGEND_HOME", &home);

        // Bare task, no train metadata.
        let sender = InstanceName::from("orchestrator");
        let tid = TaskId("t-c4-bare".into());
        append_batch(
            &home,
            &sender,
            vec![TaskEvent::Created {
                task_id: tid,
                title: "bare".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: Some(InstanceName::from("dev-a")),
                due_at: None,
                depends_on: vec![],
                routed_to: None,
                branch: Some("feat/bare".into()),
                bind: Some(true),
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            }],
        )
        .unwrap();

        let event_log = crate::task_events::board_root(&home, "default").join("event-log.jsonl");
        let lines_before = std::fs::read_to_string(&event_log)
            .unwrap_or_default()
            .lines()
            .count();

        let result = evaluate(&home, "t-c4-bare", "acme/web", Some("auth"));
        assert!(
            matches!(result, MergeTrainAdmission::Refuse { .. }),
            "must Refuse for missing metadata"
        );

        let lines_after = std::fs::read_to_string(&event_log)
            .unwrap_or_default()
            .lines()
            .count();
        assert_eq!(
            lines_before, lines_after,
            "Refuse must not write any metadata events (before={lines_before}, after={lines_after})"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn c5_nonexistent_task_refuses() {
        let _g = fleet_test_guard();
        let home = tmp_home("c5-ghost");
        write_fleet_yaml(&home);
        std::env::set_var("AGEND_HOME", &home);

        let result = evaluate(&home, "t-c5-ghost", "acme/web", Some("auth"));
        match result {
            MergeTrainAdmission::Refuse { ref reason } => {
                assert!(
                    !reason.is_empty(),
                    "Refuse for nonexistent task must give reason"
                );
            }
            other => panic!("nonexistent task must Refuse, got {other:?}"),
        }

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }
}
