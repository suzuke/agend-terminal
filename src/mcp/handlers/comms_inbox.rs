use serde_json::{json, Value};
use std::path::Path;

pub fn handle_describe_message(home: &Path, args: &Value, instance_name: &str) -> Value {
    let msg_id = match args["message_id"].as_str() {
        Some(id) => id,
        None => return json!({"error": "missing 'message_id'"}),
    };
    let target = args["instance"].as_str().unwrap_or(instance_name);
    let status = crate::inbox::describe_message(home, msg_id, target);
    match status {
        crate::inbox::MessageStatus::ReadAt(t, dm) => {
            let mut resp = json!({"status": "read", "read_at": t});
            if let Some(mode) = dm {
                resp["delivery_mode"] = json!(mode);
            }
            if let Some(msg) = crate::inbox::find_message(home, msg_id) {
                if let Some(ref cid) = msg.correlation_id {
                    resp["correlation_id"] = json!(cid);
                }
                if let Some(ref rh) = msg.reviewed_head {
                    resp["reviewed_head"] = json!(rh);
                    resp["stale_possible"] = json!(true);
                }
            }
            resp
        }
        crate::inbox::MessageStatus::Unread {
            delivery_mode,
            correlation_id,
        } => {
            // #bughunt-r2 #3: a live, un-drained message — distinct from
            // not_found so delivery audit sees it was delivered but not yet read.
            let mut resp = json!({"status": "unread"});
            if let Some(mode) = delivery_mode {
                resp["delivery_mode"] = json!(mode);
            }
            if let Some(cid) = correlation_id {
                resp["correlation_id"] = json!(cid);
            }
            resp
        }
        crate::inbox::MessageStatus::UnreadExpired => {
            json!({"status": "unread_expired"})
        }
        crate::inbox::MessageStatus::NotFound => {
            json!({"status": "not_found"})
        }
    }
}

pub fn handle_describe_thread(home: &Path, args: &Value) -> Value {
    let thread_id = match args["thread_id"].as_str() {
        Some(id) => id,
        None => return json!({"error": "missing 'thread_id'"}),
    };
    let instance = args["instance"].as_str();
    let msgs = crate::inbox::get_thread(home, thread_id, instance);
    json!({"thread_id": thread_id, "messages": msgs, "count": msgs.len()})
}

/// #inbox-gc part a: quiet compact-clear. Marks non-obligation messages read
/// and returns bounded compact summaries; obligations stay unread + surface in
/// `requires_response`. Does NOT drain (no reply-ledger arm / heartbeat touch).
pub fn handle_inbox_clear(home: &Path, instance_name: &str) -> Value {
    let result =
        crate::inbox::clear_compact(home, instance_name, |msg| obligation_reason(home, msg));
    serde_json::to_value(&result).unwrap_or_else(|e| json!({"error": format!("serialize: {e}")}))
}

/// Trust rule (decision d-20260607081209372642-1): which UNREAD messages MUST
/// stay unread on a compact-clear. `Some(reason)` → keep unread; `None` → safe
/// to clear. `read_at` means "non-obligation cleared from attention", NOT
/// "obligation accepted" — so an unanswered query or an unsettled task is kept.
/// When proof is uncertain we KEEP (failure mode = noise, never hidden work).
fn obligation_reason(home: &Path, msg: &crate::inbox::InboxMessage) -> Option<String> {
    match msg.kind.as_deref() {
        // An unanswered query — the sender is blocked waiting on a reply.
        Some("query") => Some("unanswered query".to_string()),
        // A delegated task — keep unless the task board proves it terminal.
        Some("task") => {
            let tid = msg.task_id.as_deref().or(msg.correlation_id.as_deref());
            match tid {
                Some(id) => match crate::tasks::load_by_id(home, id) {
                    Some(t)
                        if matches!(
                            t.status,
                            crate::task_events::TaskStatus::Done
                                | crate::task_events::TaskStatus::Cancelled
                        ) =>
                    {
                        None
                    }
                    Some(t) => Some(format!("task {id} not terminal (status={})", t.status)),
                    None => Some(format!("task {id} not on board — kept")),
                },
                None => Some("task without id — kept".to_string()),
            }
        }
        // update / report / ci-watch / poll / ambient — safe to clear.
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// #bughunt-r2 #3: querying a LIVE, un-drained message by id must report
    /// `status: "unread"` (with its delivery_mode + correlation_id) — NOT
    /// `not_found`, which previously broke delivery audit of undelivered work.
    #[test]
    fn describe_live_unread_message_returns_status_unread() {
        let home = std::env::temp_dir().join(format!(
            "agend-bughunt-r2-unread-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let inbox_dir = home.join("inbox");
        std::fs::create_dir_all(&inbox_dir).unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let msg = format!(
            r#"{{"schema_version":1,"id":"m-live","from":"lead","text":"hi","kind":"task","timestamp":"{now}","delivery_mode":"pty","correlation_id":"t-abc"}}"#
        );
        std::fs::write(inbox_dir.join("agent1.jsonl"), format!("{msg}\n")).unwrap();

        let resp = handle_describe_message(
            &home,
            &json!({"message_id": "m-live", "instance": "agent1"}),
            "caller",
        );
        assert_eq!(
            resp["status"], "unread",
            "live unread must report status=unread, got {resp}"
        );
        assert_eq!(resp["delivery_mode"], "pty");
        assert_eq!(resp["correlation_id"], "t-abc");

        // A genuinely-absent id stays distinct.
        let nf = handle_describe_message(
            &home,
            &json!({"message_id": "m-nope", "instance": "agent1"}),
            "caller",
        );
        assert_eq!(nf["status"], "not_found");

        std::fs::remove_dir_all(&home).ok();
    }
}
