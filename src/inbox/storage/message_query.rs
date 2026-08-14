use super::{inbox_files, inbox_path_resolved, parse_inbox_messages, UnreadProbe};
use crate::inbox::{InboxMessage, MessageStatus};
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

/// #2604: the agent names with an inbox file (the `*.jsonl` file stems under
/// `home/inbox`). The offline-unread watchdog iterates these to reach agents
/// that are NOT in the live registry (offline / never-existed) — the exact set
/// `poll_reminder` (registry-driven) can never see. A `read_dir` error yields an
/// empty iteration, same as [`inbox_files`].
pub fn inbox_agent_names(home: &Path) -> Vec<String> {
    inbox_files(home)
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(String::from))
        .collect()
}

/// Count unread messages (read_at == None) for an agent.
///
/// The cheap `UnreadProbe` deserialize keeps this hot-path query from
/// allocating the full message text. Its actionable-unread filter excludes
/// superseded and delivering rows, matching the drain path and avoiding
/// duplicate paging of a healthy agent.
pub fn unread_count(home: &Path, name: &str) -> (usize, Option<chrono::DateTime<chrono::Utc>>) {
    let path = inbox_path_resolved(home, name);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return (0, None),
    };
    let mut count = 0usize;
    let mut oldest: Option<chrono::DateTime<chrono::Utc>> = None;
    for line in content.lines() {
        if let Ok(probe) = serde_json::from_str::<UnreadProbe>(line) {
            if probe.is_unread() {
                count += 1;
                if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&probe.timestamp) {
                    let ts_utc = ts.with_timezone(&chrono::Utc);
                    if oldest.is_none_or(|t| t > ts_utc) {
                        oldest = Some(ts_utc);
                    }
                }
            }
        }
    }
    (count, oldest)
}

/// Look up a message by ID in a specific agent's inbox file.
/// If `instance` is provided, only that agent's inbox is searched.
pub fn describe_message(home: &Path, msg_id: &str, instance: &str) -> MessageStatus {
    let path = inbox_path_resolved(home, instance);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let now = chrono::Utc::now();
    for msg in parse_inbox_messages(&content) {
        if msg.id.as_deref() != Some(msg_id) {
            continue;
        }
        if let Some(ref read_at) = msg.read_at {
            return MessageStatus::ReadAt(read_at.clone(), msg.delivery_mode.clone());
        }
        // #2299: a delivered-but-unconfirmed row is reported as `Delivering`
        // so delivery audits distinguish it from an unread or missing row.
        if msg.delivering_at.is_some() {
            return MessageStatus::Delivering {
                delivery_mode: msg.delivery_mode.clone(),
                correlation_id: msg.correlation_id.clone(),
            };
        }
        let ts = chrono::DateTime::parse_from_rfc3339(&msg.timestamp)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or(now);
        if now.signed_duration_since(ts) > chrono::Duration::days(30) {
            return MessageStatus::UnreadExpired;
        }
        return MessageStatus::Unread {
            delivery_mode: msg.delivery_mode.clone(),
            correlation_id: msg.correlation_id.clone(),
        };
    }
    MessageStatus::NotFound
}

/// #982 B-narrow: scan `agent_name`'s inbox for a delivered blocking
/// dispatch (`kind ∈ {query, task}`) that shares the given `correlation_id`.
/// A delivering row counts as delivered, matching the JSONL dedup query.
pub fn has_drained_blocker_for_correlation(
    home: &Path,
    agent_name: &str,
    correlation_id: &str,
) -> bool {
    #[cfg(test)]
    super::super::record_blocker_scan(agent_name);
    let path = inbox_path_resolved(home, agent_name);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let found = parse_inbox_messages(&content).any(|msg| {
        msg.correlation_id.as_deref() == Some(correlation_id)
            && (msg.read_at.is_some() || msg.delivering_at.is_some())
            && matches!(msg.kind.as_deref(), Some("query") | Some("task"))
    });
    found
}

/// Read the agent's inbox JSONL and return `true` iff an id was delivered —
/// either processed (`read_at`) or in-flight (`delivering_at`).
pub(crate) fn msg_already_drained_in_jsonl(home: &Path, agent_name: &str, msg_id: &str) -> bool {
    let path = inbox_path_resolved(home, agent_name);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let found = parse_inbox_messages(&content).any(|msg| {
        msg.id.as_deref() == Some(msg_id) && (msg.read_at.is_some() || msg.delivering_at.is_some())
    });
    found
}
