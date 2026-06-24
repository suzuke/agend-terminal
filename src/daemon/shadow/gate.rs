//! #2413 — the SHARED `observed_status` consumption gate.
//!
//! Both consumers of the Shadow Observer's fused [`ObservedStatus`] decide IDENTICALLY
//! whether it should OVERRIDE the raw screen-scrape state: the pane badge ((A) —
//! `per_tick::shadow_observe`, the lock-free `published_observed` mirror render reads) and
//! the operated snapshot state ((B) — `per_tick::snapshot`, the `agent_state` string that
//! `dispatch_idle` / inbox / handoff / reply deciders read). Factored here so the two can
//! never drift (#1493 class) and so the gate — including the P2 composition invariant — is
//! unit-tested in ONE place.
//!
//! [`gated_override`] fires ONLY for a HIGH-CONFIDENCE real-time observer correction, i.e.
//! ALL of: (a) `authority ∈ {Hook, Stream}` — a live lifecycle / event-stream plane, not
//! the `Screen` baseline and not a low-confidence `Inferred` reconcile; (b) `confidence ∈
//! {Confirmed, Strong}` — freshness is already implied (the reducer only labels a fresh
//! observer plane this high); (c) the raw screen is NOT itself a GATE screen (`Approval` /
//! `RateLimited`) — a human approval prompt or a rate-limit banner is authoritative and must
//! NEVER be masked by an observed override (the operator/daemon must always see it); and
//! (d) the observed state genuinely DISAGREES with the raw screen baseline at the coarse
//! level (the §5 correction predicate via [`screen_as_observed`]), so an agreeing-but-vaguer
//! observed state never DOWNGRADES a more-specific raw state. `None` otherwise → the consumer
//! keeps the raw state. Weak / screen-only backends (agy has no Hook/Stream plane →
//! `authority` is always `Screen`/`ProcessHeuristic`/`Inferred`) can NEVER satisfy (a) → zero
//! regression for them, by construction.

use super::evidence::{Authority, Confidence};
use super::reducer::{ObservedState, ObservedStatus, ScreenSignal};
use crate::state::AgentState;

/// Map the 18-variant screen-scrape [`AgentState`] into the reducer's coarse
/// [`ScreenSignal`] buckets. Exhaustive (no wildcard) ON PURPOSE: a future `AgentState`
/// variant forces a compile error here so the map can never silently miss a state.
pub(crate) fn screen_signal(s: AgentState) -> ScreenSignal {
    match s {
        AgentState::Idle => ScreenSignal::Idle,
        // Actively rendering work (incl. boot/respawn churn, treated as working).
        AgentState::Active | AgentState::Starting | AgentState::Restarting => ScreenSignal::Working,
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
pub(crate) fn screen_as_observed(screen: ScreenSignal) -> Option<ObservedState> {
    match screen {
        ScreenSignal::Idle => Some(ObservedState::Idle),
        ScreenSignal::Working => Some(ObservedState::Active),
        ScreenSignal::Approval => Some(ObservedState::WaitingForUser),
        ScreenSignal::RateLimited => Some(ObservedState::RateLimited),
        ScreenSignal::Other => None,
    }
}

/// Map a corrected [`ObservedState`] to the [`AgentState`] a consumer shows (reusing the
/// existing `state_color` / `display_name` tables). `Idle` ⇒ `None`: a correction never
/// flips TO idle (the only idle-direction correction is the excluded `Inferred` reconcile).
fn observed_to_agent_state(state: ObservedState) -> Option<AgentState> {
    Some(match state {
        ObservedState::ToolUse
        | ObservedState::Thinking
        | ObservedState::Responding
        | ObservedState::Active => AgentState::Active,
        ObservedState::WaitingForUser => AgentState::AwaitingOperator,
        ObservedState::RateLimited => AgentState::RateLimit,
        ObservedState::Idle => return None,
    })
}

/// The shared gate (see module docs). `Some(state)` ⇒ both the badge AND the operated
/// snapshot should show `state` instead of `raw`; `None` ⇒ keep `raw`. Pure over its
/// inputs — never touches `State::current` (the cycle-proof invariant lives at the call
/// sites, but the gate itself reads nothing mutable).
pub(crate) fn gated_override(raw: AgentState, status: &ObservedStatus) -> Option<AgentState> {
    let screen = screen_signal(raw);
    // (c) A raw GATE screen is authoritative — never let an observed override mask a human
    // approval prompt or a rate-limit banner (also blocks a stale observed=Active from
    // hiding a raw that just transitioned to Approval under the reducer's snapshot).
    if matches!(screen, ScreenSignal::Approval | ScreenSignal::RateLimited) {
        return None;
    }
    // (a) + (b) high-confidence real-time observer plane only.
    let high_confidence = matches!(status.authority, Authority::Hook | Authority::Stream)
        && matches!(
            status.confidence,
            Confidence::Confirmed | Confidence::Strong
        );
    if !high_confidence {
        return None;
    }
    // (d) a genuine §5 coarse correction vs the raw screen baseline.
    let baseline = screen_as_observed(screen);
    if !baseline.is_some_and(|b| b != status.state.coarse()) {
        return None;
    }
    observed_to_agent_state(status.state)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::daemon::shadow::evidence::{Evidence, EvidenceKind};
    use crate::daemon::shadow::reducer::AgentRuntime;

    fn status(
        state: ObservedState,
        authority: Authority,
        confidence: Confidence,
    ) -> ObservedStatus {
        ObservedStatus {
            state,
            confidence,
            authority,
            evidence: vec![],
            since_ms: 0,
        }
    }

    /// The headline win: raw screen reads Idle mid-request, but a fresh Hook (or Stream)
    /// episode proves Active ⇒ override to a working state.
    #[test]
    fn flips_on_high_confidence_false_idle() {
        let s = status(ObservedState::Active, Authority::Hook, Confidence::Strong);
        assert_eq!(
            gated_override(AgentState::Idle, &s),
            Some(AgentState::Active)
        );
        // Stream plane (codex) parity.
        let s = status(
            ObservedState::ToolUse,
            Authority::Stream,
            Confidence::Strong,
        );
        assert_eq!(
            gated_override(AgentState::Idle, &s),
            Some(AgentState::Active)
        );
    }

    /// A hook `ApprovalRequired` (Confirmed) splits WaitingForUser out of an idle raw
    /// screen ⇒ override to AwaitingOperator (the approval-out-of-idle correction).
    #[test]
    fn flips_approval_out_of_idle() {
        let s = status(
            ObservedState::WaitingForUser,
            Authority::Hook,
            Confidence::Confirmed,
        );
        assert_eq!(
            gated_override(AgentState::Idle, &s),
            Some(AgentState::AwaitingOperator)
        );
    }

    /// Firewall (1): a screen-only backend (agy — authority always `Screen`) can NEVER
    /// satisfy the Hook/Stream gate → raw kept even at Strong confidence + disagreement.
    #[test]
    fn screen_only_backend_keeps_raw() {
        let s = status(
            ObservedState::WaitingForUser,
            Authority::Screen,
            Confidence::Strong,
        );
        assert_eq!(gated_override(AgentState::Idle, &s), None);
    }

    /// Firewall (2): a low-confidence (`Probable`) status does not override even with Hook.
    #[test]
    fn excludes_low_confidence() {
        let s = status(ObservedState::Active, Authority::Hook, Confidence::Probable);
        assert_eq!(gated_override(AgentState::Idle, &s), None);
    }

    /// The dropped-hook reconcile-to-Idle (`Inferred`/`Probable`) is excluded (lead open-q1).
    #[test]
    fn excludes_inferred_reconcile() {
        let s = status(
            ObservedState::Idle,
            Authority::Inferred,
            Confidence::Probable,
        );
        assert_eq!(gated_override(AgentState::Active, &s), None);
    }

    /// No downgrade: a Hook+Strong status that AGREES coarsely with the raw working screen
    /// (`Active`) does not override — the coarse baselines match, so nothing flips.
    #[test]
    fn no_downgrade_on_coarse_agreement() {
        let s = status(ObservedState::Active, Authority::Hook, Confidence::Strong);
        assert_eq!(gated_override(AgentState::Active, &s), None);
    }

    /// P2 composition invariant (t-…5060-11): a raw GATE screen (`Approval` / `RateLimited`)
    /// is NEVER masked by an override — for EVERY gate-class raw state, even a maximally
    /// confident Hook status pointing elsewhere returns `None` (keep the raw gate). This is
    /// the pin that the dispatch/operator can always see a pending approval or rate-limit.
    #[test]
    fn gate_screen_is_never_overridden() {
        let gate_raws = [
            AgentState::PermissionPrompt,
            AgentState::InteractivePrompt,
            AgentState::AwaitingOperator,
            AgentState::RateLimit,
            AgentState::ServerRateLimit,
            AgentState::UsageLimit,
        ];
        // The most "override-prone" status: highest confidence, active-family, disagreeing.
        let aggressive = status(ObservedState::Active, Authority::Hook, Confidence::Strong);
        for raw in gate_raws {
            assert_eq!(
                gated_override(raw, &aggressive),
                None,
                "a gate screen ({raw:?}) must never be masked by an observed override"
            );
        }
    }

    /// #1493 composition: drive the REAL reducer (not a hand-built status) and confirm the
    /// gate's end-to-end contract. A real `observe()` at an Approval screen yields
    /// WaitingForUser; promoting it back at an Approval raw is a no-op (gate screen kept),
    /// and a real mid-API false-idle (Idle screen + open hook episode + live socket) yields
    /// an Active status that the gate DOES promote at an Idle raw.
    #[test]
    fn composition_with_real_reducer_output() {
        // (1) Real approval at an approval screen → WaitingForUser; gate keeps the raw gate.
        let mut rt = AgentRuntime::default();
        rt.ingest(&Evidence::hook(EvidenceKind::TurnStarted, 1_000));
        rt.ingest(&Evidence::hook(EvidenceKind::ApprovalRequired, 1_100));
        let live = crate::daemon::shadow::reducer::Liveness {
            api_in_flight: true,
            productive_silent_ms: 0,
            child_alive: true,
        };
        let s = rt.observe(ScreenSignal::Approval, &live, 1_200);
        assert_eq!(s.state, ObservedState::WaitingForUser);
        assert_eq!(
            gated_override(AgentState::PermissionPrompt, &s),
            None,
            "an approval screen must stay raw, not be masked"
        );

        // (2) Real mid-API false-idle (Idle screen, fresh hook episode, live socket) →
        // Active; the gate promotes it at an Idle raw.
        let mut rt = AgentRuntime::default();
        rt.ingest(&Evidence::hook(EvidenceKind::TurnStarted, 1_000));
        let s = rt.observe(ScreenSignal::Idle, &live, 1_500);
        assert_ne!(s.state, ObservedState::Idle);
        assert!(
            gated_override(AgentState::Idle, &s).is_some(),
            "a real mid-API false-idle must promote at an idle raw"
        );
    }

    #[test]
    fn observed_to_agent_state_maps_active_family_and_excludes_idle() {
        assert_eq!(
            observed_to_agent_state(ObservedState::ToolUse),
            Some(AgentState::Active)
        );
        assert_eq!(
            observed_to_agent_state(ObservedState::Thinking),
            Some(AgentState::Active)
        );
        assert_eq!(
            observed_to_agent_state(ObservedState::Responding),
            Some(AgentState::Active)
        );
        assert_eq!(
            observed_to_agent_state(ObservedState::Active),
            Some(AgentState::Active)
        );
        assert_eq!(
            observed_to_agent_state(ObservedState::WaitingForUser),
            Some(AgentState::AwaitingOperator)
        );
        assert_eq!(
            observed_to_agent_state(ObservedState::RateLimited),
            Some(AgentState::RateLimit)
        );
        assert_eq!(observed_to_agent_state(ObservedState::Idle), None);
    }

    #[test]
    fn screen_signal_maps_gate_classes() {
        assert_eq!(screen_signal(AgentState::Idle), ScreenSignal::Idle);
        assert_eq!(screen_signal(AgentState::Active), ScreenSignal::Working);
        assert_eq!(
            screen_signal(AgentState::PermissionPrompt),
            ScreenSignal::Approval
        );
        assert_eq!(
            screen_signal(AgentState::UsageLimit),
            ScreenSignal::RateLimited
        );
        assert_eq!(screen_signal(AgentState::Crashed), ScreenSignal::Other);
    }
}
