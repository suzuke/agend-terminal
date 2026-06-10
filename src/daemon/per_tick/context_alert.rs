//! Context% alert — operator-directed early warning when an agent's context
//! usage crosses the alert threshold. Detection + notification ONLY: the
//! alert goes to the agent's team orchestrator (and the usage is visible via
//! LIST `context_pct`/`context_source`); nothing is auto-restarted.
//!
//! Source resolution per agent (see `StateTracker::resolved_context`):
//! 1. `pattern` — the agent's own statusline percent, parsed by the PTY
//!    feed (cheap, in-band). Preferred when fresh.
//! 2. `transcript` — for Claude agents whose pattern can't be read (narrow
//!    pane truncation / no statusline), this handler computes the estimate
//!    from the newest transcript's LAST message usage. That file IO runs
//!    HERE — on the tick, with no registry/core lock held — never in the
//!    PTY reader's feed path.
//! 3. otherwise unknown — no alert, honest `null` in LIST.
//!
//! Dedup/hysteresis: an alert fires on crossing `>= threshold` while armed;
//! firing disarms; re-arming requires dropping below `threshold -
//! HYSTERESIS_PCT` (no 79↔81 flapping); a continuously-high agent re-alerts
//! every [`REALERT_AFTER`]. State is in-memory: a daemon restart re-fires
//! once — accepted (current-state alert, single, self-limiting).

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Default alert threshold (percent). Override: `AGEND_CONTEXT_ALERT_PCT`.
const DEFAULT_ALERT_PCT: f32 = 80.0;
/// Re-arm requires dropping this far below the threshold (compact/restart),
/// so boundary noise can't re-fire.
const HYSTERESIS_PCT: f32 = 5.0;
/// Re-alert cadence while usage stays continuously above the threshold.
const REALERT_AFTER: Duration = Duration::from_secs(30 * 60);

fn alert_threshold() -> f32 {
    std::env::var("AGEND_CONTEXT_ALERT_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ALERT_PCT)
}

/// Per-agent alert latch.
struct AlertState {
    /// Armed = the next threshold crossing alerts immediately. Disarmed by a
    /// fire; re-armed once the pct drops below `threshold - HYSTERESIS_PCT`.
    armed: bool,
    last_alert: Option<Instant>,
}

impl Default for AlertState {
    fn default() -> Self {
        Self {
            armed: true,
            last_alert: None,
        }
    }
}

/// Pure alert decision: should an alert fire NOW for this reading? Mutates
/// the per-agent latch accordingly (fire disarms + stamps; a low reading
/// below the hysteresis floor re-arms).
fn decide(state: &mut AlertState, pct: f32, threshold: f32, now: Instant) -> bool {
    if pct >= threshold {
        let realert_due = state
            .last_alert
            .is_none_or(|t| now.saturating_duration_since(t) >= REALERT_AFTER);
        if state.armed || realert_due {
            state.armed = false;
            state.last_alert = Some(now);
            return true;
        }
        return false;
    }
    if pct < threshold - HYSTERESIS_PCT {
        state.armed = true;
    }
    false
}

pub(crate) struct ContextAlertHandler {
    every_n_ticks: u64,
    counter: AtomicU64,
    states: Mutex<HashMap<String, AlertState>>,
}

impl ContextAlertHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            every_n_ticks,
            counter: AtomicU64::new(0),
            states: Mutex::new(HashMap::new()),
        }
    }

    /// Fires at tick indices 0, N, 2N, … (matches `InboxStuckHandler`).
    fn should_fire(&self) -> bool {
        self.counter
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(self.every_n_ticks)
    }
}

impl PerTickHandler for ContextAlertHandler {
    fn name(&self) -> &'static str {
        "context_alert"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.should_fire() {
            return;
        }

        // Phase 1 (cheap, locks only): snapshot resolved context per agent;
        // collect the Claude agents that need a transcript-estimate refresh.
        let mut resolved: Vec<(String, f32, &'static str)> = Vec::new();
        let mut need_estimate = Vec::new();
        {
            let reg = crate::agent::lock_registry(ctx.registry);
            for handle in reg.values() {
                let name = handle.name.as_str().to_string();
                match handle.core.lock().state.resolved_context() {
                    Some((pct, source)) => resolved.push((name, pct, source)),
                    // The estimator reads Claude transcripts — only Claude
                    // agents can produce one; everything else stays unknown.
                    None if handle.backend_command.contains("claude") => {
                        need_estimate.push((name, std::sync::Arc::clone(&handle.core)));
                    }
                    None => {}
                }
            }
        }

        // Phase 2 (file IO, NO locks held during the read): refresh estimates,
        // then store back under a short core lock so LIST surfaces them.
        for (name, core) in need_estimate {
            if let Some(pct) = crate::token_cost::estimate_context_pct(ctx.home, &name) {
                core.lock().state.set_context_estimate(pct);
                resolved.push((name, pct, "transcript"));
            }
        }

        // Phase 3: threshold/hysteresis evaluation + orchestrator notify.
        let threshold = alert_threshold();
        let now = Instant::now();
        let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(ctx.home))
            .unwrap_or_default();
        let mut states = self.states.lock();
        for (name, pct, source) in resolved {
            let state = states.entry(name.clone()).or_default();
            if !decide(state, pct, threshold, now) {
                continue;
            }
            // Notify the agent's team orchestrator. Never the agent about
            // itself (it can't act on an alert it isn't reading) — matches
            // the inbox-stuck watchdog's recipient rule.
            let recipient = crate::daemon::inbox_stuck_watchdog::orchestrator_for(&fleet, &name)
                .unwrap_or_else(|| {
                    crate::daemon::inbox_stuck_watchdog::FALLBACK_RECIPIENT.to_string()
                });
            if recipient == name {
                continue;
            }
            let text = format!(
                "[context_alert] agent '{name}' context usage at {pct:.0}% \
                 (source: {source}, threshold {threshold:.0}%). Handling is NOT \
                 automated — at a natural boundary consider a handoff + \
                 restart_instance to free the context. Re-alerts every 30min \
                 while it stays high."
            );
            if let Err(e) = crate::inbox::notify_system(
                ctx.home,
                &recipient,
                "system:context_alert",
                "context_alert",
                text,
                Some(&name),
                None,
            ) {
                tracing::warn!(agent = %name, %recipient, error = %e, "context_alert: notify failed");
                continue;
            }
            tracing::info!(agent = %name, %recipient, pct, source, "context_alert: alerted orchestrator");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: f32 = 80.0;

    /// Crossing the threshold while armed fires once; staying high does not
    /// re-fire within the re-alert window.
    #[test]
    fn fires_on_crossing_then_dedups_while_high() {
        let mut s = AlertState::default();
        let now = Instant::now();
        assert!(decide(&mut s, 81.0, T, now), "armed crossing fires");
        assert!(
            !decide(&mut s, 85.0, T, now),
            "still high, within window — no re-fire"
        );
        assert!(!decide(&mut s, 99.0, T, now), "still no re-fire");
    }

    /// Continuously-high usage re-alerts once the re-alert window elapses.
    #[test]
    fn realerts_after_window_while_still_high() {
        let mut s = AlertState::default();
        let now = Instant::now();
        assert!(decide(&mut s, 90.0, T, now));
        let later = now + REALERT_AFTER;
        assert!(decide(&mut s, 90.0, T, later), "re-alert due after window");
        assert!(!decide(&mut s, 90.0, T, later), "and dedups again");
    }

    /// Dropping below the threshold but ABOVE the hysteresis floor does NOT
    /// re-arm — a 79↔81 boundary wobble cannot fire repeatedly.
    #[test]
    fn boundary_wobble_does_not_refire() {
        let mut s = AlertState::default();
        let now = Instant::now();
        assert!(decide(&mut s, 81.0, T, now));
        assert!(!decide(&mut s, 79.0, T, now), "below threshold — no fire");
        assert!(
            !decide(&mut s, 81.0, T, now),
            "wobble back above must not re-fire (not re-armed, window not due)"
        );
    }

    /// Dropping below the hysteresis floor (compact/restart) re-arms, so the
    /// next genuine crossing alerts immediately.
    #[test]
    fn compact_rearms_next_crossing_fires() {
        let mut s = AlertState::default();
        let now = Instant::now();
        assert!(decide(&mut s, 85.0, T, now));
        assert!(
            !decide(&mut s, 40.0, T, now),
            "compact drop — re-arms, no fire"
        );
        assert!(
            decide(&mut s, 82.0, T, now),
            "fresh crossing fires immediately"
        );
    }

    /// Below-threshold readings never fire, armed or not.
    #[test]
    fn below_threshold_never_fires() {
        let mut s = AlertState::default();
        let now = Instant::now();
        assert!(!decide(&mut s, 0.0, T, now));
        assert!(!decide(&mut s, 79.9, T, now));
    }
}
