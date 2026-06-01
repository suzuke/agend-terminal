//! Poll-reminder: nudge idle agents that have unread inbox messages.

use crate::agent::{self, AgentRegistry};
use crate::state::AgentState;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;

/// Per-agent de-dup state: last notified unread count.
static LAST_NOTIFIED: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

/// Atomic check-and-record: returns true if count changed (should notify),
/// and records the new count in the same lock scope.
fn should_notify_and_record(name: &str, count: usize) -> bool {
    let mut guard = LAST_NOTIFIED.lock();
    let map = guard.get_or_insert_with(HashMap::new);
    let prev = map.get(name).copied().unwrap_or(0);
    if prev == count {
        return false;
    }
    map.insert(name.to_string(), count);
    true
}

/// H3: Remove agent from dedup state when deleted (prevents unbounded growth).
pub fn remove_agent(name: &str) {
    let mut guard = LAST_NOTIFIED.lock();
    if let Some(map) = guard.as_mut() {
        map.remove(name);
    }
}

/// Pure collector: returns (agent_name, reminder_string) for each agent
/// that should be nudged. No side effects — does not inject into PTY.
pub fn collect_poll_reminders(home: &Path, registry: &AgentRegistry) -> Vec<(String, String)> {
    // #1617-class (mirror conflict_notify's phase-1-collect / phase-2-IO): snapshot
    // the idle-agent names UNDER the registry lock, then DROP the guard before any
    // inbox file IO. `inbox::unread_count` does `fs::read_to_string` + full-file
    // parse; holding the GLOBAL registry lock across that (per agent, in a loop)
    // stalls every other registry user when the inbox is large or the FS is slow.
    let idle_names: Vec<String> = {
        let reg = agent::lock_registry(registry);
        reg.values()
            .filter(|handle| handle.core.lock().state.current == AgentState::Idle)
            .map(|handle| handle.name.to_string())
            .collect()
    };

    // Phase 2: the inbox reads + dedup + formatting run lock-free.
    let mut result = Vec::new();
    for name in &idle_names {
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
        result.push((name.to_string(), reminder));
    }
    result
}

/// Run one poll-reminder pass. Called from daemon tick every N ticks.
/// Collects reminders via [`collect_poll_reminders`] then injects each.
pub fn poll_reminder_pass(home: &Path, registry: &AgentRegistry) {
    for (name, reminder) in collect_poll_reminders(home, registry) {
        crate::inbox::compose_aware_inject(home, &name, &reminder);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::agent::{AgentCore, AgentHandle};
    use crate::state::StateTracker;
    use parking_lot::Mutex;
    use portable_pty::native_pty_system;
    use std::sync::Arc;

    /// #1617-class invariant: `collect_poll_reminders` must NEVER hold the
    /// global registry lock across the blocking `inbox::unread_count` file read.
    /// Holding the registry across per-agent inbox reads stalls every other
    /// registry user (same class #1593/#1617 closed elsewhere; conflict_notify
    /// already does the phase-1-collect / phase-2-IO split this mirrors).
    ///
    /// Structural source-scan (mirrors #1593 F2): brace-match the idle-name
    /// snapshot block and assert (a) `unread_count` is NOT inside it (not under
    /// the lock) and (b) `unread_count` IS called after the block closes (the
    /// IO runs lock-free). Needles are `concat`-built and the scan is sliced to
    /// the production region so this test can't self-satisfy.
    #[test]
    fn poll_reminder_unread_read_not_held_across_registry_lock() {
        let src = include_str!("poll_reminder.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let prod = match src.find(&cfg_test) {
            Some(i) => &src[..i],
            None => src,
        };

        // Fix marker: idle agent names are snapshotted into a Vec under the lock.
        let bind_needle = ["let idle_names: Vec<String>", " = {"].concat();
        let bstart = prod
            .find(&bind_needle)
            .expect("idle-name snapshot binding present (fix marker)");

        let open_rel = prod[bstart..].find('{').expect("binding block opens");
        let block_start = bstart + open_rel;
        let mut depth = 0usize;
        let mut block_end = block_start;
        for (i, c) in prod[block_start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = block_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(block_end > block_start, "binding block must close");

        let io_needle = ["unread", "_count"].concat();
        let locked_region = &prod[block_start..=block_end];
        assert!(
            !locked_region.contains(&io_needle),
            "collect_poll_reminders must NOT call inbox::unread_count under the registry lock (#1617 class)"
        );
        assert!(
            prod[block_end..].contains(&io_needle),
            "inbox::unread_count must run AFTER the registry lock is dropped"
        );
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agend-poll-{}-{}-{}", std::process::id(), tag, id));
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
                    id: Some(format!("m-{agent}-{i}")),
                    from: "test".into(),
                    text: format!("msg {i}"),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    ..Default::default()
                },
            );
        }
    }

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
        let mut st = StateTracker::new(None);
        st.current = state;
        let core = AgentCore {
            vterm: crate::vterm::VTerm::new(24, 80),
            subscribers: Vec::new(),
            state: st,
            health: crate::health::HealthTracker::new(),
        };
        let handle = AgentHandle {
            id: crate::types::InstanceId::default(),
            name: name.to_string().into(),
            backend_command: "test".to_string(),
            pty_writer: Arc::new(Mutex::new(writer)),
            pty_master: Arc::new(Mutex::new(pair.master)),
            core: Arc::new(crate::sync_audit::CoreMutex::new(core)),
            child: Arc::new(Mutex::new(child)),
            submit_key: "\r".to_string(),
            inject_prefix: String::new(),
            typed_inject: false,
            spawned_at: std::time::Instant::now(),
            spawned_at_epoch_ms: 0,
            deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let reg: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        // #1441: registry is UUID-keyed — insert under the handle's own id.
        reg.lock().insert(handle.id, handle);
        reg
    }

    /// Reset dedup state for a specific agent to allow fresh test runs.
    fn reset_dedup(name: &str) {
        let mut guard = LAST_NOTIFIED.lock();
        if let Some(map) = guard.as_mut() {
            map.remove(name);
        }
    }

    #[test]
    fn test_collect_poll_reminders_returns_reminder_when_idle_with_unread() {
        let home = tmp_home("collect-idle");
        let agent = "collect-idle-agent";
        seed_unread(&home, agent, 3);
        let registry = mock_registry(agent, AgentState::Idle);
        reset_dedup(agent);

        let v = collect_poll_reminders(&home, &registry);
        assert_eq!(v.len(), 1, "should produce 1 reminder");
        assert_eq!(v[0].0, agent);
        assert!(v[0].1.contains("[AGEND-MSG]"), "must contain header prefix");
        assert!(v[0].1.contains("kind=poll-reminder"), "must contain kind");
        assert!(v[0].1.contains("unread=3"), "must contain unread count");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_collect_poll_reminders_dedupes_same_count() {
        let home = tmp_home("collect-dedup");
        let agent = "collect-dedup-agent";
        seed_unread(&home, agent, 3);
        let registry = mock_registry(agent, AgentState::Idle);
        reset_dedup(agent);

        let v1 = collect_poll_reminders(&home, &registry);
        assert_eq!(v1.len(), 1);

        let v2 = collect_poll_reminders(&home, &registry);
        assert!(
            v2.is_empty(),
            "second call with same count must be suppressed by dedup"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_collect_poll_reminders_re_notifies_on_count_change() {
        let home = tmp_home("collect-renotify");
        let agent = "collect-renotify-agent";
        seed_unread(&home, agent, 3);
        let registry = mock_registry(agent, AgentState::Idle);
        reset_dedup(agent);

        let v1 = collect_poll_reminders(&home, &registry);
        assert_eq!(v1.len(), 1);
        assert!(v1[0].1.contains("unread=3"));

        // Add 2 more unread → count changes to 5
        seed_unread(&home, agent, 2);
        let v2 = collect_poll_reminders(&home, &registry);
        assert_eq!(v2.len(), 1, "count changed → should re-notify");
        assert!(
            v2[0].1.contains("unread=5"),
            "must reflect new count, got: {}",
            v2[0].1
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_collect_poll_reminders_skips_when_busy() {
        let home = tmp_home("collect-busy");
        let agent = "collect-busy-agent";
        seed_unread(&home, agent, 5);
        let registry = mock_registry(agent, AgentState::Thinking);
        reset_dedup(agent);

        let v = collect_poll_reminders(&home, &registry);
        assert!(v.is_empty(), "busy agent must not get reminder");

        std::fs::remove_dir_all(&home).ok();
    }
}
