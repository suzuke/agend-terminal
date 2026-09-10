//! Exact operator preview. Only the authenticated direct API dispatch calls this.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

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

pub(crate) fn apply(home: &Path, params: &Value, actor_digest: &str) -> Value {
    use crate::task_events::{OperatorSettlement, TaskEvent, TaskId};
    let request: ApplyRequest = match serde_json::from_value(params.clone()) {
        Ok(request) => request,
        Err(error) => return json!({"ok":false,"code":"invalid_request","error":error.to_string()}),
    };
    if !uuid::Uuid::parse_str(&request.confirmation)
        .is_ok_and(|id| id.to_string() == request.confirmation) {
        return json!({"ok":false,"code":"invalid_confirmation"});
    }
    let bytes = match std::fs::read(home.join("operator-task-confirmations").join(format!("{}.json", request.confirmation))) {
        Ok(bytes) => bytes,
        Err(error) => return json!({"ok":false,"code":"confirmation_unavailable","error":error.to_string()}),
    };
    let confirmation: Confirmation = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => return json!({"ok":false,"code":"confirmation_invalid","error":error.to_string()}),
    };
    if confirmation.schema_version != 1 || confirmation.actor_digest != actor_digest {
        return json!({"ok":false,"code":"confirmation_authority_mismatch"});
    }
    let routed = match super::load_routed(home, &confirmation.task_id) {
        Ok(routed) => routed,
        Err(error) => return json!({"ok":false,"code":"task_route_unavailable","error":error.to_string()}),
    };
    if routed.board().project() != confirmation.board {
        return json!({"ok":false,"code":"stale_preview"});
    }
    let board = routed.board().path().to_owned();
    let tid = TaskId(confirmation.task_id.clone());
    let emitter = "operator".into();
    let digest = crate::daemon::utils::sha256_hex(&bytes);
    let outcome = routed.with_revalidated_computed(home, &emitter, |state| {
        // Read durable event proof before checking the current row: retry must
        // not close a row that was reopened after this operation completed.
        let history = crate::task_events::envelopes_for_task_at(&board, &confirmation.task_id)
            .map_err(|error| format!("history_unavailable: {error}"))?;
        for envelope in history {
            if let TaskEvent::OperatorSettled { proof, .. } = envelope.event {
                if proof.operation_id == request.confirmation {
                    return if proof.preview_digest == digest { Ok(Vec::new()) }
                        else { Err("confirmation_proof_mismatch".into()) };
                }
            }
        }
        let record = state.tasks.get(&tid).ok_or("stale_preview")?;
        if record.status.is_terminal()
            || serde_json::to_value(record).map_err(|e| e.to_string())? != confirmation.subject {
            return Err("stale_preview".into());
        }
        Ok(vec![TaskEvent::OperatorSettled {
            task_id: tid.clone(),
            proof: OperatorSettlement {
                operation_id: request.confirmation.clone(), preview_digest: digest.clone(),
                by: emitter.clone(), holder_instance: record.owner.clone(),
                target: confirmation.target, result: confirmation.result.clone(),
            },
        }])
    });
    match outcome {
        Ok(Ok(Ok(seqs))) => json!({"ok":true,"result":{
            "task_id":confirmation.task_id,"already_applied":seqs.is_empty(),
            "worktree_cleanup":false,"cleanup_status":"not_verified"
        }}),
        Ok(Ok(Err(code))) => json!({"ok":false,"code":code}),
        Ok(Err(error)) => json!({"ok":false,"code":"settlement_write_failed","error":error.to_string()}),
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
            let code = if matches!(error, super::TaskRouteError::NotFound) { "task_not_found" } else { "task_route_unavailable" };
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
    if let Err(error) = std::fs::create_dir_all(&directory).map_err(anyhow::Error::from)
        .and_then(|_| crate::store::save_atomic(&directory.join(format!("{token}.json")), &confirmation)) {
        return json!({"ok":false,"code":"confirmation_write_failed","error":error.to_string()});
    }
    json!({"ok":true,"result":{
        "task_id":confirmation.task_id,"board":confirmation.board,
        "subject":confirmation.subject,"target":confirmation.target,"result":confirmation.result,
        "confirmation":token,"worktree_cleanup":false
    }})
}
