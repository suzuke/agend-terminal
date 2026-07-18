//! Context% alert — operator-directed early warning when an agent's context
//! usage crosses the alert threshold. Detection + notification ONLY: the
//! alert goes to the agent's team orchestrator (and the usage is visible via
//! LIST `context_pct`/`context_source`); nothing is auto-restarted.
//!
//! Source per agent (see `StateTracker::resolved_context`): the statusline
//! `pattern` ONLY — a pane whose statusline can't be read is honestly
//! `unknown` (no alert, `null` in LIST).
//!
//! #1945-disable (operator decision, 2026-06-10): the transcript-estimate
//! fallback is DISABLED — its first live minute fired a triple false 100%
//! alert (window misjudge). The corrected estimator survives, tested but
//! uncalled, in `token_cost::estimate_context_pct`; re-enable ONLY after
//! validating its readings against statusline ground truth.
//!
//! Dedup/hysteresis: an alert fires on crossing `>= threshold` while armed;
//! firing disarms; re-arming requires dropping below `threshold -
//! HYSTERESIS_PCT` (no 79↔81 flapping); a continuously-high agent re-alerts
//! every [`REALERT_AFTER`]. State is in-memory: a daemon restart re-fires
//! once — accepted (current-state alert, single, self-limiting).

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Re-alert cadence while usage stays continuously above the threshold.
const REALERT_AFTER: Duration = Duration::from_secs(30 * 60);

fn alert_threshold() -> f32 {
    let (alert, _, _) = crate::runtime_config::resolve_effective_thresholds();
    alert
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
    if pct < threshold - crate::runtime_config::HYSTERESIS_PCT {
        state.armed = true;
    }
    false
}

pub(crate) struct ContextAlertHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
    states: Mutex<HashMap<String, AlertState>>,
}

impl ContextAlertHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
            states: Mutex::new(HashMap::new()),
        }
    }

    /// Test-only: whether `name`'s alert latch is currently armed (`None` if
    /// the agent has no latch entry yet). Used by the #2549 W5 merge's
    /// cross-independence pin — proves `ContextHandoffHandler` firing never
    /// touches this handler's OWN latch, and vice versa (P2-2549-SPIKE.md
    /// §3c: the two handlers' latch/hysteresis state must stay independent
    /// after the merge).
    #[cfg(test)]
    pub(crate) fn is_armed(&self, name: &str) -> Option<bool> {
        self.states.lock().get(name).map(|s| s.armed)
    }
}

impl PerTickHandler for ContextAlertHandler {
    fn name(&self) -> &'static str {
        "context_alert"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }

        // Phase 1 (cheap, locks only): snapshot each agent's resolved context
        // — statusline pattern only (#1945-disable: no transcript estimate;
        // an unreadable pane is unknown and never alerts).
        let mut resolved: Vec<(String, f32, &'static str)> = Vec::new();
        // #latch-prune (cleanup-on-delete, #1923 G5 class): capture ALL live
        // agent names — not just those with a context reading — so the per-agent
        // `states` latch can drop deleted agents below. Without it a same-name
        // redeploy inherits a stale armed/alerted latch.
        let live: std::collections::HashSet<String> = {
            let reg = crate::agent::lock_registry(ctx.registry);
            let mut live = std::collections::HashSet::new();
            for handle in reg.values() {
                live.insert(handle.name.as_str().to_string());
                if let Some((pct, source)) = handle.core.lock().state.resolved_context() {
                    resolved.push((handle.name.as_str().to_string(), pct, source));
                }
            }
            live
        };

        // Phase 2: threshold/hysteresis evaluation + orchestrator notify.
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
                "[context_alert] agent '{name}' context usage at {pct:.1}% \
                 (source: {source}, threshold {threshold:.1}%). Handling is NOT \
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
        // #latch-prune: drop latch entries for agents gone from the registry
        // (cleanup-on-delete) so a deleted agent leaves no stale state.
        states.retain(|name, _| live.contains(name));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;

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

    /// #latch-prune (cleanup-on-delete, #1923 G5 class): a latch entry for an
    /// agent no longer in the registry (deleted) is dropped on the next `run`,
    /// via the REAL handler entry (empty registry = the agent was deleted) — so
    /// a same-name redeploy never inherits stale alert state.
    #[test]
    fn deleted_agent_latch_pruned_on_run() {
        use parking_lot::Mutex as PLMutex;
        use std::collections::HashMap;
        use std::sync::Arc;
        let home =
            std::env::temp_dir().join(format!("agend-ctxalert-prune-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let registry: crate::agent::AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let externals: crate::agent::ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs: Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>> =
            Arc::new(PLMutex::new(HashMap::new()));
        let h = ContextAlertHandler::new(1); // fire every tick (no boot-grace)
        h.states
            .lock()
            .insert("ghost".to_string(), AlertState::default());
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        h.run(&ctx); // real entry: live={} from the empty registry → retain
        assert!(
            !h.states.lock().contains_key("ghost"),
            "a deleted agent's latch must be pruned on run (cleanup-on-delete)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #latch-prune reverse-regression (reviewer-2 #2097): the prune's `live`
    /// set MUST capture EVERY live agent — NOT just those with a context
    /// reading this tick. A live agent without a reading keeps its latch; if a
    /// future edit moved `live.insert` INTO the `resolved_context()` `if`
    /// (degrading `live` to the reading-subset), this agent would be wrongly
    /// pruned + re-alerted (worse than the leak). Drives the REAL `run()` with a
    /// live agent that produces NO context reading; asserts its latch SURVIVES.
    #[test]
    fn live_agent_without_context_reading_keeps_latch() {
        use parking_lot::Mutex as PLMutex;
        use std::collections::HashMap;
        use std::sync::Arc;
        let home = std::env::temp_dir().join(format!("agend-ctxalert-keep-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let registry: crate::agent::AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let (handle, _reader) = crate::daemon::per_tick::mock_live_agent_no_context("alive");
        registry.lock().insert(handle.id, handle);
        let externals: crate::agent::ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs: Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>> =
            Arc::new(PLMutex::new(HashMap::new()));
        let h = ContextAlertHandler::new(1);
        h.states
            .lock()
            .insert("alive".to_string(), AlertState::default());
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        h.run(&ctx);
        assert!(
            h.states.lock().contains_key("alive"),
            "a LIVE agent with no context reading must KEEP its latch — `live.insert` must be \
             UNCONDITIONAL, not gated on resolved_context()"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial(runtime_config)]
    fn alert_threshold_precedence() {
        let temp_dir = std::env::temp_dir().join("agend-test-clean-alert");
        std::fs::create_dir_all(&temp_dir).ok();

        // 1. Write non-default valid config to check loader/consumer fallback
        std::fs::write(
            temp_dir.join("runtime-config.json"),
            r#"{"schema_version": 1, "context_alert_pct": 60.0, "context_handoff_pct": 70.0, "context_handoff_escalate_pct": 80.0}"#,
        )
        .unwrap();
        crate::runtime_config::reload(&temp_dir);

        let old_env = std::env::var("AGEND_CONTEXT_ALERT_PCT").ok();
        std::env::remove_var("AGEND_CONTEXT_ALERT_PCT");

        // Runtime config non-default value resolved
        assert_eq!(alert_threshold(), 60.0);

        // 2. Env var set overrides config
        std::env::set_var("AGEND_CONTEXT_ALERT_PCT", "55.5");
        assert_eq!(alert_threshold(), 55.5);

        // 3. Invalid env var resolved combination falls back to config value
        // alert 95.0, handoff in config is 70.0 -> invalid triplet combination (alert >= handoff), should fallback to config (60.0)
        std::env::set_var("AGEND_CONTEXT_ALERT_PCT", "95.0");
        assert_eq!(alert_threshold(), 60.0);

        // Restore env var
        if let Some(val) = old_env {
            std::env::set_var("AGEND_CONTEXT_ALERT_PCT", val);
        } else {
            std::env::remove_var("AGEND_CONTEXT_ALERT_PCT");
        }

        // Clean up global config back to default
        std::fs::write(
            temp_dir.join("runtime-config.json"),
            r#"{"schema_version": 1}"#,
        )
        .unwrap();
        crate::runtime_config::reload(&temp_dir);
        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// #2781: the PRODUCTION context_alert message must render usage and
    /// threshold with one decimal place. Drives the real ContextAlertHandler
    /// with a mock agent at 82.0% (above the 80.0% default threshold),
    /// drains the orchestrator inbox, and asserts the rendered text contains
    /// one-decimal values.
    #[test]
    #[serial(runtime_config)]
    fn context_alert_production_renders_one_decimal() {
        use parking_lot::Mutex as PLMutex;
        use std::collections::HashMap;
        use std::sync::Arc;

        let home =
            std::env::temp_dir().join(format!("agend-ctxalert-decimal-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();

        // Ensure default thresholds (alert=80.0).
        std::fs::write(home.join("runtime-config.json"), r#"{"schema_version": 1}"#).unwrap();
        crate::runtime_config::reload(&home);
        std::env::remove_var("AGEND_CONTEXT_ALERT_PCT");

        // Fleet with a team so alert routes to orchestrator.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  lead:\n    backend: claude\n  watched:\n    backend: claude\n\
             teams:\n  test:\n    members: [lead, watched]\n    orchestrator: lead\n",
        )
        .unwrap();

        let registry: crate::agent::AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let (handle, _reader) =
            crate::daemon::per_tick::mock_live_agent_with_context("watched", 82.0);
        registry.lock().insert(handle.id, handle);
        let externals: crate::agent::ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs: Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>> =
            Arc::new(PLMutex::new(HashMap::new()));

        let h = ContextAlertHandler::new(1);
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        h.run(&ctx);

        // Drain orchestrator inbox and check the rendered message.
        let msgs = crate::inbox::drain(&home, "lead");
        let alert_msg = msgs
            .iter()
            .find(|m| m.text.contains("[context_alert]"))
            .expect("#2781: context_alert must deliver an inbox message to the orchestrator");
        assert!(
            alert_msg.text.contains("82.0%"),
            "#2781: usage must render one decimal: {}",
            alert_msg.text
        );
        assert!(
            alert_msg.text.contains("80.0%"),
            "#2781: threshold must render one decimal: {}",
            alert_msg.text
        );

        // Clean up.
        std::fs::write(home.join("runtime-config.json"), r#"{"schema_version": 1}"#).unwrap();
        crate::runtime_config::reload(&home);
        std::fs::remove_dir_all(&home).ok();
    }
}
