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
