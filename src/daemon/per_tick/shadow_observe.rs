//! #2413 Shadow Observer — Phase B per-tick driver.
//!
//! The reducer ([`crate::daemon::shadow::reducer`]) is a pure state machine; this
//! handler is the per-tick glue that feeds it. Each tick (default-ON; skipped only under
//! the `AGEND_SHADOW_OBSERVER=0` kill-switch) it, per managed agent:
//!   1. snapshots the screen baseline (`agent_state` → [`ScreenSignal`]) + the cheap
//!      out-of-path liveness ([`Liveness`]: `api_in_flight` / productive-silence /
//!      child-alive) under one `core.lock()`;
//!   2. folds the agent's buffered hook Evidence into its persistent reducer runtime
//!      and derives an [`crate::daemon::shadow::reducer::ObservedStatus`]
//!      ([`crate::daemon::shadow::observe`]);
//!   3. hangs the result on `AgentCore.observed_status` — purely ADDITIVE, beside
//!      `agent_state`, which it NEVER rewrites. `list_instances` serializes both under
//!      one lock so a consumer can diff them (that diff IS the §5 quantification).
//!
//! Flag-OFF default ⇒ a single cheap `enabled()` check then early-return (zero work,
//! zero behaviour change). The reduce reads ONLY the hook buffer + screen + lsof signal
//! — zero in-path (SHADOW-OBSERVER-ARCH-2413.md invariants).

use super::{PerTickHandler, TickContext};
use crate::agent::AgentCore;
use crate::daemon::shadow;
use crate::daemon::shadow::reducer::{Liveness, ObservedState, ScreenSignal};
use crate::state::AgentState;
use crate::sync_audit::CoreMutex;
use std::sync::Arc;

/// Per-tick reduce driver. Stateless: the per-agent accumulators live in the shadow
/// module's runtime registry (keyed by name, pruned on despawn), so nothing here needs
/// interior mutability.
pub(crate) struct ShadowObserveHandler;

impl ShadowObserveHandler {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl PerTickHandler for ShadowObserveHandler {
    fn name(&self) -> &'static str {
        "shadow_observe"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        // Flag-OFF default: one env check then nothing. The hook socket server is also
        // gated off, so the buffers are empty anyway.
        if !shadow::enabled() {
            return;
        }
        // Snapshot (name, core, child_alive) under a BRIEF registry lock, then release
        // it before touching any per-agent core lock (never hold the registry lock
        // across another lock — mirrors `api_activity_probe::probe_once`). child_alive
        // is read here (its own `child.lock()`) so the reduce loop needs only core.lock.
        let agents: Vec<(String, Arc<CoreMutex<AgentCore>>, bool)> = {
            let reg = crate::agent::lock_registry(ctx.registry);
            reg.values()
                .map(|h| {
                    let child_alive = h.child.lock().process_id().is_some();
                    (h.name.to_string(), Arc::clone(&h.core), child_alive)
                })
                .collect()
        };
        if agents.is_empty() {
            return;
        }
        let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
        for (name, core, child_alive) in &agents {
            // Read the screen + liveness inputs under one core.lock(), drop it, run the
            // reduce (which takes the shadow buffer/runtime locks — DISJOINT from
            // core.lock, and no thread ever acquires core.lock while holding those, so
            // no ordering hazard), then write the result under a fresh brief lock.
            let (raw_state, screen, live) = {
                let c = core.lock();
                let raw_state = c.state.get_state();
                let live = Liveness {
                    api_in_flight: c.api_activity.in_flight,
                    productive_silent_ms: c.state.productive_silence().as_millis() as u64,
                    child_alive: *child_alive,
                };
                (raw_state, screen_signal(raw_state), live)
            };
            let status = shadow::observe(name, screen, &live, now_ms);
            log_correction(name, raw_state, screen, &status, &live);
            core.lock().observed_status = Some(status);
        }
    }
}

/// Map the 18-variant screen-scrape [`AgentState`] into the reducer's coarse
/// [`ScreenSignal`] buckets. Exhaustive (no wildcard) ON PURPOSE: a future `AgentState`
/// variant forces a compile error here so the map can never silently miss a state.
fn screen_signal(s: AgentState) -> ScreenSignal {
    match s {
        AgentState::Idle => ScreenSignal::Idle,
        // Actively rendering work (incl. boot/respawn churn, treated as working).
        AgentState::ToolUse
        | AgentState::Thinking
        | AgentState::Starting
        | AgentState::Restarting => ScreenSignal::Working,
        // A human gate.
        AgentState::PermissionPrompt
        | AgentState::InteractivePrompt
        | AgentState::AwaitingOperator => ScreenSignal::Approval,
        AgentState::RateLimit | AgentState::ServerRateLimit | AgentState::UsageLimit => {
            ScreenSignal::RateLimited
        }
        // Non-decisive for the liveness reconcile (it only fires on `Idle`). A genuinely
        // crashed agent is caught by `child_alive=false`, not by the screen chrome.
        AgentState::Hang
        | AgentState::GitConflict
        | AgentState::ContextFull
        | AgentState::AuthError
        | AgentState::ApiError
        | AgentState::ModelUnsupported
        | AgentState::Crashed => ScreenSignal::Other,
    }
}

/// The coarse [`ObservedState`] the raw screen-scrape ALONE would report — the baseline
/// the reducer is measured against (§5 quantification). `None` for a non-decisive screen
/// (`Other`), which the reducer never claims to "correct" (no meaningful baseline).
fn screen_as_observed(screen: ScreenSignal) -> Option<ObservedState> {
    match screen {
        ScreenSignal::Idle => Some(ObservedState::Idle),
        ScreenSignal::Working => Some(ObservedState::Active),
        ScreenSignal::Approval => Some(ObservedState::WaitingForUser),
        ScreenSignal::RateLimited => Some(ObservedState::RateLimited),
        ScreenSignal::Other => None,
    }
}

/// §5 quantification telemetry. When the fused `ObservedStatus` disagrees with what the
/// raw screen-scrape ALONE would report, that's a CORRECTION — the reducer's whole value
/// (e.g. screen renders `Idle` mid-request but a live hook episode + API socket prove
/// `Active`; or screen looks `Idle`/ambiguous at a permission prompt but a hook
/// `ApprovalRequired` proves `WaitingForUser`). Logged at INFO (the headline metric: grep
/// `#shadow-observer` + "correction" to count false-idles caught / approval splits); the
/// agreeing per-tick trace is DEBUG. Runs only under the flag ⇒ zero prod noise.
fn log_correction(
    name: &str,
    raw_state: AgentState,
    screen: ScreenSignal,
    status: &crate::daemon::shadow::reducer::ObservedStatus,
    live: &Liveness,
) {
    let Some(screen_state) = screen_as_observed(screen) else {
        return; // non-decisive screen → no baseline to correct
    };
    if screen_state != status.state.coarse() {
        tracing::info!(
            tag = "#shadow-observer",
            agent = %name,
            raw_screen = %raw_state.display_name(),
            observed = ?status.state,
            confidence = ?status.confidence,
            authority = ?status.authority,
            api_in_flight = live.api_in_flight,
            productive_silent_ms = live.productive_silent_ms,
            "shadow correction: ObservedStatus overrides raw screen-scrape"
        );
    } else {
        tracing::debug!(
            tag = "#shadow-observer",
            agent = %name,
            raw_screen = %raw_state.display_name(),
            observed = ?status.state,
            "shadow tick: observed agrees with raw screen"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::agent::{AgentRegistry, ExternalRegistry};
    use crate::daemon::shadow::evidence::{Evidence, EvidenceKind};
    use crate::daemon::shadow::reducer::ObservedState;
    use parking_lot::Mutex as PLMutex;
    use serial_test::serial;
    use std::collections::HashMap;

    /// Set `AGEND_SHADOW_OBSERVER`, run a closure, then restore the prior value. Paired
    /// with `#[serial(shadow_observer)]` so the process-global env flip can't leak into a
    /// parallel test reading `shadow::enabled()`. Since the plane is now **default-ON**, the
    /// OFF case must set the explicit `=0` kill-switch (an unset/removed var is now ON).
    fn with_flag<T>(on: bool, f: impl FnOnce() -> T) -> T {
        let prev = std::env::var("AGEND_SHADOW_OBSERVER").ok();
        if on {
            std::env::set_var("AGEND_SHADOW_OBSERVER", "1");
        } else {
            std::env::set_var("AGEND_SHADOW_OBSERVER", "0");
        }
        let out = f();
        match prev {
            Some(v) => std::env::set_var("AGEND_SHADOW_OBSERVER", v),
            None => std::env::remove_var("AGEND_SHADOW_OBSERVER"),
        }
        out
    }

    /// Build a one-agent registry around a live mock agent (real `cat` child → its
    /// `process_id()` is `Some`, so `child_alive=true`). Returns the kept-alive PTY
    /// reader the caller must not drop, and the agent's name + core for assertions.
    fn one_agent_registry(
        name: &str,
    ) -> (
        AgentRegistry,
        Arc<CoreMutex<AgentCore>>,
        String,
        Box<dyn std::io::Read + Send>,
    ) {
        let (handle, reader) = super::super::mock_live_agent_no_context(name);
        let core = Arc::clone(&handle.core);
        let agent_name = handle.name.to_string();
        let registry: AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
        registry.lock().insert(handle.id, handle);
        (registry, core, agent_name, reader)
    }

    fn ctx_for<'a>(
        home: &'a std::path::Path,
        registry: &'a AgentRegistry,
        externals: &'a ExternalRegistry,
        configs: &'a Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>>,
    ) -> TickContext<'a> {
        TickContext {
            home,
            registry,
            externals,
            configs,
        }
    }

    /// Prod wiring, flag-ON: a buffered `TurnStarted` (episode open) + a live child ⇒ the
    /// handler writes `observed_status` = an Active-family state beside `agent_state`.
    /// This is the regression pin that the per-tick reduce actually runs in prod (not
    /// just the pure reducer unit tests).
    #[test]
    #[serial(shadow_observer)]
    fn flag_on_reduce_writes_observed_status() {
        let home = std::env::temp_dir();
        let externals: ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs = Arc::new(PLMutex::new(HashMap::new()));
        let (registry, core, name, _reader) = one_agent_registry("shadow-drv-on");

        let token = shadow::new_session_token().unwrap();
        shadow::register(&token, &name);
        shadow::push(
            &name,
            Evidence::hook(
                EvidenceKind::TurnStarted,
                chrono::Utc::now().timestamp_millis().max(0) as u64,
            ),
        );

        with_flag(true, || {
            let ctx = ctx_for(&home, &registry, &externals, &configs);
            ShadowObserveHandler::new().run(&ctx);
        });

        let status = core.lock().observed_status.clone();
        let status = status.expect("flag-ON ⇒ reduce ran ⇒ observed_status set");
        assert_ne!(
            status.state,
            ObservedState::Idle,
            "an open episode + a live child ⇒ Active family, not Idle"
        );
        shadow::forget_agent(&name);
    }

    /// Flag-OFF default: the handler early-returns, so `observed_status` stays `None`
    /// (zero behaviour change for the default fleet). Pins that the flag actually gates.
    #[test]
    #[serial(shadow_observer)]
    fn flag_off_leaves_observed_status_none() {
        let home = std::env::temp_dir();
        let externals: ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs = Arc::new(PLMutex::new(HashMap::new()));
        let (registry, core, name, _reader) = one_agent_registry("shadow-drv-off");

        let token = shadow::new_session_token().unwrap();
        shadow::register(&token, &name);
        shadow::push(&name, Evidence::hook(EvidenceKind::TurnStarted, 1_000));

        with_flag(false, || {
            let ctx = ctx_for(&home, &registry, &externals, &configs);
            ShadowObserveHandler::new().run(&ctx);
        });

        assert!(
            core.lock().observed_status.is_none(),
            "flag-OFF ⇒ no reduce ⇒ observed_status stays None"
        );
        shadow::forget_agent(&name);
    }

    #[test]
    fn screen_signal_maps_every_bucket() {
        assert_eq!(screen_signal(AgentState::Idle), ScreenSignal::Idle);
        assert_eq!(screen_signal(AgentState::ToolUse), ScreenSignal::Working);
        assert_eq!(screen_signal(AgentState::Thinking), ScreenSignal::Working);
        assert_eq!(screen_signal(AgentState::Starting), ScreenSignal::Working);
        assert_eq!(
            screen_signal(AgentState::PermissionPrompt),
            ScreenSignal::Approval
        );
        assert_eq!(
            screen_signal(AgentState::InteractivePrompt),
            ScreenSignal::Approval
        );
        assert_eq!(
            screen_signal(AgentState::AwaitingOperator),
            ScreenSignal::Approval
        );
        assert_eq!(
            screen_signal(AgentState::RateLimit),
            ScreenSignal::RateLimited
        );
        assert_eq!(
            screen_signal(AgentState::ServerRateLimit),
            ScreenSignal::RateLimited
        );
        assert_eq!(
            screen_signal(AgentState::UsageLimit),
            ScreenSignal::RateLimited
        );
        assert_eq!(screen_signal(AgentState::Crashed), ScreenSignal::Other);
        assert_eq!(screen_signal(AgentState::GitConflict), ScreenSignal::Other);
        assert_eq!(screen_signal(AgentState::Hang), ScreenSignal::Other);
    }

    #[test]
    fn screen_as_observed_baseline_buckets() {
        assert_eq!(
            screen_as_observed(ScreenSignal::Idle),
            Some(ObservedState::Idle)
        );
        assert_eq!(
            screen_as_observed(ScreenSignal::Working),
            Some(ObservedState::Active)
        );
        assert_eq!(
            screen_as_observed(ScreenSignal::Approval),
            Some(ObservedState::WaitingForUser)
        );
        assert_eq!(
            screen_as_observed(ScreenSignal::RateLimited),
            Some(ObservedState::RateLimited)
        );
        // A non-decisive screen has no baseline the reducer can "correct".
        assert_eq!(screen_as_observed(ScreenSignal::Other), None);
    }

    /// The §5 correction predicate, deterministically: the two headline wins the live
    /// quantification counts. (a) mid-API false-idle — screen Idle but a fresh hook
    /// episode + a live socket ⇒ Active ⇒ Idle≠Active ⇒ counts as a correction.
    /// (b) approval-vs-idle — hook ApprovalRequired but screen reads Idle ⇒ WaitingForUser
    /// ⇒ correction. (c) control — steady tool-use agrees with the screen ⇒ NOT a
    /// correction (no false-active regression).
    #[test]
    fn correction_predicate_flags_false_idle_and_approval_not_steady() {
        let is_correction = |screen: ScreenSignal, observed: ObservedState| {
            screen_as_observed(screen).is_some_and(|b| b != observed.coarse())
        };
        // (a) false-idle caught.
        assert!(is_correction(ScreenSignal::Idle, ObservedState::Active));
        assert!(is_correction(ScreenSignal::Idle, ObservedState::ToolUse));
        // (b) approval split out of idle.
        assert!(is_correction(
            ScreenSignal::Idle,
            ObservedState::WaitingForUser
        ));
        // (c) steady states agree → no correction (no regression).
        assert!(!is_correction(
            ScreenSignal::Working,
            ObservedState::ToolUse
        ));
        assert!(!is_correction(ScreenSignal::Idle, ObservedState::Idle));
        assert!(!is_correction(
            ScreenSignal::Approval,
            ObservedState::WaitingForUser
        ));
    }
}
