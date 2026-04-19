//! Per-agent supervisor loop — detects pre-ready interactive stalls and
//! pushes a vterm tail to the agent's Telegram topic.
//!
//! Runs as a background thread spawned from both daemon mode
//! (`start_daemon`) and app mode (`app::run`). Both call paths create agents
//! via the shared `AgentRegistry`, so the supervisor needs no state beyond a
//! registry handle and the AGEND_HOME path. Shutdown is implicit: when the
//! hosting process exits, this thread dies with it.
//!
//! Detection logic lives in `health::HealthTracker::check_awaiting_operator`
//! and the transition in `state::StateTracker::set_awaiting_operator`. This
//! module is the plumbing that glues them to Telegram notifications.

use super::telegram::notify_telegram;
use crate::agent::{self, AgentRegistry};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// How often the supervisor wakes to scan the registry.
const TICK: Duration = Duration::from_secs(10);
/// Vterm tail size pushed to Telegram when a stall is detected.
const TAIL_LINES: usize = 40;

/// Spawn the supervisor thread. Idempotent per process is the caller's
/// responsibility — in practice each entry point calls it exactly once.
pub fn spawn(home: PathBuf, registry: AgentRegistry) {
    let _ = thread::Builder::new()
        .name("supervisor".into())
        .spawn(move || run_loop(home, registry));
}

fn run_loop(home: PathBuf, registry: AgentRegistry) {
    loop {
        thread::sleep(TICK);
        tick(&home, &registry);
    }
}

/// One iteration of the supervisor loop. Public for tests.
fn tick(home: &std::path::Path, registry: &AgentRegistry) {
    // Snapshot the agent names + handles so we can release the registry lock
    // before touching any per-agent core lock. Holding both at once risks
    // deadlocks against code paths that take core then registry.
    let handles: Vec<(String, _)> = {
        let reg = agent::lock_registry(registry);
        reg.iter()
            .map(|(n, h)| (n.clone(), Arc::clone(&h.core)))
            .collect()
    };

    for (name, core) in handles {
        // tail is pulled while we hold the core lock; Telegram IO happens
        // after we release it so slow-network pushes don't block other
        // agents' updates.
        let notify_payload: Option<String> = {
            let mut core = match core.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let agent_state = core.state.current;
            let silent = core.state.last_output.elapsed();
            let time_in_state = core.state.since.elapsed();
            if core
                .health
                .check_awaiting_operator(agent_state, silent, time_in_state)
            {
                core.state.set_awaiting_operator();
                let tail = core.vterm.tail_lines(TAIL_LINES);
                tracing::info!(
                    agent = %name,
                    silent_secs = silent.as_secs(),
                    prev_state = agent_state.display_name(),
                    "awaiting operator (stalled on interactive prompt)"
                );
                Some(format!(
                    "⚠️ {name} 靜默 {silent_secs}s，可能卡在互動 prompt\n\
                     ────────\n\
                     {tail}\n\
                     ────────\n\
                     💬 回覆將以原始鍵盤輸入寫入 agent stdin",
                    silent_secs = silent.as_secs(),
                ))
            } else {
                None
            }
        };

        if let Some(msg) = notify_payload {
            notify_telegram(home, &name, &msg);
        }
    }
}
