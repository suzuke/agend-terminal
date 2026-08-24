//! Bounded task-state projection for the catalog shadow path.
//!
//! This first P2 slice deliberately has no authority: it projects an incumbent
//! replay result into O(tasks) state. Incremental writes and catalog-backed
//! reads land separately so this representation can be reviewed in isolation.

// This module is intentionally wired into production in the next P2 slice.
// Keeping the allow local avoids fake call sites whose only purpose is lint
// suppression while this independently reviewable representation lands.
#![allow(dead_code)]

use super::{HistoryEntry, InstanceName, PrId, TaskBoardState, TaskId, TaskRecord, TaskStatus};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};

/// Enough recent activity for the task-board detail view without retaining an
/// unbounded audit timeline in memory.
pub const RECENT_HISTORY_LIMIT: usize = 16;

/// Current task state stored by the catalog. Unlike [`TaskRecord`], this type
/// is bounded with respect to the number of events applied to a task.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectedTaskRecord {
    pub id: TaskId,
    pub title: String,
    pub description: String,
    pub priority: String,
    pub status: TaskStatus,
    pub owner: Option<InstanceName>,
    pub linked_prs: Vec<PrId>,
    pub block_reason: Option<String>,
    pub created_by: InstanceName,
    pub created_at: String,
    pub updated_at: String,
    pub due_at: Option<String>,
    pub depends_on: Vec<TaskId>,
    pub routed_to: Option<InstanceName>,
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<TaskId>,
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
    pub history_len: u64,
    pub last_seq: Option<(InstanceName, u64)>,
    pub recent_history: VecDeque<HistoryEntry>,
}

impl From<TaskRecord> for ProjectedTaskRecord {
    fn from(task: TaskRecord) -> Self {
        let history_len = task.history.len() as u64;
        let last_seq = task
            .history
            .last()
            .map(|entry| (entry.instance.clone(), entry.seq));
        let recent_history = task
            .history
            .into_iter()
            .rev()
            .take(RECENT_HISTORY_LIMIT)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        Self {
            id: task.id,
            title: task.title,
            description: task.description,
            priority: task.priority,
            status: task.status,
            owner: task.owner,
            linked_prs: task.linked_prs,
            block_reason: task.block_reason,
            created_by: task.created_by,
            created_at: task.created_at,
            updated_at: task.updated_at,
            due_at: task.due_at,
            depends_on: task.depends_on,
            routed_to: task.routed_to,
            result: task.result,
            superseded_by: task.superseded_by,
            branch: task.branch,
            bind: task.bind,
            started_at: task.started_at,
            eta_secs: task.eta_secs,
            tags: task.tags,
            parent_id: task.parent_id,
            metadata: task.metadata,
            history_len,
            last_seq,
            recent_history,
        }
    }
}

/// One board's bounded shadow snapshot, built from the incumbent replay.
#[derive(Clone, Debug, Default, Serialize)]
pub struct BoardProjection {
    tasks: BTreeMap<TaskId, ProjectedTaskRecord>,
    last_seq_per_instance: BTreeMap<InstanceName, u64>,
    events_folded: u64,
}

impl BoardProjection {
    pub fn from_replay(state: TaskBoardState) -> Self {
        Self {
            tasks: state
                .tasks
                .into_iter()
                .map(|(id, task)| (id, task.into()))
                .collect(),
            last_seq_per_instance: state.last_seq_per_instance,
            events_folded: state.events_folded,
        }
    }

    pub fn task(&self, id: &TaskId) -> Option<&ProjectedTaskRecord> {
        self.tasks.get(id)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &ProjectedTaskRecord> {
        self.tasks.values()
    }

    pub fn last_seq_for(&self, instance: &InstanceName) -> Option<u64> {
        self.last_seq_per_instance.get(instance).copied()
    }

    pub fn events_folded(&self) -> u64 {
        self.events_folded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_events::{TaskEvent, TaskEventEnvelope, SCHEMA_VERSION};

    fn envelope(seq: u64, event: TaskEvent) -> TaskEventEnvelope {
        TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq,
            timestamp: format!("2026-08-24T00:00:{:02}Z", seq % 60),
            instance: InstanceName::from("writer"),
            emitter_id: None,
            event,
        }
    }

    fn replay_with_metadata_events(count: u64) -> TaskBoardState {
        let task_id = TaskId::from("t-20260824000000000000-1-1");
        let mut state = TaskBoardState::default();
        assert!(state.apply(&envelope(
            1,
            TaskEvent::Created {
                task_id: task_id.clone(),
                title: "bounded projection".into(),
                description: "parity fixture".into(),
                priority: "high".into(),
                owner: Some(InstanceName::from("owner")),
                due_at: Some("2026-08-25T00:00:00Z".into()),
                depends_on: vec![TaskId::from("t-20260823000000000000-1-1")],
                routed_to: Some(InstanceName::from("lead")),
                branch: Some("fix/catalog".into()),
                bind: Some(true),
                eta_secs: Some(60),
                tags: vec!["catalog".into()],
                parent_id: None,
            },
        )));
        for seq in 2..=count + 1 {
            assert!(state.apply(&envelope(
                seq,
                TaskEvent::MetadataSet {
                    task_id: task_id.clone(),
                    by: InstanceName::from("writer"),
                    key: "stable".into(),
                    value: serde_json::json!(true),
                },
            )));
        }
        state
    }

    #[test]
    fn projection_preserves_current_state_and_replay_high_water() {
        let task_id = TaskId::from("t-20260824000000000000-1-1");
        let replay = replay_with_metadata_events(20);
        let mut incumbent =
            serde_json::to_value(replay.tasks.get(&task_id).expect("replayed task"))
                .expect("serialize replayed task");
        incumbent
            .as_object_mut()
            .expect("task object")
            .remove("history");

        let projection = BoardProjection::from_replay(replay);
        let task = projection.task(&task_id).expect("projected task");
        let mut projected = serde_json::to_value(task).expect("serialize projected task");
        let projected_object = projected.as_object_mut().expect("projected task object");
        projected_object.remove("history_len");
        projected_object.remove("last_seq");
        projected_object.remove("recent_history");

        assert_eq!(projected, incumbent, "every current-state field must match");
        assert_eq!(task.history_len, 21);
        assert_eq!(task.recent_history.len(), RECENT_HISTORY_LIMIT);
        assert_eq!(task.recent_history.front().map(|entry| entry.seq), Some(6));
        assert_eq!(task.last_seq, Some((InstanceName::from("writer"), 21)));
        assert_eq!(
            projection.last_seq_for(&InstanceName::from("writer")),
            Some(21)
        );
        assert_eq!(projection.events_folded(), 21);
        assert_eq!(projection.tasks().count(), 1);
    }

    #[test]
    fn projection_size_is_bounded_by_tasks_not_events() {
        let one_x = BoardProjection::from_replay(replay_with_metadata_events(100));
        let ten_x = BoardProjection::from_replay(replay_with_metadata_events(1_000));
        let one_x_bytes = serde_json::to_vec(&one_x).expect("serialize 1x projection");
        let ten_x_bytes = serde_json::to_vec(&ten_x).expect("serialize 10x projection");

        assert_eq!(one_x.tasks().count(), ten_x.tasks().count());
        assert_eq!(
            one_x.tasks().next().expect("1x task").recent_history.len(),
            RECENT_HISTORY_LIMIT
        );
        assert_eq!(
            ten_x.tasks().next().expect("10x task").recent_history.len(),
            RECENT_HISTORY_LIMIT
        );
        assert!(
            ten_x_bytes.len() <= one_x_bytes.len() + 32,
            "10x events grew bounded projection from {} to {} bytes",
            one_x_bytes.len(),
            ten_x_bytes.len()
        );
    }
}
