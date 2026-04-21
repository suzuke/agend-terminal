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

use super::telegram::{notify_telegram, notify_telegram_silent};
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
        // Mutate state + pull the tail under the core lock, then drop it
        // before running `format!` and the Telegram spawn. `tail_lines`
        // allocates a fresh String, so the lock window is bounded by the
        // vterm copy — no async IO or string formatting held against it.
        let action: Option<NoticeAction> = {
            let mut core = match core.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let agent_state = core.state.current;
            let silent = core.state.last_output.elapsed();
            if core.health.check_awaiting_operator(agent_state, silent) {
                core.state.set_awaiting_operator();
                tracing::info!(
                    agent = %name,
                    silent_secs = silent.as_secs(),
                    prev_state = agent_state.display_name(),
                    "awaiting operator (stalled on interactive prompt)"
                );
                // Consume the recovery flag if somehow armed in the same tick,
                // so the "ready again" ping doesn't fire right after we just
                // re-entered a blocked state.
                let _ = core.state.take_recovery_notice();
                Some(NoticeAction::Stall {
                    tail: core.vterm.tail_lines(TAIL_LINES),
                    silent_secs: Some(silent.as_secs()),
                })
            } else if core.state.take_interactive_prompt_notice() {
                // Pattern-based InteractivePrompt fires immediately on state
                // entry (no silence window), so the notice also goes out on
                // the first tick after entry rather than waiting for quiet.
                tracing::info!(
                    agent = %name,
                    "interactive prompt detected — forwarding to telegram"
                );
                let _ = core.state.take_recovery_notice();
                Some(NoticeAction::Stall {
                    tail: core.vterm.tail_lines(TAIL_LINES),
                    silent_secs: None,
                })
            } else if core.state.take_recovery_notice() {
                // Symmetric "ready again" signal: armed on the transition
                // out of InteractivePrompt / AwaitingOperator. Silent push so
                // operators aren't vibrated twice per interactive cycle.
                tracing::info!(
                    agent = %name,
                    "recovered from blocked state — notifying telegram"
                );
                Some(NoticeAction::Recovered)
            } else {
                None
            }
        };

        match action {
            Some(NoticeAction::Stall { tail, silent_secs }) => {
                let msg = format_stall_notice(&name, &tail, silent_secs);
                notify_telegram(home, &name, &msg);
            }
            Some(NoticeAction::Recovered) => {
                let msg = format_recovery_notice(&name);
                notify_telegram_silent(home, &name, &msg);
            }
            None => {}
        }
    }
}

/// Internal enum describing what the tick produced for a single agent, so the
/// Telegram send can run after the core lock has been released.
enum NoticeAction {
    Stall {
        tail: String,
        silent_secs: Option<u64>,
    },
    Recovered,
}

/// Build the Telegram notice shown when an agent is blocked on an interactive
/// prompt. `silent_secs = Some` for the AwaitingOperator time-based fallback
/// (reports how long the agent has been quiet); `None` for pattern-matched
/// InteractivePrompt (no silence window).
fn format_stall_notice(name: &str, tail: &str, silent_secs: Option<u64>) -> String {
    let header = match silent_secs {
        Some(s) => format!("⚠️ {name} 靜默 {s}s，可能卡在互動 prompt"),
        None => format!("⚠️ {name} 卡在互動 prompt"),
    };
    format!(
        "{header}\n\
         ────────\n\
         {tail}\n\
         ────────\n\
         💬 回覆將以原始鍵盤輸入寫入 agent stdin"
    )
}

/// Short, silent ping emitted when an agent leaves a blocked state
/// (InteractivePrompt / AwaitingOperator) and is ready for normal
/// conversation again.
fn format_recovery_notice(name: &str) -> String {
    format!("✅ {name} 已就緒，可以繼續對話")
}
