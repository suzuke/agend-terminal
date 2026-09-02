//! PR #3495 r4: the self-kick RECONCILIATION half of the Claude ChannelBridge
//! transport — the durable `ack_start` transition, the acknowledgement-window
//! watchdog, and the notice-intent lifecycle.
//!
//! Split out of `claude_channel.rs` in round 4 (the file crossed the 2500-LOC
//! anti-monolith ceiling); every item keeps its documentation verbatim and the
//! parent re-exports them, so `crate::transport::claude_channel::…` paths are
//! unchanged.
//!
//! The CONTRACT these items implement: no-loss (at-least-once) delivery of the
//! two operator notices — the `AckOverdue` alert and the late-ack resolution.
//! Dedup by idempotency key is best-effort quality on top of that. Hence the
//! rule every write here obeys: a compare-and-swap whose expected value comes
//! from a fresh re-read is not a compare-and-swap. Each marker-aware write
//! names the value its caller OBSERVED and acted on, and the receipt store
//! refuses any unconditional append that would regress a self-kick row or drop
//! its markers.

use super::*;

fn self_kick_ack_elapsed(recorded_at: &str, now: DateTime<Utc>) -> Option<Duration> {
    let recorded = DateTime::parse_from_rfc3339(recorded_at)
        .ok()?
        .with_timezone(&Utc);
    now.signed_duration_since(recorded).to_std().ok()
}

/// A hook-authority turn observed at or after a self-kick's acceptance —
/// the evidence that distinguishes "the resume never became a session turn"
/// from "the turn started but the ack protocol was not followed". Supplied by
/// the daemon-side caller, which owns the agent registry; the transport never
/// reads screen or hook state itself.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TurnObservation {
    /// Seconds between the bridge's acceptance and the observed turn.
    pub after_accept_secs: i64,
}

/// The durable `-> TurnStarted` half of the consumer's exact-id `ack_start`,
/// factored out of [`ChannelRuntime::acknowledge_self_kick`] (which owns only
/// the identity checks) so the daemon's per-tick tests — which hold no bridge
/// runtime — can drive the REAL transition rather than a hand-written receipt.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_self_kick_turn_started(
    home: &Path,
    instance: &str,
    session_id: &str,
    store: &ReceiptStore,
    envelope: &DeliveryEnvelope,
    mut current: DeliveryReceipt,
    now: DateTime<Utc>,
) -> anyhow::Result<DeliveryReceipt> {
    let delivery_id = envelope.delivery_id;
    if current.state == DeliveryState::TurnStarted {
        return Ok(current);
    }
    if current.state.is_terminal() && current.state != DeliveryState::Ambiguous {
        anyhow::bail!("self-kick delivery is no longer awaiting start acknowledgement")
    }
    if !matches!(
        current.state,
        DeliveryState::Queued
            | DeliveryState::ProtocolAccepted
            | DeliveryState::Ambiguous
            | DeliveryState::AckOverdue
    ) {
        anyhow::bail!("self-kick delivery is not awaiting start acknowledgement")
    }

    // The bridge can write the notification and receive the consumer's
    // ack before the daemon-side post-202 receipt append. Preserve the
    // truthful ProtocolAccepted state first when the durable row is still
    // Queued, then advance it to TurnStarted.
    if current.state == DeliveryState::Queued {
        let mut accepted = current.clone();
        accepted.state = DeliveryState::ProtocolAccepted;
        accepted.protocol_request_id = Some(delivery_id.to_string());
        accepted.backend_session_id = Some(session_id.to_string());
        accepted.backend_event = Some("webhook_accepted".to_string());
        // r3: marker-aware like every other write on this path. A `Queued`
        // row carries no intent today, so the predicate is the same one the
        // state-only CAS applied — but it can never SILENTLY overwrite a row
        // whose markers moved under it.
        store.record_if_marker(
            delivery_id,
            DeliveryState::Queued,
            current.late_ack_secs,
            current.notice_pending.as_ref(),
            accepted,
        )?;
        current = store
            .latest(delivery_id)?
            .ok_or_else(|| anyhow::anyhow!("self-kick receipt disappeared during accept"))?;
    }

    if current.state == DeliveryState::TurnStarted {
        return Ok(current);
    }
    if !matches!(
        current.state,
        DeliveryState::ProtocolAccepted | DeliveryState::Ambiguous | DeliveryState::AckOverdue
    ) {
        anyhow::bail!("self-kick delivery is not protocol-accepted")
    }
    // r3: ONE bounded retry against a freshly read snapshot. The CAS below is
    // marker-aware, so it also fails when only the markers moved (the per-tick
    // pass clearing the overdue intent concurrently); re-reading and retrying
    // is correct there, and there is deliberately no state-only fallback.
    for _ in 0..2 {
        // t-…-82348-105: a truthful ack that arrives AFTER the watchdog gave
        // up. The `AckOverdue` receipt still carries the ACCEPTED timestamp
        // (the watchdog transitions state in place and never restamps
        // `recorded_at`), so the overshoot past the window is computable here
        // and nowhere else downstream. Recorded as additive audit metadata —
        // the admissible `AckOverdue -> TurnStarted` transition is unchanged.
        let late_by_secs = (current.state == DeliveryState::AckOverdue)
            .then(|| self_kick_ack_elapsed(&current.recorded_at, now))
            .flatten()
            .map(|elapsed| {
                (elapsed.as_secs() as i64 - SELF_KICK_ACK_WINDOW.as_secs() as i64).max(0)
            });
        let mut started = DeliveryReceipt::for_state(envelope, DeliveryState::TurnStarted);
        started.protocol_request_id = current
            .protocol_request_id
            .clone()
            .or_else(|| Some(delivery_id.to_string()));
        started.backend_session_id = Some(session_id.to_string());
        started.backend_event = Some(
            if late_by_secs.is_some() {
                "claude_channel_turn_started_late"
            } else {
                "claude_channel_turn_started"
            }
            .to_string(),
        );
        started.late_ack_secs = late_by_secs;
        // r3 P0: the AckOverdue receipt may carry an UNSENT notice intent —
        // the watchdog persists it in the same CAS that declares the state,
        // and only the per-tick enqueue clears it. Carrying it forward is what
        // keeps the overdue alert derivable from the durable log after this
        // transition; the operator is owed BOTH notices, overdue first and
        // then the late-ack resolution the stamp above feeds.
        started.notice_pending = current.notice_pending.clone();
        started.detail = Some(match late_by_secs {
            Some(late) => format!(
                "consumer acknowledged exact self-kick delivery_id {late}s after the {:?} ack window closed",
                SELF_KICK_ACK_WINDOW
            ),
            None => "consumer acknowledged exact self-kick delivery_id".to_string(),
        });
        // Marker-aware: a concurrent pass that cleared (or moved) the intent
        // between the read and here must make this attempt lose, otherwise the
        // clone above would REINTRODUCE an intent whose notice is already out.
        if store.record_if_marker(
            delivery_id,
            current.state,
            current.late_ack_secs,
            current.notice_pending.as_ref(),
            started.clone(),
        )? {
            if let Some(late) = late_by_secs {
                crate::event_log::log(
                    home,
                    "claude_self_kick_ack_late",
                    instance,
                    &format!(
                        "agent {instance} delivery {delivery_id} late by {late}s past the {:?} ack window; the earlier AckOverdue is resolved",
                        SELF_KICK_ACK_WINDOW
                    ),
                );
            }
            return Ok(started);
        }
        let latest = store
            .latest(delivery_id)?
            .ok_or_else(|| anyhow::anyhow!("self-kick receipt disappeared during start ack"))?;
        if latest.state == DeliveryState::TurnStarted {
            return Ok(latest);
        }
        if !matches!(
            latest.state,
            DeliveryState::ProtocolAccepted | DeliveryState::Ambiguous | DeliveryState::AckOverdue
        ) {
            anyhow::bail!("self-kick start acknowledgement lost a receipt race")
        }
        current = latest;
    }
    anyhow::bail!("self-kick start acknowledgement lost a receipt race")
}

/// Drive the real `ack_start` receipt transition for `delivery_id` without a
/// bridge runtime: the daemon-side tests own no `ChannelRuntime`, and a
/// hand-written `TurnStarted` receipt would pin the fixture instead of the
/// production transition.
#[cfg(test)]
pub(crate) fn ack_start_for_test(
    home: &Path,
    instance: &str,
    delivery_id: Uuid,
) -> anyhow::Result<DeliveryReceipt> {
    let store = ReceiptStore::for_instance(home, instance)?;
    let (envelope, current) = store
        .delivery(delivery_id)?
        .ok_or_else(|| anyhow::anyhow!("no durable receipt for {delivery_id}"))?;
    let session_id = envelope
        .session
        .session_id
        .clone()
        .unwrap_or_else(|| "test-session".to_string());
    record_self_kick_turn_started(
        home,
        instance,
        &session_id,
        &store,
        &envelope,
        current,
        Utc::now(),
    )
}

/// One reconciliation outcome from a watchdog pass. The transport owns the
/// durable state transition and the escalation-channel/event-log record; the
/// per-tick handler owns the operator-facing inbox notice, because that is
/// the layer that holds the fleet, the recipient and the agent registry.
#[derive(Debug, Clone)]
pub(crate) struct SelfKickOutcome {
    pub delivery_id: Uuid,
    /// `AckOverdue`: when the bridge accepted the delivery (the accepted
    /// receipt's timestamp is carried forward unchanged into `AckOverdue`).
    /// `AckLate`: when the late `ack_start` was recorded.
    pub at: DateTime<Utc>,
    pub kind: SelfKickOutcomeKind,
    /// PR #3495 r4: the EXACT durable marker this pass observed and acted on.
    ///
    /// It is the token the caller must hand back to
    /// [`clear_self_kick_notice`]: the clear is only allowed to erase the very
    /// marker whose notice was just delivered. Re-reading the row and clearing
    /// "whatever is there now" is not a compare-and-swap — a stale scanner
    /// finishing its enqueue would erase a NEWER marker (the late-ack
    /// resolution) that nobody has enqueued yet, and that notice would be lost.
    /// `PendingNotice` carries `created_at`, so equality here is an identity
    /// test on the marker, not just on its kind.
    pub observed_notice: PendingNotice,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SelfKickOutcomeKind {
    /// The acknowledgement window closed with no `ack_start`. Emitted at most
    /// once per delivery — the `ProtocolAccepted -> AckOverdue` CAS is the
    /// latch.
    AckOverdue { turn: Option<TurnObservation> },
    /// An `ack_start` arrived after an `AckOverdue`, this many seconds past
    /// the window. Emitted at most once per delivery — the state-preserving
    /// CAS that clears `late_ack_secs` is the latch.
    AckLate { late_by_secs: i64 },
}

/// Reconcile accepted self-kicks from the durable receipt log. This scan is
/// intentionally separate from Claude hook observations: elapsed time only
/// creates a truthful nonterminal AckOverdue alert, never a TurnStarted proof
/// or retry. `observe` answers "was a hook-authority turn seen at/after this
/// acceptance time?" — evidence for the operator's notice, never a transition.
pub(crate) fn self_kick_watchdog_pass_at(
    home: &Path,
    instance: &str,
    now: DateTime<Utc>,
    observe: &dyn Fn(DateTime<Utc>) -> Option<TurnObservation>,
) -> anyhow::Result<Vec<SelfKickOutcome>> {
    let store = ReceiptStore::for_instance(home, instance)?;
    let mut outcomes = Vec::new();
    for (envelope, current) in store.deliveries_owing_notices()? {
        if !envelope.self_kick {
            continue;
        }
        let recorded_at = DateTime::parse_from_rfc3339(&current.recorded_at)
            .map(|at| at.with_timezone(&Utc))
            .unwrap_or(now);
        // PR #3495: an UNFINISHED notice from an earlier pass — the daemon died,
        // or the enqueue returned `Err`, between the durable intent and the
        // notice reaching the recipient's inbox. Replay it verbatim; the
        // caller's enqueue is idempotent by key, so a replay after a crash that
        // happened AFTER the enqueue appends nothing. The marker itself is the
        // retry bound: it is cleared only by `clear_self_kick_notice`, and no
        // counter is needed because the pass re-derives everything from it.
        if let Some(pending) = current.notice_pending.clone() {
            let kind = match pending.kind.as_str() {
                PendingNotice::LATE_ACK => SelfKickOutcomeKind::AckLate {
                    late_by_secs: pending.late_by_secs.unwrap_or_default(),
                },
                PendingNotice::ACK_OVERDUE => SelfKickOutcomeKind::AckOverdue {
                    turn: observe(recorded_at),
                },
                other => {
                    // A marker kind this binary does not know (a newer daemon
                    // wrote it). Leave it pending for that daemon rather than
                    // guessing a notice, and do not spin on it.
                    tracing::warn!(
                        agent = %instance,
                        delivery_id = %envelope.delivery_id,
                        kind = other,
                        "self-kick notice marker of unknown kind left pending"
                    );
                    continue;
                }
            };
            outcomes.push(SelfKickOutcome {
                delivery_id: envelope.delivery_id,
                at: recorded_at,
                kind,
                observed_notice: pending,
            });
            continue;
        }
        match current.state {
            DeliveryState::ProtocolAccepted => {
                if self_kick_ack_elapsed(&current.recorded_at, now)
                    .is_none_or(|elapsed| elapsed < SELF_KICK_ACK_WINDOW)
                {
                    continue;
                }
                let mut overdue = current.clone();
                overdue.state = DeliveryState::AckOverdue;
                overdue.backend_event = Some("claude_channel_self_kick_ack_timeout".to_string());
                overdue.detail = Some(format!(
                    "ProtocolAccepted but no exact consumer ack within {:?}; no automatic retry; inspect the agent and reconcile delivery {}",
                    SELF_KICK_ACK_WINDOW, envelope.delivery_id
                ));
                // PR #3495: the SAME compare-and-append that consumes the
                // overdue condition persists the durable INTENT to notify. The
                // caller clears it only after the inbox notice is durably
                // enqueued, so a crash — or a failed enqueue — in that gap
                // cannot swallow the alert the way the pre-#3495 order could.
                let intent = PendingNotice::new(PendingNotice::ACK_OVERDUE, None);
                overdue.notice_pending = Some(intent.clone());
                // The CAS is the exactly-once latch: only the pass that moves
                // ProtocolAccepted -> AckOverdue may alert. It is marker-aware,
                // so a scanner holding a stale pre-intent snapshot loses.
                if !store.record_if_marker(
                    envelope.delivery_id,
                    DeliveryState::ProtocolAccepted,
                    current.late_ack_secs,
                    None,
                    overdue,
                )? {
                    continue;
                }
                // Evidence, not a transition: whether the caller saw a
                // hook-authority turn after the bridge accepted the delivery.
                // It never influences the state machine — it only tells the
                // operator which of the two failures this is.
                let turn = observe(recorded_at);
                let alert = format!(
                    "[claude-self-kick] {} accepted delivery {} but received no exact consumer ack within {:?}; state=AckOverdue; no automatic retry — inspect the agent and reconcile manually",
                    instance, envelope.delivery_id, SELF_KICK_ACK_WINDOW
                );
                let channels = crate::channel::notify_all_escalation_channels(
                    instance,
                    crate::channel::NotifySeverity::Error,
                    &alert,
                    false,
                );
                tracing::warn!(agent = %instance, delivery_id = %envelope.delivery_id, "{alert}");
                crate::event_log::log(
                    home,
                    "claude_self_kick_ack_overdue",
                    instance,
                    &format!(
                        "{alert} notified_channels={channels} accepted_at={} turn_observed_since_kick={}{}",
                        current.recorded_at,
                        turn.is_some(),
                        turn.map(|t| format!(
                            " turn_observed_after_accept_s={}",
                            t.after_accept_secs
                        ))
                        .unwrap_or_default()
                    ),
                );
                outcomes.push(SelfKickOutcome {
                    delivery_id: envelope.delivery_id,
                    at: recorded_at,
                    kind: SelfKickOutcomeKind::AckOverdue { turn },
                    observed_notice: intent,
                });
            }
            // r3: `Completed` too — the consumer can finish its recovery
            // before this pass runs, and the reconciliation notice is still
            // owed. The CAS below preserves whatever state that is.
            DeliveryState::TurnStarted | DeliveryState::Completed => {
                // A late ack left a typed marker (`late_ack_secs`). Clear it
                // with a STATE-PRESERVING compare-and-append so the
                // reconciliation is reported exactly once per delivery, and
                // durably so across a daemon restart.
                let Some(late_by_secs) = current.late_ack_secs else {
                    continue;
                };
                let mut reconciled = current.clone();
                reconciled.late_ack_secs = None;
                let intent = PendingNotice::new(PendingNotice::LATE_ACK, Some(late_by_secs));
                reconciled.notice_pending = Some(intent.clone());
                reconciled.backend_event =
                    Some("claude_channel_turn_started_late_reconciled".to_string());
                // PR #3495: MARKER-AWARE CAS. The transition preserves
                // `TurnStarted`, so a state-only predicate is not a
                // linearization point: two scanners holding the same stale
                // `late_ack_secs` snapshot would both pass it and both emit.
                // Requiring the observed marker makes the loser fail. The
                // marker is MOVED, not consumed: the notice intent is durable
                // before the caller enqueues anything.
                if !store.record_if_marker(
                    envelope.delivery_id,
                    current.state,
                    Some(late_by_secs),
                    None,
                    reconciled,
                )? {
                    continue;
                }
                outcomes.push(SelfKickOutcome {
                    delivery_id: envelope.delivery_id,
                    at: recorded_at,
                    kind: SelfKickOutcomeKind::AckLate { late_by_secs },
                    observed_notice: intent,
                });
            }
            _ => {}
        }
    }
    Ok(outcomes)
}

pub(crate) fn self_kick_watchdog_pass(
    home: &Path,
    instance: &str,
    observe: &dyn Fn(DateTime<Utc>) -> Option<TurnObservation>,
) -> anyhow::Result<Vec<SelfKickOutcome>> {
    self_kick_watchdog_pass_at(home, instance, Utc::now(), observe)
}

/// PR #3495, step (c): clear the durable notice intent for `delivery_id` —
/// called by the per-tick handler ONLY after the operator notice is durably in
/// the recipient's inbox.
///
/// r4 RULE — a CAS whose expected value comes from a fresh re-read is NOT a
/// CAS. `observed` is the exact [`PendingNotice`] the caller's pass read and
/// acted on (`SelfKickOutcome::observed_notice`), and this function refuses to
/// touch the row unless that very marker is still the one on it.
///
/// The r3 shape re-read the latest receipt and cleared whatever marker it
/// found. That loses notices: scanner A observes `ack_overdue`, enqueues and
/// clears; a later pass moves `late_ack_secs` into a `late_ack` intent;
/// scanner B — stale, holding the same old `ack_overdue` observation —
/// finishes its own enqueue and clears the NEW marker, whose notice nobody has
/// sent. The late-ack resolution is then unreachable from the durable log.
///
/// The delivery state and `late_ack_secs` in the CAS expectation come from the
/// snapshot read here on purpose: the row may LEGITIMATELY have advanced
/// (`TurnStarted -> Completed`) while carrying the same unsent marker, and the
/// clear is still owed. Losing that race is harmless — this returns `false`
/// and the next pass replays the marker into an idempotent enqueue.
///
/// Returns `false` when the observed marker is no longer the row's (nothing was
/// written), `true` when the marker is now clear — including the idempotent
/// case where this caller had already cleared it.
pub(crate) fn clear_self_kick_notice(
    home: &Path,
    instance: &str,
    delivery_id: Uuid,
    observed: &PendingNotice,
) -> anyhow::Result<bool> {
    let store = ReceiptStore::for_instance(home, instance)?;
    let Some(current) = store.latest(delivery_id)? else {
        return Ok(false);
    };
    match current.notice_pending.as_ref() {
        // The marker this pass observed is still the row's: clear exactly it.
        Some(pending) if pending == observed => {}
        // Already cleared by this same pass's earlier attempt — idempotent.
        None => return Ok(true),
        // A DIFFERENT marker is parked here. It belongs to a notice nobody has
        // enqueued yet; erasing it would lose that notice.
        Some(other) => {
            tracing::warn!(
                agent = %instance,
                %delivery_id,
                observed = %observed.kind,
                found = %other.kind,
                "claude_self_kick: notice marker moved since it was observed; leaving it for the pass that owns it"
            );
            return Ok(false);
        }
    }
    let mut cleared = current.clone();
    cleared.notice_pending = None;
    store.record_if_marker(
        delivery_id,
        current.state,
        current.late_ack_secs,
        Some(observed),
        cleared,
    )
}
