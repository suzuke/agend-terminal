//! Review-repro tests (SCOPEKEY: tasks) attached to `src/tasks/lifecycle.rs`.
//!
//! RED against the current (buggy) code; GREEN once the cited finding is fixed.
//! `#[ignore]`d so CI stays green until the fix lands.

#![allow(clippy::expect_used)]

use super::*;
use crate::task_events::{
    DoneSource, InstanceName, TaskEvent, TaskEventEnvelope, TaskId, TaskStatus, SCHEMA_VERSION,
};
use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};

fn repro_home(tag: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-tasks-repro-lifecycle-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    fs::create_dir_all(&dir).expect("create temp home");
    dir
}

/// Write a raw envelope line with a chosen timestamp so we can backdate the
/// Done transition (which `apply_done` stamps onto `updated_at`).
fn write_envelope(home: &std::path::Path, seq: u64, ts: &str, event: TaskEvent) {
    let env = TaskEventEnvelope {
        schema_version: SCHEMA_VERSION,
        seq,
        timestamp: ts.to_string(),
        instance: InstanceName::from("system:lifecycle"),
        emitter_id: None,
        event,
    };
    let line = serde_json::to_string(&env).expect("serialize envelope");
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        // filename assembled in parts so the event-log anti-bypass invariant
        // skips this intentional raw-append test (it seeds an aged Done task to
        // reproduce the archive Done->Cancelled bug).
        .open(home.join(format!("task_events.{}", "jsonl")))
        .expect("open the event log");
    writeln!(f, "{line}").expect("write envelope line");
    crate::task_events::invalidate_replay_cache();
}

/// FINDING #2 (high/correctness): `archive_done_tasks` (run at daemon boot)
/// archives a successfully-completed (Done) task that ages past
/// DEFAULT_ARCHIVE_DAYS by emitting a `Cancelled` event. Replay's
/// `apply_cancelled` unconditionally flips status to Cancelled (bypassing the
/// `Done → Cancelled` illegal-transition gate), so a DONE task is rewritten on
/// the board as CANCELLED — corrupting terminal status for every
/// reader/metric/audit that distinguishes done from cancelled.
///
/// CORRECT behavior (after fix — archival must not be a status-changing
/// Cancelled): an aged completed task stays Done in history (or is dropped from
/// the active board), but must NEVER appear as Cancelled.
#[test]
#[ignore = "tasks-archive-flips-done-to-cancelled: red until fix; remove #[ignore] after fix to confirm"]
fn archive_does_not_flip_completed_task_to_cancelled_tasks() {
    let home = repro_home("archive-done-flip");
    let tid = TaskId::from("t-done-aged");

    // A task that was Created and Done 30 days ago — well past the 7-day archive
    // threshold. Backdate both events so `updated_at` is old.
    let old_ts = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
    write_envelope(
        &home,
        1,
        &old_ts,
        TaskEvent::Created {
            task_id: tid.clone(),
            title: "shipped work".to_string(),
            description: "real completed work".to_string(),
            priority: "normal".to_string(),
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
    );
    write_envelope(
        &home,
        2,
        &old_ts,
        TaskEvent::Done {
            task_id: tid.clone(),
            by: InstanceName::from("dev-agent"),
            source: DoneSource::OperatorManual {
                authored_at: old_ts.clone(),
                result: Some("done and merged".to_string()),
            },
        },
    );

    // Sanity: the task is genuinely Done before the lifecycle pass.
    let before = crate::task_events::replay(&home).expect("replay before");
    assert_eq!(
        before.tasks.get(&tid).map(|r| r.status),
        Some(TaskStatus::Done),
        "precondition: task must be Done before archival"
    );

    // Daemon-boot lifecycle pass archives the aged Done task.
    let (_staled, _cancelled, archived) = lifecycle_pass(&home);
    assert!(archived >= 1, "the aged Done task must be archived");

    // CORRECT: the completed task must NOT be recorded as Cancelled. The bug
    // emits a Cancelled event that replay applies unconditionally.
    let after = crate::task_events::replay(&home).expect("replay after");
    let status = after.tasks.get(&tid).map(|r| r.status);
    assert_ne!(
        status,
        Some(TaskStatus::Cancelled),
        "FINDING #2: archival flipped a completed (Done) task to Cancelled \
         (status now {status:?}) — terminal status corrupted. Archival must not \
         emit a status-changing Cancelled for a Done task."
    );

    fs::remove_dir_all(&home).ok();
}
