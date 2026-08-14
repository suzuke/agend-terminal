//! Member-state-change notify + reaction routing (split from supervisor.rs).

use super::{
    escalate_self_orch_autherror, mark_usage_limit_recovered, parse_unlock_at,
    record_usage_limit_notified, self_orchestrator_escalates, usage_limit_notify_suppressed,
    NotifyTrack, NOTIFY_COOLDOWN,
};
use std::collections::HashMap;
use std::time::Instant;

/// Decide and dispatch member-state-change notify. Returns true if notify sent.
/// Production-path-coupled per §3.5.10 — tests call this same function.
pub(crate) fn maybe_notify_member_state_change(
    home: &std::path::Path,
    name: &str,
    prev_state: crate::state::AgentState,
    new_state: crate::state::AgentState,
    pane_tail: &str,
    tracks: &mut HashMap<String, NotifyTrack>,
) -> bool {
    if prev_state == crate::state::AgentState::UsageLimit
        && new_state != crate::state::AgentState::UsageLimit
    {
        // Preserve the notice ledger but close the live episode boundary. The
        // next UsageLimit edge must be readable even when unlock_at is absent.
        mark_usage_limit_recovered(home, name);
    }
    if prev_state == new_state || !new_state.is_notify_error_class() {
        return false;
    }
    let now = Instant::now();
    let should = tracks
        .get(name)
        .is_none_or(|t| now.duration_since(t.last_at) >= NOTIFY_COOLDOWN);
    if !should {
        return false;
    }
    // #1744-M7: distinguish "teams config unreadable" (Err → can't determine the
    // orchestrator) from "loaded, no team for this member" (None). For the no-peer
    // AuthError P0 the unreadable case fails CLOSED — escalate to the operator
    // rather than silently dropping (we can't relay to an orchestrator we can't
    // identify, and AuthError is operator-only). Non-escalation states stay
    // dropped (we genuinely can't route them).
    let fleet = match crate::teams::try_load_fleet(home) {
        Ok(f) => f,
        Err(_) => {
            if self_orchestrator_escalates(new_state) {
                escalate_self_orch_autherror(name, now, tracks);
            }
            return false;
        }
    };
    let Some(team) = crate::teams::find_team_for_in(&fleet, name) else {
        return false;
    };
    let Some(ref orch) = team.orchestrator else {
        tracing::warn!(agent = %name, team = %team.name, "member-state-change: team has no orchestrator — notify dropped");
        return false;
    };
    if orch == name {
        // #1595 Step 2: the orchestrator IS the affected agent — no peer can relay
        // its inbox P0. For a state only the operator can resolve (AuthError: only
        // the operator can re-authenticate), escalate straight to the operator
        // via gated_notify(Error) — the same Sleep-penetrating path #1594 allows
        // through. Cooldown-stamped so a persistent AuthError escalates at most
        // once per NOTIFY_COOLDOWN, not every tick. Other states keep the D3
        // self-notify skip (transient / the agent reads its own inbox).
        // NOTE: Crashed/Hang are NOT live AgentStates via this hook (never assigned
        // to `state.current`); real crash/hang self-orchestrator escalation is a
        // follow-up (#1701) using the process-exit / HealthState::Hung paths (the
        // latter strong-gated for the known 348-FP).
        if self_orchestrator_escalates(new_state) {
            escalate_self_orch_autherror(name, now, tracks);
        }
        return false; // D3: still skip the inbox self-notify (no peer reads it)
    }
    let unlock_at = if new_state == crate::state::AgentState::UsageLimit {
        parse_unlock_at(pane_tail)
    } else {
        None
    };
    // #1861: usage_limit notify re-fired on EVERY daemon restart — the in-mem
    // `tracks` cooldown is Instant-based and wiped on restart, and the backend
    // boots `Starting` → re-detects UsageLimit → re-transitions. Persist the
    // "already notified" decision keyed (member, unlock_at) so a restart with the
    // SAME unlock window stays silent; re-notify only when unlock_at ADVANCES (new
    // limit) or has PASSED. Scoped to UsageLimit ONLY — other error-class notifies
    // keep the in-session cooldown unchanged.
    if new_state == crate::state::AgentState::UsageLimit
        && usage_limit_notify_suppressed(home, name, unlock_at.as_deref(), chrono::Utc::now())
    {
        // Stamp the in-mem track so same-session ticks short-circuit at the
        // cooldown gate above without re-reading the persisted record each tick.
        let track = tracks.entry(name.to_string()).or_insert(NotifyTrack {
            last_at: now,
            consecutive: 0,
        });
        track.last_at = now;
        return false;
    }
    let track = tracks.entry(name.to_string()).or_insert(NotifyTrack {
        last_at: now,
        consecutive: 0,
    });
    track.consecutive += 1;
    track.last_at = now;
    // #event-bus pattern #9, Step 2 (legacy-zero): freeze the only now()-derived
    // value (detected_at) here so the subscriber renders the inbox payload
    // byte-identically, then emit MemberStateChanged (the subscriber delivers via
    // `deliver_member_state_change`). The bus is the sole delivery path.
    let detected_at = chrono::Utc::now().to_rfc3339();
    let from_display = prev_state.display_name();
    let to_display = new_state.display_name();
    crate::daemon::event_bus::global().emit(
        home,
        crate::daemon::event_bus::EventKind::MemberStateChanged {
            agent: name.to_string(),
            team: team.name.clone(),
            from_state: from_display.to_string(),
            to_state: to_display.to_string(),
            orch: orch.clone(),
            new_state,
            pane_tail: pane_tail.to_string(),
            unlock_at: unlock_at.clone(),
            consecutive_count: track.consecutive,
            detected_at,
        },
    );
    // #1861: record the notify so a daemon restart with the SAME unlock window
    // stays silent (the in-mem track above is wiped on restart).
    if new_state == crate::state::AgentState::UsageLimit {
        record_usage_limit_notified(home, name, unlock_at.as_deref(), chrono::Utc::now());
    }
    true
}

/// Shared deliver for the member-state-change notify: (A) enqueue the structured
/// JSON event to the orchestrator's inbox, (B) PTY-notify the orchestrator with
/// the human-readable line + action hint. Called by BOTH the legacy direct path
/// AND the event-bus subscriber, so A and B are byte-identical by construction
/// (the gate only chooses which path invokes this fn — the fn itself is fixed).
/// The notify_agent half (B) is a PTY-inject, not an inbox enqueue, so it is not
/// drain-assertable in tests; it is covered by this shared-deliver-fn invariant
/// (parity tests assert the inbox half A). All now()-derived input (`detected_at`)
/// is passed in frozen so the bus path reproduces A byte-for-byte.
#[allow(clippy::too_many_arguments)]
pub(crate) fn deliver_member_state_change(
    home: &std::path::Path,
    orch: &str,
    name: &str,
    team_name: &str,
    from_display: &str,
    to_display: &str,
    new_state: crate::state::AgentState,
    pane_tail: &str,
    unlock_at: Option<&str>,
    consecutive: u32,
    detected_at: &str,
) {
    // (A) structured JSON inbox enqueue.
    let payload = serde_json::json!({
        "type": "member_state_change",
        "member": name,
        "team": team_name,
        "from_state": from_display,
        "to_state": to_display,
        "detected_at": detected_at,
        "context": {
            "last_pane_excerpt": pane_tail,
            "unlock_at": unlock_at,
            "consecutive_count": consecutive,
        }
    });
    let msg = crate::inbox::InboxMessage::new_system(
        "system:supervisor",
        "member_state_change",
        payload.to_string(),
    );
    persist_or_log!(
        crate::inbox::enqueue(home, orch, msg),
        "member_state_change",
        orch
    );
    // (B) human-readable PTY notify with action hint.
    let action_hint = match new_state {
        crate::state::AgentState::Hang => {
            "\nAction: check agent pane snapshot, consider restart if no progress >5min"
        }
        crate::state::AgentState::UsageLimit => {
            "\nAction: wait for limit reset or switch backend. Do NOT retry."
        }
        crate::state::AgentState::Crashed => {
            "\nAction: check logs, restart agent, reassign task if needed"
        }
        crate::state::AgentState::PermissionPrompt => {
            "\nAction: approve or deny the pending permission prompt"
        }
        crate::state::AgentState::RateLimit => {
            "\nAction: wait for rate limit cooldown, auto-retry expected"
        }
        crate::state::AgentState::AuthError => {
            "\nAction: check credentials, may need operator re-auth"
        }
        _ => "",
    };
    crate::inbox::notify_agent(
        home,
        orch,
        &crate::inbox::NotifySource::System("supervisor"),
        &format!("[member_state_change] {name}: {from_display} → {to_display}{action_hint}"),
    );
    tracing::info!(agent = %name, from = %from_display, to = %to_display, orchestrator = %orch, "member-state-change notify sent");
}

/// #event-bus pattern #9 subscriber: re-deliver a `MemberStateChanged` event via
/// the shared `deliver_member_state_change`.
pub(crate) fn handle_event(event: &crate::daemon::event_bus::Event) -> bool {
    if let crate::daemon::event_bus::EventKind::MemberStateChanged {
        agent,
        team,
        from_state,
        to_state,
        orch,
        new_state,
        pane_tail,
        unlock_at,
        consecutive_count,
        detected_at,
    } = &event.kind
    {
        deliver_member_state_change(
            &event.home,
            orch,
            agent,
            team,
            from_state,
            to_state,
            *new_state,
            pane_tail,
            unlock_at.as_deref(),
            *consecutive_count,
            detected_at,
        );
        true
    } else {
        false
    }
}

/// Register the member-state-change subscriber once at daemon startup (`run_core`).
/// Home-agnostic — the home travels on each event.
pub(crate) fn register_subscriber() {
    crate::daemon::event_bus::global().subscribe(handle_event);
}

/// #1530: a reaction-worthy net state change for one agent in one tick.
/// `to` is guaranteed reaction-worthy (`is_notify_error_class`, which
/// includes `UsageLimit`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReactionDecision {
    pub(crate) from: crate::state::AgentState,
    pub(crate) to: crate::state::AgentState,
}

/// #1530: enriched [`ReactionDecision`] carrying the data captured under the
/// core lock so the actual reaction emit can run lock-free after `drop(core)`.
pub(crate) struct ReactionIntent {
    pub(crate) from: crate::state::AgentState,
    pub(crate) to: crate::state::AgentState,
    pub(crate) backend: Option<crate::backend::Backend>,
    /// 3-line PTY tail for the operator UsageLimit notice.
    pub(crate) snippet: String,
    /// 10-line PTY tail for the member-state-change notice.
    pub(crate) pane_tail: String,
}

/// #1530: which reactions a net `to` state drives. Pure + testable — proves the
/// emit routing (esp. that a `UsageLimit` final state ALSO produces a
/// `MemberNotify`, which the pre-#1530 `propagate ... continue` silently ate).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReactionKind {
    NotifyOperator,
    Propagate,
    MemberNotify,
}

/// #1530: derive the NET state change across the drained transition list and
/// return a reaction decision iff the net `to` differs from the net `from` AND
/// is reaction-worthy.
///
/// Net-state (not per-transition) semantics: an intra-tick flap that enters
/// then leaves an error state (e.g. `Idle→UsageLimit→Idle`) has no net change
/// → no reaction, so transient blips don't spam the operator/orchestrator.
/// Transition LOGGING records every transition separately (#1527); only the
/// reaction converges to the final state.
///
/// This replaces the pre-#1530 `if prev_state != new_state` gate, which was
/// blind to feed-driven transitions (they complete async in the read-loop
/// thread, so `prev == new` by the next supervisor tick) — see #1530.
pub(crate) fn reactions_from_transitions(
    transitions: &[crate::state::TransitionRecord],
) -> Vec<ReactionDecision> {
    let (Some(first), Some(last)) = (transitions.first(), transitions.last()) else {
        return Vec::new();
    };
    let (from, to) = (first.from, last.to);
    if from == to || !to.is_notify_error_class() {
        return Vec::new();
    }
    vec![ReactionDecision { from, to }]
}

/// #1530: pure emit-routing. `UsageLimit` → operator notice (+ propagate when
/// enabled) AND member-notify (UsageLimit ∈ `is_notify_error_class`); any other
/// error-class state → member-notify only. Keeping this separate from the emit
/// lets a unit test assert no reaction is dropped (the regression the removed
/// `continue` caused).
pub(crate) fn reaction_kinds(
    to: crate::state::AgentState,
    propagation_enabled: bool,
) -> Vec<ReactionKind> {
    let mut kinds = Vec::new();
    if to == crate::state::AgentState::UsageLimit {
        kinds.push(ReactionKind::NotifyOperator);
        if propagation_enabled {
            kinds.push(ReactionKind::Propagate);
        }
    }
    if to.is_notify_error_class() {
        kinds.push(ReactionKind::MemberNotify);
    }
    kinds
}
