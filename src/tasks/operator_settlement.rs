//! Exact operator preview. Only the authenticated direct API dispatch calls this.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

// An unused preview is short-lived; durable applied-operation proof is checked
// first so expiration never turns a committed retry into a new write.
const CONFIRMATION_TTL_SECS: i64 = 15 * 60;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewRequest {
    task_id: String,
    target: crate::task_events::OperatorSettlementTarget,
    result: String,
}

#[derive(Serialize, Deserialize)]
struct Confirmation {
    schema_version: u32,
    actor_digest: String,
    created_at: String,
    task_id: String,
    board: String,
    subject: Value,
    target: crate::task_events::OperatorSettlementTarget,
    result: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyRequest {
    confirmation: String,
}

/// Run cleanup only while the exact terminal event remains authoritative.
/// Caller may hold an assignment branch lock; this acquires task-id then board.
/// Cleanup must not append task events or perform daemon IPC under these locks.
pub(crate) fn with_terminal_cleanup<T>(
    home: &Path,
    board: &str,
    task_id: &str,
    instance: &str,
    seq: u64,
    cleanup: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    with_current_terminal_cleanup(home, board, task_id, Some((instance, seq)), cleanup)
}

/// Background reconciliation may use current terminal state for a cascaded
/// child, which has no independent terminal event. Operator retries always
/// provide their exact event and cannot take this current-state path.
pub(crate) fn with_current_terminal_cleanup<T>(
    home: &Path,
    board: &str,
    task_id: &str,
    expected_event: Option<(&str, u64)>,
    cleanup: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let routed = super::load_routed(home, task_id).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    anyhow::ensure!(routed.board().project() == board, "cleanup board changed");
    let mut result = None;
    let checked = routed
        .with_revalidated_computed(home, &"system:terminal-cleanup".into(), |state| {
            let task = state
                .tasks
                .get(&task_id.into())
                .ok_or("cleanup task missing")?;
            let terminal = task.history.iter().rev().find(|entry| {
                matches!(
                    entry.kind,
                    "done" | "cancelled" | "superseded" | "operator_settled"
                )
            });
            if !task.status.is_terminal()
                || expected_event.is_some_and(|(instance, seq)| {
                    !terminal.is_some_and(|entry| entry.instance.0 == instance && entry.seq == seq)
                })
            {
                return Err("stale terminal cleanup generation".into());
            }
            result = Some(cleanup());
            Ok(Vec::new())
        })
        .map_err(|e| anyhow::anyhow!("{e:?}"))??;
    checked.map_err(anyhow::Error::msg)?;
    result.ok_or_else(|| anyhow::anyhow!("cleanup was not executed"))?
}

fn cleanup_committed(
    home: &Path,
    task_id: &str,
    operation: &str,
    digest: &str,
    expected_board: Option<&str>,
) -> anyhow::Result<()> {
    use crate::task_events::TaskEvent;
    let routed = super::load_routed(home, task_id).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let board = routed.board();
    anyhow::ensure!(
        expected_board.is_none_or(|expected| board.project() == expected),
        "cleanup board changed"
    );
    let history = crate::task_events::envelopes_for_task_at(board.path(), task_id)?;
    let event = history
        .iter()
        .find(|envelope| {
            matches!(&envelope.event,
        TaskEvent::OperatorSettled { proof, .. }
        if proof.operation_id == operation && proof.preview_digest == digest)
        })
        .ok_or_else(|| anyhow::anyhow!("settlement cleanup event unavailable"))?;
    crate::daemon::assignment_authority::retire_terminal_event_checked(
        home,
        board.project(),
        task_id,
        &event.instance.0,
        event.seq,
        &event.timestamp,
    )?;
    with_terminal_cleanup(
        home,
        board.project(),
        task_id,
        &event.instance.0,
        event.seq,
        || {
            crate::daemon::dispatch_idle::cleanup_pending_for_task_id_checked(home, task_id)?;
            crate::dispatch_tracking::remove_all_for_task_checked(home, task_id)
        },
    )
}

/// Committed event proof, not preview files or surviving assignments, drives
/// recovery. Reopened tasks are excluded and the final locks recheck identity.
pub(crate) fn retry_committed_cleanups(home: &Path) -> anyhow::Result<()> {
    let tasks = crate::task_events::catalog::for_home(home)
        .all_tasks()
        .map_err(|e| anyhow::anyhow!("cleanup catalog unavailable: {e:?}"))?;
    let mut failures = Vec::new();
    for task in tasks {
        if !task.status.is_terminal() {
            continue;
        }
        if let Some(proof) = &task.last_operator_settlement {
            if let Err(error) = cleanup_committed(
                home,
                &task.id.0,
                &proof.operation_id,
                &proof.preview_digest,
                None,
            ) {
                failures.push(format!("{}: {error}", task.id.0));
            }
        }
    }
    anyhow::ensure!(
        failures.is_empty(),
        "operator cleanup pending: {}",
        failures.join("; ")
    );
    Ok(())
}

pub(crate) fn apply(home: &Path, params: &Value, actor_digest: &str) -> Value {
    use crate::task_events::{OperatorSettlement, TaskEvent, TaskId};
    let request: ApplyRequest = match serde_json::from_value(params.clone()) {
        Ok(request) => request,
        Err(error) => return json!({"ok":false,"code":"invalid_request","error":error.to_string()}),
    };
    if !uuid::Uuid::parse_str(&request.confirmation)
        .is_ok_and(|id| id.to_string() == request.confirmation)
    {
        return json!({"ok":false,"code":"invalid_confirmation"});
    }
    let bytes = match std::fs::read(
        home.join("operator-task-confirmations")
            .join(format!("{}.json", request.confirmation)),
    ) {
        Ok(bytes) => bytes,
        Err(error) => {
            return json!({"ok":false,"code":"confirmation_unavailable","error":error.to_string()})
        }
    };
    let confirmation: Confirmation = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return json!({"ok":false,"code":"confirmation_invalid","error":error.to_string()})
        }
    };
    if confirmation.schema_version != 1 {
        return json!({"ok":false,"code":"confirmation_authority_mismatch"});
    }
    let routed = match super::load_routed(home, &confirmation.task_id) {
        Ok(routed) => routed,
        Err(error) => {
            return json!({"ok":false,"code":"task_route_unavailable","error":error.to_string()})
        }
    };
    if routed.board().project() != confirmation.board {
        return json!({"ok":false,"code":"stale_preview"});
    }
    let board = routed.board().path().to_owned();
    let tid = TaskId(confirmation.task_id.clone());
    let emitter = "operator".into();
    let digest = crate::daemon::utils::sha256_hex(&bytes);
    let mut current_status = None;
    let outcome = routed.with_revalidated_computed(home, &emitter, |state| {
        let record = state.tasks.get(&tid).ok_or("stale_preview")?;
        current_status = Some(record.status.to_string());
        // Read durable event proof before checking the current row: retry must
        // not close a row that was reopened after this operation completed.
        let history = crate::task_events::envelopes_for_task_at(&board, &confirmation.task_id)
            .map_err(|error| format!("history_unavailable: {error}"))?;
        for envelope in history {
            if let TaskEvent::OperatorSettled { proof, .. } = envelope.event {
                if proof.operation_id == request.confirmation {
                    return if proof.preview_digest == digest {
                        Ok(Vec::new())
                    } else {
                        Err("confirmation_proof_mismatch".into())
                    };
                }
            }
        }
        // A committed operation is recoverable after daemon restart: the new
        // daemon has a fresh operator credential, but the durable proof binds
        // this exact confirmation and no new mutation is possible here.  For
        // an uncommitted confirmation, retain the actor-bound authority check.
        if confirmation.actor_digest != actor_digest {
            return Err("confirmation_authority_mismatch".into());
        }
        let created = chrono::DateTime::parse_from_rfc3339(&confirmation.created_at)
            .map_err(|_| "confirmation_invalid".to_owned())?;
        let age = chrono::Utc::now().signed_duration_since(created);
        if age < chrono::Duration::zero() || age >= chrono::Duration::seconds(CONFIRMATION_TTL_SECS)
        {
            return Err("confirmation_expired".into());
        }
        if record.status.is_terminal()
            || serde_json::to_value(record).map_err(|e| e.to_string())? != confirmation.subject
        {
            return Err("stale_preview".into());
        }
        current_status = Some(confirmation.target.status().to_string());
        Ok(vec![TaskEvent::OperatorSettled {
            task_id: tid.clone(),
            proof: OperatorSettlement {
                operation_id: request.confirmation.clone(),
                preview_digest: digest.clone(),
                by: emitter.clone(),
                holder_instance: record.owner.clone(),
                target: confirmation.target,
                result: confirmation.result.clone(),
            },
        }])
    });
    match outcome {
        Ok(Ok(Ok(seqs))) => {
            // The task-id closure has returned. Never take the assignment
            // branch lock from the catalog callback under that outer lock.
            // The event itself is durable retry intent, including after replay.
            let cleanup = cleanup_committed(
                home,
                &confirmation.task_id,
                &request.confirmation,
                &digest,
                Some(&confirmation.board),
            );
            json!({"ok":true,"result":{
                "task_id":confirmation.task_id,"already_applied":seqs.is_empty(),
                "original_target":confirmation.target,"original_result":confirmation.result,
                "current_status":current_status,
                "worktree_cleanup":false,
                "cleanup_status":if cleanup.is_ok() {"complete"} else {"pending"},
                "cleanup_error":cleanup.err().map(|error| error.to_string())
            }})
        }
        Ok(Ok(Err(code))) => json!({"ok":false,"code":code}),
        Ok(Err(error)) => {
            json!({"ok":false,"code":"settlement_write_failed","error":error.to_string()})
        }
        Err(error) => json!({"ok":false,"code":"stale_preview","error":error.to_string()}),
    }
}

pub(crate) fn preview(home: &Path, params: &Value, actor_digest: &str) -> Value {
    let request: PreviewRequest = match serde_json::from_value(params.clone()) {
        Ok(request) => request,
        Err(error) => return json!({"ok":false,"code":"invalid_request","error":error.to_string()}),
    };
    if request.task_id.trim().is_empty() || request.result.trim().is_empty() {
        return json!({"ok":false,"code":"invalid_request","error":"task_id and result must be nonempty"});
    }
    let routed = match super::load_routed(home, &request.task_id) {
        Ok(task) => task,
        Err(error) => {
            let code = if matches!(error, super::TaskRouteError::NotFound) {
                "task_not_found"
            } else {
                "task_route_unavailable"
            };
            return json!({"ok":false,"code":code,"error":error.to_string()});
        }
    };
    if routed.record().status.is_terminal() {
        return json!({"ok":false,"code":"already_terminal","error":"task is already terminal"});
    }
    let subject = match serde_json::to_value(routed.record()) {
        Ok(subject) => subject,
        Err(error) => return json!({"ok":false,"code":"snapshot_failed","error":error.to_string()}),
    };
    let confirmation = Confirmation {
        schema_version: 1,
        actor_digest: actor_digest.to_owned(),
        created_at: chrono::Utc::now().to_rfc3339(),
        task_id: request.task_id,
        board: routed.board().project().to_owned(),
        subject,
        target: request.target,
        result: request.result,
    };
    let token = uuid::Uuid::new_v4().to_string();
    let directory = home.join("operator-task-confirmations");
    if let Err(error) = std::fs::create_dir_all(&directory)
        .map_err(anyhow::Error::from)
        .and_then(|_| {
            crate::store::save_atomic(&directory.join(format!("{token}.json")), &confirmation)
        })
    {
        return json!({"ok":false,"code":"confirmation_write_failed","error":error.to_string()});
    }
    json!({"ok":true,"result":{
        "task_id":confirmation.task_id,"board":confirmation.board,
        "subject":confirmation.subject,"target":confirmation.target,"result":confirmation.result,
        "confirmation":token,"valid_for_seconds":CONFIRMATION_TTL_SECS,"worktree_cleanup":false
    }})
}
