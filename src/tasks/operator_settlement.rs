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
