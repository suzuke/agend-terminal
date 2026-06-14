//! M3 (#2090) — progress backstop watchdog (report mode).
//!
//! When `runtime_config.progress_mode == 2` (report mode), the AGENT owns its
//! own progress updates (per the injected "Long-Task Progress Reporting"
//! directive) — the daemon does NOT mirror the transcript. This handler is the
//! daemon-side BACKSTOP for that contract: if an agent has an ACTIVE
//! external-channel turn that's been running a while with NO user-facing reply
//! yet, nudge the AGENT (inject `[AGEND-AUTO kind=progress-backstop]`) to post a
//! brief progress update itself.
//!
//! It never sends to Telegram/Discord directly — it prods the agent to
//! self-report, so the operator-visible message is clean and agent-authored.
//!
//! Fail-open: the mode gate + cadence gate keep the common case free; every
//! per-agent step is best-effort and panic-isolated so one bad agent can't kill
//! the sweep. A per-agent debounce prevents nudge spam on a genuinely long turn.

use super::{PerTickHandler, TickContext};
use crate::daemon::cadence_gate::CadenceGate;
use crate::daemon::heartbeat_pair;
use crate::reply_ledger::ReplyOutcome;
use parking_lot::Mutex;
use std::collections::HashMap;

/// How long an active, un-replied external-channel turn must run before the
/// backstop nudges the agent to self-report. Comfortably past a normal
/// quick-reply turn — only genuinely long tasks trip it.
const BACKSTOP_THRESHOLD_MS: i64 = 45_000;

/// Minimum gap between two backstop nudges for the SAME agent — a long task
/// gets reminded periodically, not on every cadence fire.
const BACKSTOP_DEBOUNCE_MS: i64 = 60_000;

pub(crate) struct ProgressBackstopHandler {
    gate: CadenceGate,
    /// Per-agent last-nudge epoch-ms (keyed by agent name). Absence == never
    /// nudged.
    last_nudge: Mutex<HashMap<String, i64>>,
}

impl ProgressBackstopHandler {
    pub(crate) fn new() -> Self {
        Self {
            // ~30s cadence at the 10s tick (fire every 3rd tick), plus the
            // shared notification boot-grace so a restart can't false-fire the
            // backstop for a turn that simply hasn't had time to reply yet.
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
        // Mode gate: only report mode (2) backstops here. 0 = off, 1 = mirror
        // (the daemon relays the transcript itself) — both skip this handler.
        if crate::runtime_config::get().progress_mode != 2 {
            return;
        }
        if !self.gate.fire() {
            return;
        }

        let now = heartbeat_pair::now_ms() as i64;

        // Snapshot agent names under the configs lock, then release it before
        // any heartbeat-pair reads / PTY injects.
        let names: Vec<String> = {
            let configs = ctx.configs.lock();
            configs.keys().cloned().collect()
        };

        let mut last_nudge = self.last_nudge.lock();
        for name in names {
            // Per-agent panic isolation: one malformed turn / inject hiccup
            // must not abort the rest of the sweep.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Self::backstop_agent(ctx.home, ctx.registry, &name, now, &mut last_nudge);
            }));
            if result.is_err() {
                tracing::warn!(agent = %name, "progress_backstop: per-agent panic isolated");
            }
        }
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
        let snap = {
            let reg = agent::lock_registry(registry);
            crate::fleet::resolve_uuid(home, name)
                .and_then(|id| reg.get(&id))
                .map(agent::InjectTarget::from_handle)
        };
        if let Some(tgt) = snap {
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

    // ── should_nudge (pure) ─────────────────────────────────────────────
    #[test]
    fn should_nudge_active_pending_old_not_recently_nudged() {
        // Active channel + pending + armed past the threshold + never nudged.
        assert!(should_nudge(
            Some("telegram"),
            true,
            Some(0),
            None,
            BACKSTOP_THRESHOLD_MS,
        ));
    }

    #[test]
    fn should_not_nudge_when_no_channel() {
        assert!(!should_nudge(
            None,
            true,
            Some(0),
            None,
            BACKSTOP_THRESHOLD_MS,
        ));
    }

    #[test]
    fn should_not_nudge_when_not_pending() {
        // Reply already delivered (or send-failed) → not a silent pending turn.
        assert!(!should_nudge(
            Some("telegram"),
            false,
            Some(0),
            None,
            BACKSTOP_THRESHOLD_MS,
        ));
    }

    #[test]
    fn should_not_nudge_when_too_fresh() {
        // Armed only a moment ago — under the threshold.
        assert!(!should_nudge(
            Some("telegram"),
            true,
            Some(BACKSTOP_THRESHOLD_MS - 1),
            None,
            BACKSTOP_THRESHOLD_MS,
        ));
    }

    #[test]
    fn should_not_nudge_when_recently_nudged() {
        // Old turn, but nudged within the debounce window.
        let now = BACKSTOP_THRESHOLD_MS + 10_000;
        assert!(!should_nudge(
            Some("telegram"),
            true,
            Some(0),
            Some(now - (BACKSTOP_DEBOUNCE_MS - 1)),
            now,
        ));
    }

    #[test]
    fn should_nudge_again_after_debounce_window() {
        // Long task, last nudge older than the debounce window → re-nudge.
        let now = 1_000_000;
        assert!(should_nudge(
            Some("telegram"),
            true,
            Some(0),
            Some(now - BACKSTOP_DEBOUNCE_MS),
            now,
        ));
    }

    #[test]
    fn should_not_nudge_when_no_armed_timestamp() {
        assert!(!should_nudge(Some("telegram"), true, None, None, 999_999));
    }
}
