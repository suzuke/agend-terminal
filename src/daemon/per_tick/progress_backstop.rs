//! #2090 M3 — progress-backstop watchdog (report mode).
//!
//! When `runtime_config.progress_mode == 2` (report), an agent OWNS its own
//! progress updates: it self-reports on the channel a request came from. This
//! per-tick watchdog is the safety net — if an external-channel turn has been
//! armed longer than [`BACKSTOP_THRESHOLD_MS`] with the reply still `Pending`
//! and no recent nudge, it injects a short `[AGEND-AUTO kind=progress-backstop]`
//! prompt asking the agent to post a brief update itself.
//!
//! Crucially the daemon NEVER authors or relays the user-facing content — it
//! only prods the agent. So unlike a raw-stream mirror, no transcript / raw
//! assistant output is ever sent to an external channel (zero exfil surface).
//!
//! Fail-open: every per-agent step is best-effort and panic-isolated so one bad
//! agent can't abort the sweep.

use super::{PerTickHandler, TickContext};
use crate::daemon::cadence_gate::CadenceGate;
use crate::daemon::heartbeat_pair;
use crate::reply_ledger::ReplyOutcome;
use parking_lot::Mutex;
use std::collections::HashMap;

/// An external-channel turn must be armed at least this long before we nudge —
/// generous so the agent gets room to self-report on its own first.
const BACKSTOP_THRESHOLD_MS: i64 = 45_000;
/// Don't re-nudge the same agent within this window (anti-spam debounce).
const BACKSTOP_DEBOUNCE_MS: i64 = 60_000;

pub(crate) struct ProgressBackstopHandler {
    gate: CadenceGate,
    /// Per-agent last-nudge timestamp (ms). Bounded each run by pruning entries
    /// for agents no longer in the live config snapshot (cleanup-on-delete) so
    /// the map can't grow without limit across a long-lived daemon.
    last_nudge: Mutex<HashMap<String, i64>>,
}

impl ProgressBackstopHandler {
    pub(crate) fn new() -> Self {
        Self {
            // ~30s cadence, suppressed during the boot grace so a daemon restart
            // doesn't nudge every in-flight turn at once.
            gate: CadenceGate::new_with_boot_grace(3, super::NOTIFICATION_BOOT_GRACE),
            last_nudge: Mutex::new(HashMap::new()),
        }
    }
}

impl PerTickHandler for ProgressBackstopHandler {
    fn name(&self) -> &'static str {
        "progress_backstop"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        // Mode gate: only report mode (2). 0 = off, 1 = mirror (not yet active).
        if crate::runtime_config::get().progress_mode != 2 {
            return;
        }
        if !self.gate.fire() {
            return;
        }

        let now = heartbeat_pair::now_ms() as i64;
        let names: Vec<String> = {
            let configs = ctx.configs.lock();
            configs.keys().cloned().collect()
        };

        let mut last_nudge = self.last_nudge.lock();
        for name in &names {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Self::backstop_agent(ctx.home, ctx.registry, name, now, &mut last_nudge);
            }));
            if result.is_err() {
                tracing::warn!(agent = %name, "progress_backstop: per-agent panic isolated");
            }
        }
        // Bound the latch: drop entries for agents that no longer exist (a
        // deleted/renamed agent must not leave a stale debounce timer behind).
        let live: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();
        last_nudge.retain(|name, _| live.contains(name.as_str()));
    }
}

impl ProgressBackstopHandler {
    fn backstop_agent(
        home: &std::path::Path,
        registry: &crate::agent::AgentRegistry,
        name: &str,
        now: i64,
        last_nudge: &mut HashMap<String, i64>,
    ) {
        let snap = heartbeat_pair::snapshot_for(name);
        let channel = snap.reply_to_channel.clone();
        let turn = snap.pending_user_turn.as_ref();
        let outcome_pending = turn
            .map(|t| t.reply_outcome == ReplyOutcome::Pending)
            .unwrap_or(false);
        let armed_at_ms = turn.map(|t| t.armed_at_ms);

        if !should_nudge(
            channel.as_deref(),
            outcome_pending,
            armed_at_ms,
            last_nudge.get(name).copied(),
            now,
        ) {
            return;
        }

        // `should_nudge` proved the channel + armed timestamp are present.
        let (Some(channel), Some(armed)) = (channel.as_deref(), armed_at_ms) else {
            return;
        };
        Self::inject_backstop(home, registry, name, channel, armed, now);
        last_nudge.insert(name.to_string(), now);
    }

    /// Resolve the live agent under the registry lock (released before the
    /// blocking PTY write) and inject the self-report nudge, tagged
    /// `[AGEND-AUTO kind=progress-backstop]` so an orchestrator doesn't mistake
    /// it for an operator command. Best-effort — a missing/failed target drops.
    fn inject_backstop(
        home: &std::path::Path,
        registry: &crate::agent::AgentRegistry,
        name: &str,
        channel: &str,
        armed_at_ms: i64,
        now: i64,
    ) {
        use crate::agent;
        let target = {
            let reg = agent::lock_registry(registry);
            crate::fleet::resolve_uuid(home, name)
                .and_then(|id| reg.get(&id))
                .map(agent::InjectTarget::from_handle)
        };
        if let Some(tgt) = target {
            let payload = crate::reply_ledger::backstop_nudge_text(channel, armed_at_ms, now);
            let _ = agent::inject_with_target_gated(
                &tgt,
                name,
                payload.as_bytes(),
                false,
                Some("progress-backstop"),
            );
        }
    }
}

/// Pure "should this agent be nudged?" decision — factored out so the per-tick
/// `run` stays thin and the threshold/debounce logic is unit-testable.
///
/// True iff the agent has an ACTIVE external-channel turn (`reply_to_channel`
/// present) whose reply is still `Pending`, that's been armed at least
/// [`BACKSTOP_THRESHOLD_MS`], and that hasn't been nudged within
/// [`BACKSTOP_DEBOUNCE_MS`]. Anything missing → false (fail-open: no nudge).
fn should_nudge(
    reply_to_channel: Option<&str>,
    outcome_pending: bool,
    armed_at_ms: Option<i64>,
    last_nudge: Option<i64>,
    now: i64,
) -> bool {
    // No active external-channel turn → idle, never nudge.
    if reply_to_channel.is_none() {
        return false;
    }
    // Reply already delivered / send-failed → the turn isn't silently pending.
    if !outcome_pending {
        return false;
    }
    let Some(armed) = armed_at_ms else {
        return false;
    };
    // Not running long enough yet → give the agent room to reply on its own.
    if now.saturating_sub(armed) < BACKSTOP_THRESHOLD_MS {
        return false;
    }
    // Debounce: recently nudged → wait out the window.
    if let Some(last) = last_nudge {
        if now.saturating_sub(last) < BACKSTOP_DEBOUNCE_MS {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const ARMED: i64 = 1_000_000;

    #[test]
    fn nudges_when_pending_external_turn_runs_long() {
        // Armed external turn, pending, 45s elapsed, never nudged → nudge.
        assert!(should_nudge(
            Some("telegram"),
            true,
            Some(ARMED),
            None,
            ARMED + BACKSTOP_THRESHOLD_MS,
        ));
    }

    #[test]
    fn no_nudge_without_external_channel() {
        // Idle agent (no origin channel) is never nudged, however long.
        assert!(!should_nudge(
            None,
            true,
            Some(ARMED),
            None,
            ARMED + 10 * BACKSTOP_THRESHOLD_MS,
        ));
    }

    #[test]
    fn no_nudge_when_reply_not_pending() {
        // Reply already delivered / send-failed → not silently pending.
        assert!(!should_nudge(
            Some("telegram"),
            false,
            Some(ARMED),
            None,
            ARMED + BACKSTOP_THRESHOLD_MS,
        ));
    }

    #[test]
    fn no_nudge_before_threshold() {
        // 1ms short of the threshold → still give the agent room.
        assert!(!should_nudge(
            Some("telegram"),
            true,
            Some(ARMED),
            None,
            ARMED + BACKSTOP_THRESHOLD_MS - 1,
        ));
    }

    #[test]
    fn no_nudge_within_debounce_window() {
        // Long-armed + pending, but nudged 1ms ago → debounced.
        let now = ARMED + 10 * BACKSTOP_THRESHOLD_MS;
        assert!(!should_nudge(
            Some("telegram"),
            true,
            Some(ARMED),
            Some(now - 1),
            now,
        ));
    }

    #[test]
    fn nudges_again_after_debounce_window() {
        let now = ARMED + 10 * BACKSTOP_THRESHOLD_MS;
        assert!(should_nudge(
            Some("telegram"),
            true,
            Some(ARMED),
            Some(now - BACKSTOP_DEBOUNCE_MS),
            now,
        ));
    }

    #[test]
    fn no_nudge_when_armed_timestamp_missing() {
        assert!(!should_nudge(Some("telegram"), true, None, None, ARMED));
    }
}
