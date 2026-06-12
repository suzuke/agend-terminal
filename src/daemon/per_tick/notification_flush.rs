//! Daemon-side flush of the deferred notification queue.
//!
//! `compose_aware_inject` (#1513) defers notifications into the
//! `notification_queue` while the target agent is mid-generation or the
//! operator is mid-keystroke. The queue's only flusher used to be the TUI
//! event loop (`app/mod.rs::flush_idle_notifications`) — but the live
//! deployment can run headless (`agend-terminal start` → `run_core`), where
//! that loop never executes. Deferred items (operator Telegram messages
//! included) then strand forever: 7 user messages sat undelivered for hours
//! on 2026-06-10 while the target agent showed `thinking`.
//!
//! This handler closes the gap: every tick it scans the fleet's queues and
//! drains them through the SAME shared gating core the TUI flush uses
//! (`inbox::notify::flush_agent_queue` — draft-state, busy-hold, MAX_DEFER
//! anti-starvation caps), injecting via the submit-aware PTY path. Running it
//! alongside an attached TUI is safe because `notification_queue::drain` is
//! claim-atomic (rename): whichever flusher claims an item delivers it
//! exactly once.

use super::{PerTickHandler, TickContext};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct NotificationFlushHandler {
    every_n_ticks: u64,
    counter: AtomicU64,
}

impl NotificationFlushHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            every_n_ticks,
            counter: AtomicU64::new(0),
        }
    }

    /// Fires at tick indices 0, N, 2N, … (matches `PollReminderHandler`).
    fn should_fire(&self) -> bool {
        self.counter
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(self.every_n_ticks)
    }
}

impl PerTickHandler for NotificationFlushHandler {
    fn name(&self) -> &'static str {
        "notification_flush"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.should_fire() {
            return;
        }
        flush_all(ctx.home);
    }
}

/// Production entry: flush every fleet instance's deferred queue through the
/// submit-aware PTY injector (the same one the TUI flush uses — #982 contract:
/// queued hints must land WITH the backend submit key or one-shot backends
/// silently drop the wake).
pub(crate) fn flush_all(home: &Path) {
    flush_all_with(home, |agent, text| {
        crate::inbox::notify::inject_notification_with_submit(home, agent, text)
    });
}

/// Test-seam variant: `injector` receives `(agent_name, notification_text)`.
/// The busy/typing/draft GATING lives in the shared core
/// (`inbox::notify::flush_agent_queue`), so a test driving this entry asserts
/// exactly what the daemon tick delivers.
pub(crate) fn flush_all_with<F>(home: &Path, mut injector: F)
where
    F: FnMut(&str, &str) -> anyhow::Result<()>,
{
    let Ok(fleet) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) else {
        return;
    };
    for agent in fleet.instances.keys() {
        // Idle fast path: a read_dir + line count of any existing queue files.
        if crate::notification_queue::pending_count(home, agent) == 0 {
            continue;
        }
        // Shared core applies the SAME draft/busy/typing gating + MAX_DEFER
        // caps as the TUI flush; failed injects are requeued for next tick.
        crate::inbox::notify::flush_agent_queue(home, agent, |text| injector(agent, text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification_queue;
    use parking_lot::Mutex;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn tmp_home(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-notif-flush-{}-{}",
            suffix,
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn write_fleet(home: &std::path::Path) {
        std::fs::write(
            crate::fleet::fleet_yaml_path(home),
            "instances:\n  a:\n    backend: claude\n",
        )
        .expect("write fleet.yaml");
    }

    fn snapshot_state(home: &std::path::Path, agent: &str, state: &str) {
        crate::snapshot::save(
            home,
            &[crate::snapshot::AgentSnapshot {
                name: agent.to_string(),
                backend_command: "claude".to_string(),
                args: vec![],
                working_dir: None,
                submit_key: "\r".to_string(),
                health_state: "healthy".to_string(),
                agent_state: state.to_string(),
                silent_secs: 0,
                output_silent_secs: 0,
            }],
        );
    }

    /// Reproduction of the 2026-06-10 operator report: a Telegram message
    /// deferred while the agent was busy must be DELIVERED by the daemon-side
    /// flush once the agent settles. Headless `run_core` has no TUI loop, so
    /// before this handler existed the queue stranded forever.
    #[test]
    fn flushes_stranded_ambient_once_agent_settles() {
        let home = tmp_home("settled");
        write_fleet(&home);
        snapshot_state(&home, "a", "idle");
        notification_queue::enqueue(&home, "a", "[user:x via telegram] hello")
            .expect("enqueue stranded message");

        let delivered: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let d = delivered.clone();
        flush_all_with(&home, |agent, text| {
            d.lock().push((agent.to_string(), text.to_string()));
            Ok(())
        });

        let got = delivered.lock();
        assert_eq!(
            got.len(),
            1,
            "stranded ambient must be delivered by the daemon-side flush"
        );
        assert_eq!(got[0].0, "a");
        assert!(got[0].1.contains("hello"));
        drop(got);
        assert_eq!(
            notification_queue::pending_count(&home, "a"),
            0,
            "queue must be empty after flush"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Mid-token guard preserved: a busy agent HOLDS fresh items, but the
    /// ambient MAX_DEFER backstop releases them even while busy (anti-starvation
    /// — the exact contract `app::flush_release` pins for the TUI flush).
    #[test]
    fn busy_agent_holds_fresh_then_cap_releases() {
        let home = tmp_home("busy");
        write_fleet(&home);
        snapshot_state(&home, "a", "thinking");

        notification_queue::enqueue(&home, "a", "fresh").expect("enqueue fresh");
        let count = Arc::new(Mutex::new(0usize));
        let c = count.clone();
        flush_all_with(&home, |_, _| {
            *c.lock() += 1;
            Ok(())
        });
        assert_eq!(*count.lock(), 0, "fresh ambient held while agent busy");
        assert_eq!(
            notification_queue::pending_count(&home, "a"),
            1,
            "held item stays queued"
        );

        // Age the queued item past the ambient cap (7s), then the backstop must
        // release it even though the agent is still busy.
        let mut items = notification_queue::drain(&home, "a");
        assert_eq!(items.len(), 1, "test setup: claim the held item to age it");
        items[0].deferred_since_ms -= 8_000;
        notification_queue::requeue_all(&home, "a", &items);

        flush_all_with(&home, |_, _| {
            *c.lock() += 1;
            Ok(())
        });
        assert_eq!(
            *count.lock(),
            1,
            "ambient past its MAX_DEFER cap must release even while the agent is busy"
        );
        assert_eq!(notification_queue::pending_count(&home, "a"), 0);
        std::fs::remove_dir_all(&home).ok();
    }

    /// A failed inject must requeue (not drop) — the next tick retries.
    #[test]
    fn failed_inject_requeues_for_next_tick() {
        let home = tmp_home("requeue");
        write_fleet(&home);
        snapshot_state(&home, "a", "idle");
        notification_queue::enqueue(&home, "a", "msg").expect("enqueue");

        flush_all_with(&home, |_, _| anyhow::bail!("pane not ready"));
        assert_eq!(
            notification_queue::pending_count(&home, "a"),
            1,
            "failed inject must keep the item queued for the next tick"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// The handler must be part of the default daemon pipeline — headless
    /// `run_core` is exactly where the TUI flush doesn't exist.
    #[test]
    fn registered_in_default_daemon_pipeline() {
        let (crash_tx, _crash_rx) = crossbeam_channel::bounded(1);
        let stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handlers = crate::daemon::build_default_handlers(crash_tx, false, stale);
        assert!(
            handlers.iter().any(|h| h.name() == "notification_flush"),
            "NotificationFlushHandler must run in headless run_core — \
             the TUI flush loop does not exist there"
        );
    }

    #[test]
    fn fires_at_expected_cadence() {
        let h = NotificationFlushHandler::new(3);
        let fires: Vec<bool> = (0..7).map(|_| h.should_fire()).collect();
        assert_eq!(fires, vec![true, false, false, true, false, false, true]);
    }

    #[test]
    fn name_matches_module() {
        assert_eq!(
            NotificationFlushHandler::new(1).name(),
            "notification_flush"
        );
    }
}
