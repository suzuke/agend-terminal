//! Poll-reminder: nudge idle agents that have unread inbox messages.

use crate::agent::{self, AgentRegistry};
use crate::state::AgentState;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

/// Per-agent de-dup state: last notified unread count.
static LAST_NOTIFIED: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

/// Atomic check-and-record: returns true if count changed (should notify),
/// and records the new count in the same lock scope.
fn should_notify_and_record(name: &str, count: usize) -> bool {
    let mut guard = match LAST_NOTIFIED.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let map = guard.get_or_insert_with(HashMap::new);
    let prev = map.get(name).copied().unwrap_or(0);
    if prev == count {
        return false;
    }
    map.insert(name.to_string(), count);
    true
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
            if !should_notify_and_record(name, count) {
                continue;
            }

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
            let count_str = count.to_string();
            let reminder = crate::inbox::format_event_header(
                "poll-reminder",
                &[("unread", &count_str), ("oldest", &age_str)],
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
    use crate::agent::{AgentCore, AgentHandle};
    use crate::state::StateTracker;
    use portable_pty::native_pty_system;
    use std::sync::{Arc, Mutex as StdMutex};

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

    /// Build a minimal AgentRegistry with one agent at the given state.
    /// The PTY/writer are real but unused — we only inspect inbox side effects.
    fn mock_registry(name: &str, state: AgentState) -> AgentRegistry {
        use portable_pty::{CommandBuilder, PtySize};
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let child = pair
            .slave
            .spawn_command(CommandBuilder::new("true"))
            .expect("spawn");

        let writer = pair.master.take_writer().expect("writer");
        let mut state_tracker = StateTracker::new(None);
        state_tracker.current = state;

        let core = AgentCore {
            vterm: crate::vterm::VTerm::new(24, 80),
            subscribers: Vec::new(),
            state: state_tracker,
            health: crate::health::HealthTracker::new(),
        };
        let handle = AgentHandle {
            name: name.to_string(),
            backend_command: "test".to_string(),
            pty_writer: Arc::new(StdMutex::new(writer)),
            pty_master: Arc::new(StdMutex::new(pair.master)),
            core: Arc::new(StdMutex::new(core)),
            child: Arc::new(StdMutex::new(child)),
            submit_key: "\r".to_string(),
            inject_prefix: String::new(),
            typed_inject: false,
        };
        let reg: AgentRegistry = Arc::new(StdMutex::new(HashMap::new()));
        reg.lock().unwrap().insert(name.to_string(), handle);
        reg
    }

    #[test]
    fn test_poll_reminder_pass_injects_when_idle_with_unread() {
        let home = tmp_home("pass-inject");
        // Use a unique agent name to avoid dedup interference from other tests
        let agent = "poll-inject-agent";
        seed_unread(&home, agent, 3);
        let registry = mock_registry(agent, AgentState::Idle);

        // Reset dedup state for this agent
        should_notify_and_record(agent, 0);

        poll_reminder_pass(&home, &registry);

        // Verify: dedup state updated to 3
        assert!(!should_notify_and_record(agent, 3), "dedup should block same count");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_poll_reminder_pass_dedupes_same_count() {
        let home = tmp_home("pass-dedup");
        let agent = "poll-dedup-agent";
        seed_unread(&home, agent, 2);
        let registry = mock_registry(agent, AgentState::Idle);

        // Reset dedup
        should_notify_and_record(agent, 0);

        // First pass: should notify (count changed 0→2)
        poll_reminder_pass(&home, &registry);
        // Second pass: same count → should NOT notify (dedup blocks)
        assert!(!should_notify_and_record(agent, 2));

        // Add more unread → count changes → should notify again
        seed_unread(&home, agent, 1); // now 3 total
        assert!(should_notify_and_record(agent, 3));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_poll_reminder_pass_skips_when_busy() {
        let home = tmp_home("pass-busy");
        let agent = "poll-busy-agent";
        seed_unread(&home, agent, 5);
        let registry = mock_registry(agent, AgentState::Thinking);

        // Reset dedup
        should_notify_and_record(agent, 0);

        poll_reminder_pass(&home, &registry);

        // Dedup state should NOT have been updated (agent was busy, skipped)
        assert!(
            should_notify_and_record(agent, 5),
            "busy agent should not have been recorded in dedup"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
