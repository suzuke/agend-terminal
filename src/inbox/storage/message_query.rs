use super::{inbox_files, inbox_path_resolved, parse_inbox_messages};
use crate::inbox::InboxMessage;
use std::path::Path;

pub fn get_thread(home: &Path, thread_id: &str, instance: Option<&str>) -> Vec<InboxMessage> {
    let mut msgs = Vec::new();

    if let Some(inst) = instance {
        let path = inbox_path_resolved(home, inst);
        collect_thread_messages(&path, thread_id, &mut msgs);
    } else {
        for path in inbox_files(home) {
            collect_thread_messages(&path, thread_id, &mut msgs);
        }
        let mut seen_ids = std::collections::HashSet::new();
        msgs.retain(|m| match &m.id {
            Some(id) => seen_ids.insert(id.clone()),
            None => true,
        });
    }

    msgs.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    msgs
}

fn collect_thread_messages(path: &Path, thread_id: &str, out: &mut Vec<InboxMessage>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };
    for line in content.lines() {
        if !line.contains(thread_id) {
            continue;
        }
        if let Ok(msg) = serde_json::from_str::<InboxMessage>(line) {
            if msg.thread_id.as_deref() == Some(thread_id) {
                out.push(msg);
            }
        }
    }
}

pub fn find_message(home: &Path, msg_id: &str) -> Option<InboxMessage> {
    for path in inbox_files(home) {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        for msg in parse_inbox_messages(&content) {
            if msg.id.as_deref() == Some(msg_id) {
                return Some(msg);
            }
        }
    }
    None
}
