//! Bounded task-state projection for the catalog shadow path.
//!
//! This P2 shadow deliberately has no authority: it projects an incumbent
//! replay result into O(tasks) state and can fold new envelopes incrementally.
//! Catalog-backed reads and writer migration land separately.

// This module is intentionally wired into production in the next P2 slice.
// Keeping the allow local avoids fake call sites whose only purpose is lint
// suppression while this independently reviewable representation lands.
#![allow(dead_code)]

use super::{
    DoneSource, HistoryEntry, InstanceName, PrId, TaskBoardState, TaskEvent, TaskEventEnvelope,
    TaskId, TaskRecord, TaskStatus,
};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};

/// Enough recent activity for the task-board detail view without retaining an
/// unbounded audit timeline in memory.
pub const RECENT_HISTORY_LIMIT: usize = 16;

/// Canonical within-file fold order used by the incumbent replay.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct OrderKey {
    timestamp_ns: i64,
    instance: InstanceName,
    seq: u64,
}

impl OrderKey {
    fn from_envelope(env: &TaskEventEnvelope) -> Result<Self, OrderedApplyError> {
        let timestamp_ns = chrono::DateTime::parse_from_rfc3339(&env.timestamp)
            .map_err(|_| OrderedApplyError::InvalidTimestamp(env.timestamp.clone()))?
            .timestamp_nanos_opt()
            .unwrap_or(0);
        Ok(Self {
            timestamp_ns,
            instance: env.instance.clone(),
            seq: env.seq,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OrderedApplyError {
    InvalidTimestamp(String),
    OutOfOrder {
        previous: OrderKey,
        received: OrderKey,
    },
}

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
    #[serde(alias = "dispatched_at", skip_serializing_if = "Option::is_none")]
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
    pub last_folded_event: Option<(InstanceName, u64)>,
    pub recent_history: VecDeque<HistoryEntry>,
}

impl From<TaskRecord> for ProjectedTaskRecord {
    fn from(task: TaskRecord) -> Self {
        let history_len = task.history.len() as u64;
        let last_folded_event = task
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
            last_folded_event,
            recent_history,
        }
    }
}

/// One board's bounded shadow snapshot, built from the incumbent replay.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct BoardProjection {
    tasks: BTreeMap<TaskId, ProjectedTaskRecord>,
    last_seq_per_instance: BTreeMap<InstanceName, u64>,
    events_folded: u64,
    last_order_key: Option<OrderKey>,
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
            last_order_key: None,
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

    pub fn last_order_key(&self) -> Option<&OrderKey> {
        self.last_order_key.as_ref()
    }

    /// Apply one live-tail envelope only when its canonical key advances.
    /// Replay/rebuild callers sort first and continue to use [`Self::apply`].
    pub fn apply_ordered(&mut self, env: &TaskEventEnvelope) -> Result<bool, OrderedApplyError> {
        let received = OrderKey::from_envelope(env)?;
        if let Some(previous) = &self.last_order_key {
            if received <= *previous {
                return Err(OrderedApplyError::OutOfOrder {
                    previous: previous.clone(),
                    received,
                });
            }
        }

        let applied = self.apply(env);
        self.last_order_key = Some(received);
        Ok(applied)
    }

    /// Fold one canonical envelope into the bounded projection. Returns false
    /// when this emitter sequence was already observed.
    pub fn apply(&mut self, env: &TaskEventEnvelope) -> bool {
        let previous = self
            .last_seq_per_instance
            .get(&env.instance)
            .copied()
            .unwrap_or(0);
        if env.seq <= previous {
            return false;
        }

        self.last_seq_per_instance
            .insert(env.instance.clone(), env.seq);
        self.events_folded += 1;

        let task_id = env.event.task_id().clone();
        self.apply_event(&env.event, &task_id, &env.instance, &env.timestamp);

        if let Some(task) = self.tasks.get_mut(&task_id) {
            task.history_len += 1;
            task.last_folded_event = Some((env.instance.clone(), env.seq));
            task.recent_history.push_back(HistoryEntry {
                seq: env.seq,
                timestamp: env.timestamp.clone(),
                instance: env.instance.clone(),
                kind: env.event.kind_str(),
            });
            if task.recent_history.len() > RECENT_HISTORY_LIMIT {
                task.recent_history.pop_front();
            }
        }
        true
    }

    fn apply_event(
        &mut self,
        event: &TaskEvent,
        task_id: &TaskId,
        instance: &InstanceName,
        timestamp: &str,
    ) {
        match event {
            TaskEvent::Created {
                title,
                description,
                priority,
                owner,
                due_at,
                depends_on,
                routed_to,
                branch,
                bind,
                eta_secs,
                tags,
                parent_id,
                ..
            } => {
                self.tasks
                    .entry(task_id.clone())
                    .or_insert_with(|| ProjectedTaskRecord {
                        id: task_id.clone(),
                        title: title.clone(),
                        description: description.clone(),
                        priority: priority.clone(),
                        status: TaskStatus::Open,
                        owner: owner.clone(),
                        linked_prs: Vec::new(),
                        block_reason: None,
                        created_by: instance.clone(),
                        created_at: timestamp.to_string(),
                        updated_at: timestamp.to_string(),
                        due_at: due_at.clone(),
                        depends_on: depends_on.clone(),
                        routed_to: routed_to.clone(),
                        result: None,
                        superseded_by: None,
                        branch: branch.clone(),
                        bind: *bind,
                        started_at: None,
                        eta_secs: *eta_secs,
                        tags: tags.clone(),
                        parent_id: parent_id.clone(),
                        metadata: BTreeMap::new(),
                        history_len: 0,
                        last_folded_event: None,
                        recent_history: VecDeque::new(),
                    });
            }
            TaskEvent::Cancelled { .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.status = TaskStatus::Cancelled;
                    task.updated_at = timestamp.to_string();
                }
                for child in self.tasks.values_mut().filter(|task| {
                    task.parent_id.as_ref() == Some(task_id)
                        && matches!(task.status, TaskStatus::Open | TaskStatus::Claimed)
                }) {
                    child.status = TaskStatus::Cancelled;
                    child.updated_at = timestamp.to_string();
                }
            }
            TaskEvent::Claimed { by, .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.status = TaskStatus::Claimed;
                    task.owner = Some(by.clone());
                    task.routed_to = None;
                    task.updated_at = timestamp.to_string();
                }
            }
            TaskEvent::InProgress { by, .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.status = TaskStatus::InProgress;
                    task.owner = Some(by.clone());
                    task.updated_at = timestamp.to_string();
                    if task.started_at.is_none() {
                        task.started_at = Some(timestamp.to_string());
                    }
                }
            }
            TaskEvent::Verified { .. } => {
                self.set_status(task_id, timestamp, TaskStatus::Verified);
            }
            TaskEvent::Done { source, .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.status = TaskStatus::Done;
                    task.updated_at = timestamp.to_string();
                    match source {
                        DoneSource::OperatorManual { result, .. } => task.result = result.clone(),
                        DoneSource::ReportAutoClose { report_summary, .. }
                            if task.result.is_none() =>
                        {
                            task.result = Some(report_summary.clone());
                        }
                        _ => {}
                    }
                }
            }
            TaskEvent::Superseded { successor_id, .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.status = TaskStatus::Superseded;
                    task.result = Some(format!("Superseded by {}", successor_id.0));
                    task.superseded_by = Some(successor_id.clone());
                    task.updated_at = timestamp.to_string();
                }
            }
            TaskEvent::Linked { pr_id, .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    if !task.linked_prs.contains(pr_id) {
                        task.linked_prs.push(*pr_id);
                    }
                    task.updated_at = timestamp.to_string();
                }
            }
            TaskEvent::Blocked { reason, .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.status = TaskStatus::Blocked;
                    task.block_reason = Some(reason.clone());
                    task.updated_at = timestamp.to_string();
                }
            }
            TaskEvent::Unblocked { .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    if task.status == TaskStatus::Blocked {
                        task.status = TaskStatus::Open;
                    }
                    task.block_reason = None;
                    task.updated_at = timestamp.to_string();
                }
            }
            TaskEvent::Reopened { .. } => self.set_status(task_id, timestamp, TaskStatus::Open),
            TaskEvent::Released { .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.status = TaskStatus::Open;
                    task.owner = None;
                    task.routed_to = None;
                    task.updated_at = timestamp.to_string();
                }
            }
            TaskEvent::MovedToBacklog { .. } => {
                self.set_status(task_id, timestamp, TaskStatus::Backlog);
            }
            TaskEvent::MovedToReview { .. } => {
                self.set_status(task_id, timestamp, TaskStatus::InReview);
            }
            TaskEvent::TaskCloseProposed { .. } => self.touch(task_id, timestamp),
            TaskEvent::OwnerAssigned {
                owner, routed_to, ..
            } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.owner = owner.clone();
                    task.routed_to = routed_to.clone();
                    task.updated_at = timestamp.to_string();
                }
            }
            TaskEvent::PriorityChanged { priority, .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.priority = priority.clone();
                    task.updated_at = timestamp.to_string();
                }
            }
            TaskEvent::DescriptionUpdated { description, .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.description = description.clone();
                    task.updated_at = timestamp.to_string();
                }
            }
            TaskEvent::TagsSet { tags, .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.tags = tags.clone();
                    task.updated_at = timestamp.to_string();
                }
            }
            TaskEvent::ResultSet { result, .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.result = Some(result.clone());
                    task.updated_at = timestamp.to_string();
                }
            }
            TaskEvent::MetadataSet { key, value, .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.metadata.insert(key.clone(), value.clone());
                    task.updated_at = timestamp.to_string();
                }
            }
            TaskEvent::BranchLinked { branch, .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.branch = Some(branch.clone());
                    task.updated_at = timestamp.to_string();
                }
            }
        }
    }

    fn set_status(&mut self, task_id: &TaskId, timestamp: &str, status: TaskStatus) {
        if let Some(task) = self.tasks.get_mut(task_id) {
            task.status = status;
            task.updated_at = timestamp.to_string();
        }
    }

    fn touch(&mut self, task_id: &TaskId, timestamp: &str) {
        if let Some(task) = self.tasks.get_mut(task_id) {
            task.updated_at = timestamp.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_events::{
        ConfidenceScore, LinkSource, PrSnapshot, TaskEvent, TaskEventEnvelope, SCHEMA_VERSION,
    };

    fn envelope(seq: u64, event: TaskEvent) -> TaskEventEnvelope {
        envelope_from("writer", seq, event)
    }

    fn envelope_from(instance: &str, seq: u64, event: TaskEvent) -> TaskEventEnvelope {
        TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq,
            timestamp: format!("2026-08-24T00:00:{:02}Z", seq % 60),
            instance: InstanceName::from(instance),
            emitter_id: None,
            event,
        }
    }

    fn envelope_at(timestamp: &str, seq: u64, event: TaskEvent) -> TaskEventEnvelope {
        let mut env = envelope(seq, event);
        env.timestamp = timestamp.to_string();
        env
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
        projected_object.remove("last_folded_event");
        projected_object.remove("recent_history");

        assert_eq!(projected, incumbent, "every current-state field must match");
        assert_eq!(task.history_len, 21);
        assert_eq!(task.recent_history.len(), RECENT_HISTORY_LIMIT);
        assert_eq!(task.recent_history.front().map(|entry| entry.seq), Some(6));
        assert_eq!(
            task.last_folded_event,
            Some((InstanceName::from("writer"), 21))
        );
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

    #[test]
    fn incremental_apply_matches_incumbent_replay() {
        let task_id = TaskId::from("t-20260824000000000000-1-1");
        let child_id = TaskId::from("t-20260824000000000000-1-2");
        let actor = InstanceName::from("owner");
        let snapshot = PrSnapshot {
            pr_state: "merged".into(),
            merge_sha: Some("abc123".into()),
            api_response_hash: "hash".into(),
            captured_at: "2026-08-24T00:00:00Z".into(),
        };
        let events = vec![
            TaskEvent::Created {
                task_id: task_id.clone(),
                title: "parent".into(),
                description: "original".into(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: Some(InstanceName::from("lead")),
                branch: None,
                bind: Some(true),
                eta_secs: Some(60),
                tags: Vec::new(),
                parent_id: None,
            },
            TaskEvent::Created {
                task_id: child_id.clone(),
                title: "child".into(),
                description: "cascade fixture".into(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: Vec::new(),
                parent_id: Some(task_id.clone()),
            },
            TaskEvent::Claimed {
                task_id: task_id.clone(),
                by: actor.clone(),
            },
            TaskEvent::InProgress {
                task_id: task_id.clone(),
                by: actor.clone(),
            },
            TaskEvent::Blocked {
                task_id: task_id.clone(),
                reason: "waiting".into(),
            },
            TaskEvent::Unblocked {
                task_id: task_id.clone(),
            },
            TaskEvent::MovedToBacklog {
                task_id: task_id.clone(),
            },
            TaskEvent::Reopened {
                task_id: task_id.clone(),
                reason: "retry".into(),
                source_evidence: "test".into(),
            },
            TaskEvent::OwnerAssigned {
                task_id: task_id.clone(),
                by: actor.clone(),
                owner: Some(actor.clone()),
                routed_to: Some(InstanceName::from("lead")),
            },
            TaskEvent::PriorityChanged {
                task_id: task_id.clone(),
                by: actor.clone(),
                priority: "high".into(),
            },
            TaskEvent::DescriptionUpdated {
                task_id: task_id.clone(),
                by: actor.clone(),
                description: "updated".into(),
            },
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec!["catalog".into()],
            },
            TaskEvent::ResultSet {
                task_id: task_id.clone(),
                by: actor.clone(),
                result: "explicit".into(),
            },
            TaskEvent::MetadataSet {
                task_id: task_id.clone(),
                by: actor.clone(),
                key: "key".into(),
                value: serde_json::json!("value"),
            },
            TaskEvent::BranchLinked {
                task_id: task_id.clone(),
                by: actor.clone(),
                branch: "fix/catalog".into(),
            },
            TaskEvent::Linked {
                task_id: task_id.clone(),
                pr_id: PrId(3347),
                source: LinkSource::Explicit {
                    authored_at: "2026-08-24T00:00:00Z".into(),
                },
                snapshot: snapshot.clone(),
            },
            TaskEvent::MovedToReview {
                task_id: task_id.clone(),
            },
            TaskEvent::Unblocked {
                task_id: task_id.clone(),
            },
            TaskEvent::Verified {
                task_id: task_id.clone(),
                by_reviewer: InstanceName::from("reviewer"),
                verdict: "VERIFIED".into(),
            },
            TaskEvent::TaskCloseProposed {
                task_id: task_id.clone(),
                candidate: DoneSource::LegacyBackfill {
                    sweep_id: "sweep".into(),
                    reasoning: "fixture".into(),
                    snapshot: Some(snapshot),
                },
                sweep_id: "sweep".into(),
                confidence: ConfidenceScore {
                    total: 1.0,
                    signal_count: 1,
                    sub_scores: BTreeMap::new(),
                },
            },
            TaskEvent::Done {
                task_id: task_id.clone(),
                by: actor.clone(),
                source: DoneSource::ReportAutoClose {
                    report_summary: "does not replace explicit".into(),
                    closed_at: "2026-08-24T00:00:00Z".into(),
                },
            },
            TaskEvent::Released {
                task_id: task_id.clone(),
                reason: "release".into(),
            },
            TaskEvent::Superseded {
                task_id: task_id.clone(),
                by: actor.clone(),
                successor_id: TaskId::from("t-20260824000000000000-1-3"),
            },
            TaskEvent::Claimed {
                task_id: child_id.clone(),
                by: actor.clone(),
            },
            TaskEvent::Cancelled {
                task_id: task_id.clone(),
                by: actor,
                reason: "cancel tree".into(),
            },
        ];

        let mut incumbent = TaskBoardState::default();
        let mut projection = BoardProjection::default();
        for (index, event) in events.into_iter().enumerate() {
            let env = envelope(index as u64 + 1, event);
            assert!(incumbent.apply(&env));
            assert!(projection.apply(&env));
            assert_eq!(
                projection,
                BoardProjection::from_replay(incumbent.clone()),
                "projection diverged at event {} ({})",
                env.seq,
                env.event.kind_str()
            );
        }

        assert_eq!(
            projection.task(&child_id).expect("child").status,
            TaskStatus::Cancelled,
            "parent cancellation must cascade exactly like incumbent replay"
        );
    }

    #[test]
    fn incremental_apply_dedupes_per_instance_and_bounds_history() {
        let task_id = TaskId::from("t-20260824000000000000-1-1");
        let created = envelope(
            1,
            TaskEvent::Created {
                task_id: task_id.clone(),
                title: "bounded".into(),
                description: "incremental".into(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: Vec::new(),
                parent_id: None,
            },
        );
        let mut projection = BoardProjection::default();
        assert!(projection.apply(&created));
        assert!(!projection.apply(&created));

        for seq in 2..=25 {
            assert!(projection.apply(&envelope(
                seq,
                TaskEvent::MetadataSet {
                    task_id: task_id.clone(),
                    by: InstanceName::from("writer"),
                    key: "seq".into(),
                    value: serde_json::json!(seq),
                },
            )));
        }
        let other = envelope_from(
            "other-writer",
            2,
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec!["other".into()],
            },
        );
        assert!(projection.apply(&other));
        assert!(!projection.apply(&envelope_from(
            "other-writer",
            1,
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec!["stale".into()],
            },
        )));

        let task = projection.task(&task_id).expect("task");
        assert_eq!(task.history_len, 26);
        assert_eq!(task.recent_history.len(), RECENT_HISTORY_LIMIT);
        assert_eq!(task.recent_history.front().map(|entry| entry.seq), Some(11));
        assert_eq!(task.recent_history.back().map(|entry| entry.seq), Some(2));
        assert_eq!(projection.events_folded(), 26);
        assert_eq!(
            projection.last_seq_for(&InstanceName::from("writer")),
            Some(25)
        );
        assert_eq!(
            projection.last_seq_for(&InstanceName::from("other-writer")),
            Some(2)
        );
    }

    #[test]
    fn ordered_apply_rejects_non_advancing_keys_without_mutation() {
        let task_id = TaskId::from("t-20260824000000000000-1-1");
        let created = envelope_at(
            "2026-08-24T00:00:02Z",
            1,
            TaskEvent::Created {
                task_id: task_id.clone(),
                title: "ordered".into(),
                description: "tail gate".into(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: Vec::new(),
                parent_id: None,
            },
        );
        let stale = envelope_at(
            "2026-08-24T00:00:01Z",
            2,
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec!["stale".into()],
            },
        );
        let invalid = envelope_at(
            "not-a-timestamp",
            3,
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec!["invalid".into()],
            },
        );
        let mut later_lower_instance = envelope_from(
            "aaa",
            2,
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec!["later".into()],
            },
        );
        later_lower_instance.timestamp = "2026-08-24T00:00:03Z".into();
        let mut lower_instance = envelope_from(
            "AAA",
            99,
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec!["wrong-instance".into()],
            },
        );
        lower_instance.timestamp = later_lower_instance.timestamp.clone();
        let mut lower_seq = envelope_from(
            "aaa",
            1,
            TaskEvent::TagsSet {
                task_id,
                tags: vec!["wrong-seq".into()],
            },
        );
        lower_seq.timestamp = later_lower_instance.timestamp.clone();

        let mut projection = BoardProjection::default();
        assert_eq!(projection.apply_ordered(&created), Ok(true));
        let accepted = projection.clone();

        assert!(matches!(
            projection.apply_ordered(&stale),
            Err(OrderedApplyError::OutOfOrder { .. })
        ));
        assert_eq!(projection, accepted);
        assert!(matches!(
            projection.apply_ordered(&created),
            Err(OrderedApplyError::OutOfOrder { .. })
        ));
        assert_eq!(projection, accepted);
        assert_eq!(
            projection.apply_ordered(&invalid),
            Err(OrderedApplyError::InvalidTimestamp(
                "not-a-timestamp".into()
            ))
        );
        assert_eq!(projection, accepted);

        assert_eq!(projection.apply_ordered(&later_lower_instance), Ok(true));
        let advanced = projection.clone();
        assert!(matches!(
            projection.apply_ordered(&lower_instance),
            Err(OrderedApplyError::OutOfOrder { .. })
        ));
        assert_eq!(projection, advanced);
        assert!(matches!(
            projection.apply_ordered(&lower_seq),
            Err(OrderedApplyError::OutOfOrder { .. })
        ));
        assert_eq!(projection, advanced);
    }

    #[test]
    fn canonical_initial_fold_matches_replay_and_seeds_order_cursor() {
        let task_id = TaskId::from("t-20260824000000000000-1-1");
        let tagged = |instance: &str, timestamp: &str, seq: u64, tag: &str| {
            let mut env = envelope_from(
                instance,
                seq,
                TaskEvent::TagsSet {
                    task_id: task_id.clone(),
                    tags: vec![tag.into()],
                },
            );
            env.timestamp = timestamp.into();
            env
        };
        let mut envelopes = vec![
            tagged("AAA", "2026-08-24T00:00:03Z", 1, "last"),
            tagged("zzz", "2026-08-24T00:00:02Z", 1, "third"),
            tagged("aaa", "2026-08-24T00:00:02Z", 2, "second"),
            tagged("aaa", "2026-08-24T00:00:02Z", 1, "first"),
            envelope_at(
                "2026-08-24T00:00:01Z",
                1,
                TaskEvent::Created {
                    task_id: task_id.clone(),
                    title: "initial fold".into(),
                    description: "cursor seed".into(),
                    priority: "normal".into(),
                    owner: None,
                    due_at: None,
                    depends_on: Vec::new(),
                    routed_to: None,
                    branch: None,
                    bind: None,
                    eta_secs: None,
                    tags: Vec::new(),
                    parent_id: None,
                },
            ),
        ];
        let mut order_key_sorted = envelopes.clone();
        order_key_sorted.sort_by_key(|env| OrderKey::from_envelope(env).expect("valid key"));
        super::super::sort_envelopes(&mut envelopes);
        let identity = |env: &TaskEventEnvelope| (&env.timestamp, &env.instance, env.seq);
        assert_eq!(
            order_key_sorted.iter().map(identity).collect::<Vec<_>>(),
            envelopes.iter().map(identity).collect::<Vec<_>>()
        );

        let mut incumbent = TaskBoardState::default();
        for env in &envelopes {
            assert!(incumbent.apply(env));
        }
        let projection = BoardProjection::from_sorted_envelopes(&envelopes).expect("initial fold");
        let mut expected = BoardProjection::from_replay(incumbent);
        expected.last_order_key = projection.last_order_key.clone();
        assert_eq!(projection, expected);

        let mut stale = tagged("zzz", "2026-08-24T00:00:02Z", 2, "stale");
        stale.timestamp = "2026-08-24T00:00:02Z".into();
        let mut projection = projection;
        let accepted = projection.clone();
        assert!(matches!(
            projection.apply_ordered(&stale),
            Err(OrderedApplyError::OutOfOrder { .. })
        ));
        assert_eq!(projection, accepted);
    }
}
