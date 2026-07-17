#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn tmp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agend-wt-reconcile-order-{tag}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup_test_repo(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agend-wt-reconcile-order-repo-{tag}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    git_in(&dir, &["init", "-b", "main"]);
    git_in(
        &dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ],
    );
    dir
}

fn git_in(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn hygiene_tasks(home: &Path) -> Vec<(String, serde_json::Value)> {
    let tasks = crate::task_events::replay(home);
    let Ok(board) = tasks else { return vec![] };
    board
        .tasks
        .values()
        .filter_map(|t| {
            let key = t
                .metadata
                .get(crate::daemon::hygiene_task::ALERT_KEY_META)?
                .as_str()?;
            Some((key.to_string(), serde_json::to_value(t).ok()?))
        })
        .collect()
}

/// RED: review_branch_reconciled must precede any fetch-degraded hygiene
/// event produced by the same sweep. On current code, reconcile runs AFTER
/// the per-repo loop, so the hygiene task's created_at is earlier.
#[test]
#[cfg(unix)]
fn reconcile_runs_before_fetch_loop_ordering() {
    static ENV_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    let _lock = ENV_LOCK.lock();
    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    let home = tmp_home("reconcile-order");
    let repo = setup_test_repo("reconcile-order-repo");
    git_in(
        &repo,
        &["remote", "add", "origin", "/nonexistent/order-test-fixture"],
    );
    git_in(&repo, &["checkout", "-b", "review/pr-order-test"]);
    std::fs::write(repo.join("f.txt"), "work").ok();
    git_in(&repo, &["add", "."]);
    git_in(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "work",
        ],
    );
    let tip = crate::git_helpers::git_cmd(&repo, &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    git_in(&repo, &["checkout", "main"]);
    crate::cleanup_intents::persist_intent(
        &home,
        &repo.display().to_string(),
        "review/pr-order-test",
        &tip,
        "t-order-test",
        None,
        None,
    )
    .unwrap();
    {
        use crate::task_events::{InstanceName, TaskEvent, TaskId};
        let tid = TaskId("t-order-test".into());
        let board = crate::task_events::board_root(&home, crate::task_events::DEFAULT_PROJECT);
        std::fs::create_dir_all(&board).ok();
        let emitter = InstanceName::from("test");
        let _ = crate::task_events::append(
            &board,
            &emitter,
            TaskEvent::Created {
                task_id: tid.clone(),
                title: "order test".into(),
                description: String::new(),
                priority: "normal".into(),
                tags: Vec::new(),
                owner: None,
                depends_on: Vec::new(),
                parent_id: None,
                branch: None,
                due_at: None,
                routed_to: None,
                bind: None,
                eta_secs: None,
            },
        );
        let _ = crate::task_events::append(
            &board,
            &emitter,
            TaskEvent::Done {
                task_id: tid,
                by: emitter.clone(),
                source: crate::task_events::DoneSource::ReportAutoClose {
                    report_summary: "done".into(),
                    closed_at: chrono::Utc::now().to_rfc3339(),
                },
            },
        );
    }
    let configs = HashMap::new();
    let _ = sweep_from_registry(&home, &configs, &[]);
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");

    let event_log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    let reconcile_line = event_log
        .lines()
        .position(|l| l.contains("review_branch_reconciled"));
    let hygiene = hygiene_tasks(&home);
    let fetch_degraded = hygiene.iter().find(|(k, _)| k.contains("fetch-degraded"));

    assert!(
        reconcile_line.is_some(),
        "review_branch_reconciled must appear in event log"
    );
    assert!(
        fetch_degraded.is_some(),
        "fetch-degraded hygiene task must be created"
    );
    let reconcile_ts: String = event_log
        .lines()
        .find(|l| l.contains("review_branch_reconciled"))
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .and_then(|v| v["timestamp"].as_str().map(String::from))
        .expect("reconcile event must have timestamp");
    let task_list = crate::task_events::replay(&home).expect("replay tasks");
    let hygiene_ts = task_list
        .tasks
        .values()
        .filter_map(|t| {
            let key = t
                .metadata
                .get(crate::daemon::hygiene_task::ALERT_KEY_META)?
                .as_str()?;
            if key.contains("fetch-degraded") {
                Some(t.created_at.clone())
            } else {
                None
            }
        })
        .next()
        .expect("fetch-degraded task must have created_at");

    assert!(
        reconcile_ts <= hygiene_ts,
        "RED: review_branch_reconciled ({reconcile_ts}) must precede \
         fetch-degraded hygiene task ({hygiene_ts})"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}
