//! Typed, atomic same-board task supersession (#3279).

use serde_json::Value;
use std::path::Path;

/// Create a successor and terminate its predecessor in one same-board event-log
/// transaction. Both task-id locks are acquired in lexical order so opposite
/// supersession attempts cannot deadlock. No IPC occurs while held.
pub(super) fn create_with_supersession(
    home: &Path,
    project: &str,
    emitter: &crate::task_events::InstanceName,
    successor_id: &str,
    predecessor_id: &str,
    created: crate::task_events::TaskEvent,
    args: &Value,
) -> Value {
    if successor_id == predecessor_id {
        return serde_json::json!({
            "error": "a task cannot supersede itself",
            "code": "supersession_refused",
        });
    }

    let mut ids = [successor_id, predecessor_id];
    ids.sort_unstable();
    let mut id_locks = Vec::with_capacity(2);
    for id in ids {
        match super::board_router::acquire_task_id_lock(home, id) {
            Ok(lock) => id_locks.push(lock),
            Err(error) => {
                return serde_json::json!({
                    "error": format!("failed to acquire per-id lock for '{id}': {error}"),
                    "code": "task_lock_failed",
                });
            }
        }
    }

    match super::load_routed(home, successor_id) {
        Err(super::TaskRouteError::NotFound) => {}
        Ok(_) => {
            return serde_json::json!({
                "error": format!("task id '{successor_id}' already exists on a board"),
                "code": "duplicate_task_id",
            });
        }
        Err(error) => {
            return serde_json::json!({
                "error": format!("cannot prove task id '{successor_id}' is unused: {error}"),
                "code": "task_route_unresolved",
            });
        }
    }

    let predecessor = match super::load_routed(home, predecessor_id) {
        Ok(routed) => routed,
        Err(error) => {
            return serde_json::json!({
                "error": format!("cannot route superseded task '{predecessor_id}': {error}"),
                "code": "supersession_refused",
            });
        }
    };
    let predecessor_project = predecessor.board().project();
    if predecessor_project != project {
        let detail = format!(
            "successor={successor_id} project={project} predecessor={predecessor_id} \
             predecessor_project={predecessor_project}"
        );
        if let Err(error) =
            crate::event_log::try_log(home, "task_supersession_refused", emitter.as_str(), &detail)
        {
            return serde_json::json!({
                "error": format!("cross-board supersession refusal could not be audited: {error}"),
                "code": "supersession_refusal_audit_failed",
            });
        }
        return serde_json::json!({
            "error": format!(
                "cross-board supersession denied: predecessor '{predecessor_id}' belongs to \
                 '{predecessor_project}' but successor targets '{project}'"
            ),
            "code": "cross_board_supersession",
        });
    }

    let board = crate::task_events::board_root(home, project);
    let plan_ack_required = args["plan_ack_required"].as_u64().unwrap_or(0);
    let typed_predecessor = crate::task_events::TaskId(predecessor_id.to_string());
    let typed_successor = crate::task_events::TaskId(successor_id.to_string());
    let plan_reason = args["plan_ack_reason"].as_str().unwrap_or("").to_string();
    let review_class = args["review_class"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let append = crate::task_events::append_batch_computed_at(&board, emitter, |state| {
        let Some(current) = state.tasks.get(&typed_predecessor) else {
            return Err(format!(
                "predecessor '{predecessor_id}' disappeared before commit"
            ));
        };
        if current.status == crate::task_events::TaskStatus::Superseded {
            return if current.superseded_by.is_some() {
                Ok(Vec::new())
            } else {
                Err(format!(
                    "predecessor '{predecessor_id}' is superseded without typed successor"
                ))
            };
        }
        if current.status.is_terminal() {
            return Err(format!(
                "predecessor '{predecessor_id}' is already terminal ({})",
                current.status
            ));
        }
        if state.tasks.contains_key(&typed_successor) {
            return Err(format!("successor id '{successor_id}' already exists"));
        }

        let mut events = vec![created];
        if plan_ack_required > 0 {
            events.push(crate::task_events::TaskEvent::MetadataSet {
                task_id: typed_successor.clone(),
                by: emitter.clone(),
                key: "plan_ack_required".to_string(),
                value: serde_json::json!(plan_ack_required),
            });
            events.push(crate::task_events::TaskEvent::MetadataSet {
                task_id: typed_successor.clone(),
                by: emitter.clone(),
                key: "plan_ack_reason".to_string(),
                value: serde_json::json!(plan_reason),
            });
        }
        if let Some(review_class) = review_class {
            events.push(crate::task_events::TaskEvent::MetadataSet {
                task_id: typed_successor.clone(),
                by: emitter.clone(),
                key: "review_class".to_string(),
                value: serde_json::json!(review_class),
            });
        }
        events.push(crate::task_events::TaskEvent::Superseded {
            task_id: typed_predecessor.clone(),
            by: emitter.clone(),
            successor_id: typed_successor.clone(),
        });
        Ok(events)
    });

    let seqs = match append {
        Ok(Ok(seqs)) => seqs,
        Ok(Err(reason)) => {
            return serde_json::json!({
                "error": reason,
                "code": "supersession_refused",
            });
        }
        Err(error) => {
            return serde_json::json!({
                "error": format!("atomic supersession append failed: {error}"),
                "code": "supersession_append_failed",
            });
        }
    };

    if seqs.is_empty() {
        let state = match crate::task_events::projected_state_at(&board) {
            Ok(state) => state,
            Err(error) => {
                return serde_json::json!({
                    "error": format!("cannot replay idempotent supersession: {error}"),
                    "code": "supersession_replay_failed",
                });
            }
        };
        let existing_id = state
            .tasks
            .get(&typed_predecessor)
            .and_then(|task| task.superseded_by.as_ref())
            .map(|id| id.0.clone());
        let Some(existing_id) = existing_id else {
            return serde_json::json!({
                "error": "idempotent supersession lost its typed successor",
                "code": "supersession_replay_failed",
            });
        };
        let task = super::handler::read_task_record_at(&board, &existing_id)
            .map(|record| super::record_to_task(&record));
        drop(id_locks);
        super::task_terminal_cleanup(home, predecessor_id);
        return serde_json::json!({
            "id": existing_id,
            "event": "already_superseded",
            "task": task,
            "status": "created",
        });
    }

    let task = super::handler::read_task_record_at(&board, successor_id)
        .map(|record| super::record_to_task(&record));
    drop(id_locks);
    super::task_terminal_cleanup(home, predecessor_id);
    serde_json::json!({
        "id": successor_id,
        "event": "created",
        "task": task,
        "status": "created",
    })
}
