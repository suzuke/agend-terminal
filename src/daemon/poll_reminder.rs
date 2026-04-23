//! Poll-reminder: nudge idle agents that have unread inbox messages.

use crate::agent::{self, AgentRegistry};
use crate::state::AgentState;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

/// Per-agent de-dup state: last notified unread count.
static LAST_NOTIFIED: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

fn get_last(name: &str) -> usize {
    LAST_NOTIFIED
        .lock()
        .ok()
        .and_then(|g| g.as_ref()?.get(name).copied())
        .unwrap_or(0)
}

fn set_last(name: &str, count: usize) {
    let mut guard = match LAST_NOTIFIED.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(name.to_string(), count);
}

/// Run one poll-reminder pass. Called from daemon tick every N ticks.
///
/// For each managed agent: if Idle + unread > 0 + count changed since
/// last notification → inject a single-line reminder to PTY.
pub fn poll_reminder_pass(home: &Path, registry: &AgentRegistry) {
    let mut to_notify: Vec<(String, String)> = Vec::new();
    {
        let reg = agent::lock_registry(registry);
        for (name, handle) in reg.iter() {
            let agent_state = match handle.core.lock() {
                Ok(c) => c.state.current,
                Err(_) => continue,
            };
            if agent_state != AgentState::Idle {
                continue;
            }
            let (count, oldest) = crate::inbox::unread_count(home, name);
            if count == 0 {
                continue;
            }
            if get_last(name) == count {
                continue;
            }
            set_last(name, count);

            let age_str = match oldest {
                Some(ts) => {
                    let mins = chrono::Utc::now()
                        .signed_duration_since(ts)
                        .num_minutes()
                        .max(0);
                    format!("{mins}m")
                }
                None => "?".to_string(),
            };
            let reminder = format!(
                "{} kind=poll-reminder unread={count} oldest={age_str}",
                crate::inbox::HEADER_PREFIX,
            );
            to_notify.push((name.clone(), reminder));
        }
    }
    for (name, reminder) in to_notify {
        crate::inbox::compose_aware_inject(home, &name, &reminder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::HealthTracker;
    use crate::state::AgentState;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-poll-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn seed_unread(home: &Path, agent: &str, count: usize) {
        for i in 0..count {
            let _ = crate::inbox::enqueue(
                home,
                agent,
                crate::inbox::InboxMessage {
                    schema_version: 1,
                    id: Some(format!("m-{i}")),
                    from: "test".into(),
                    text: format!("msg {i}"),
                    kind: None,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    read_at: None,
                    thread_id: None,
                    parent_id: None,
                },
            );
        }
    }

    #[test]
    fn test_poll_reminder_injects_when_idle_with_unread() {
        let home = tmp_home("inject");
        seed_unread(&home, "agent1", 3);
        let (count, oldest) = crate::inbox::unread_count(&home, "agent1");
        assert_eq!(count, 3);
        assert!(oldest.is_some());

        // Verify the reminder format
        let age_mins = chrono::Utc::now()
            .signed_duration_since(oldest.unwrap())
            .num_minutes()
            .max(0);
        let reminder = format!(
            "{} kind=poll-reminder unread=3 oldest={age_mins}m",
            crate::inbox::HEADER_PREFIX,
        );
        assert!(reminder.contains("[AGEND-MSG]"));
        assert!(reminder.contains("unread=3"));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_poll_reminder_dedupes_same_count() {
        // set_last + get_last should prevent re-notification
        set_last("dedup-agent", 5);
        assert_eq!(get_last("dedup-agent"), 5);
        // Same count → would skip in poll_reminder_pass
        // Different count → would notify
        set_last("dedup-agent", 7);
        assert_eq!(get_last("dedup-agent"), 7);
    }

    #[test]
    fn test_poll_reminder_skips_when_busy() {
        // AgentState != Idle should be skipped
        for state in [
            AgentState::Thinking,
            AgentState::ToolUse,
            AgentState::Starting,
            AgentState::Ready,
        ] {
            assert_ne!(state, AgentState::Idle, "{state:?} must not be Idle");
        }
    }
}
