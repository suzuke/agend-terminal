use crate::channel::telegram;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

pub(super) fn handle_reply(home: &Path, args: &Value, instance_name: &str) -> Value {
    // #1602: the reply content param is `message` (was `text`) — now consistent
    // with `send`/`schedule`. The MCP dispatch validator rejects a missing
    // `message` with a clear named error, so a mis-named param no longer
    // silently becomes an empty reply.
    let text = args["message"].as_str().unwrap_or("").to_string();
    tracing::info!(from = %instance_name, %text, "reply");

    // Reply-to correlation: carry the sending turn's task context (when the
    // agent passes it) into the sent_ledger so a later operator reply-to can be
    // resolved back to this message's task. Absent on interactive replies → None.
    let reply_task_id = args["task_id"]
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty());
    let reply_correlation_id = args["correlation_id"]
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty());

    // Sprint 59 Wave 1 PR-4 ((B) decision default with timeout):
    // dual-purpose hook on every reply call.
    //
    // (1) When `default_action` + `timeout_secs` are set, record a
    //     pending operator decision sidecar — the daemon scheduler
    //     auto-fires the default after the timeout window.
    // (2) Otherwise, treat this reply as a potential operator
    //     override that resolves any prior pending decision from
    //     the same sender (clears the timeout fire).
    //
    // Backwards-compat preserved: the existing reply path runs
    // regardless of which branch fires, so legacy callers that
    // never pass either field continue blocking on the operator's
    // explicit reply as before.
    let default_action = args["default_action"]
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty());
    let timeout_secs = args["timeout_secs"].as_i64().filter(|&t| t > 0);
    let mut decision_id: Option<String> = None;
    let mut resolved_id: Option<String> = None;
    if let (Some(action), Some(secs)) = (default_action.as_deref(), timeout_secs) {
        decision_id = crate::daemon::decision_timeout::record_pending_decision(
            home,
            instance_name,
            action,
            secs,
        );
    } else {
        resolved_id =
            crate::daemon::decision_timeout::mark_resolved_for_sender(home, instance_name);
    }

    let fleet_path = crate::fleet::fleet_yaml_path(home);
    if !fleet_path.exists() {
        // #1665 Gap D (codex catch): the reply cannot be sent (no fleet.yaml), so
        // this exit is a send-failure too — record it, matching every other
        // failure exit. Without this the turn stayed Pending and the ledger later
        // mis-classified it as a plain silent drop instead of SendFailed.
        crate::reply_ledger::record_reply_outcome(instance_name, false);
        return json!({"error": "No fleet.yaml — cannot send reply"});
    }

    // #2622 PR-3: an explicit `message_id` routes by THAT message's own
    // channel (from its inbox row) instead of the process-global prefer-chain
    // below — the targeted-reply path (Fork C: a late reply to an
    // old/reclaimed message must land on ITS channel even when the sender's
    // CURRENT `reply_to_channel` tag has moved on or gone stale). No
    // `message_id` → byte-identical to the pre-#2622 prefer-chain.
    let message_id = args["message_id"].as_str().filter(|s| !s.is_empty());
    let ch: Arc<dyn crate::channel::Channel> = if let Some(msg_id) = message_id {
        let Some(row) = crate::inbox::storage::find_message(home, msg_id) else {
            crate::reply_ledger::record_reply_outcome(instance_name, false);
            return json!({
                "error": format!("message '{msg_id}' not found in any inbox"),
                "code": "message_not_found"
            });
        };
        let Some(kind) = row.channel else {
            crate::reply_ledger::record_reply_outcome(instance_name, false);
            return json!({
                "error": format!(
                    "message '{msg_id}' has no channel — cannot route a targeted reply"
                ),
                "code": "message_has_no_channel"
            });
        };
        let channel_name = match kind {
            crate::channel::ChannelKind::Telegram => "telegram",
            crate::channel::ChannelKind::Discord => "discord",
        };
        match crate::channel::lookup_channel_by_name(channel_name) {
            Some(ch) => ch,
            None => {
                tracing::warn!(
                    from = %instance_name, channel = %channel_name,
                    "targeted reply's channel not registered — divergence"
                );
                crate::reply_ledger::record_reply_outcome(instance_name, false);
                return json!({
                    "error": format!("channel '{channel_name}' not registered or offline"),
                    "code": "reply_channel_unavailable"
                });
            }
        }
    } else {
        // Sprint 55 P0-A — prefer-chain: sender's HeartbeatPair.reply_to_channel
        // (Sprint 52 router-layer attribution) wins when present + registered;
        // else fall back to whichever channel the sending instance is bound to
        // (multi-channel-safe, t-20260703164240502572-50899-11 — `active_channel()`
        // alone returns `None` once 2+ channels are registered); last resort is
        // the `active_channel()` singleton. Returns structured error codes so
        // agents can branch on machine-readable signals.
        let snapshot = crate::daemon::heartbeat_pair::snapshot_for(instance_name);
        match snapshot.reply_to_channel.as_deref() {
            Some(name) => match crate::channel::lookup_channel_by_name(name) {
                Some(ch) => ch,
                None => {
                    tracing::warn!(
                        from = %instance_name, channel = %name,
                        "reply_to_channel tagged but not registered — divergence"
                    );
                    // #1665 Gap D: the agent tried to reply but the channel is
                    // unreachable — record the send-failure (never blocks the reply).
                    crate::reply_ledger::record_reply_outcome(instance_name, false);
                    return json!({
                        "error": format!("reply_to_channel '{name}' not registered or offline"),
                        "code": "reply_channel_unavailable"
                    });
                }
            },
            None => match crate::channel::channel_for_instance(instance_name)
                .or_else(crate::channel::active_channel)
            {
                Some(ch) => ch,
                None => {
                    // #1665 Gap D: no channel to reply on — record the send-failure.
                    crate::reply_ledger::record_reply_outcome(instance_name, false);
                    return json!({"error": "no active channel", "code": "no_active_channel"});
                }
            },
        }
    };
    // #969 RC2 fix: set mirror_skip BEFORE the send. Pre-fix this set
    // ran in the Ok arm AFTER ch.send_from_agent returned; on the
    // telegram path send_from_agent spawns a fire-and-forget task and
    // returns Ok(0) immediately, but the PTY-mirror dispatcher
    // (src/daemon/router.rs:try_dispatch_mirror) is on a different
    // thread and could sample heartbeat_pair BEFORE this set fired —
    // dispatching its own mirror of the same text. Moving the set
    // earlier closes the dominant race window (channel-side dedup
    // in src/channel/dedup.rs catches any residual collisions).
    //
    // Err path policy (dev-2 Pushback 4): leave mirror_skip set even
    // when send fails. The flag's `_until_next_turn` semantics
    // naturally expire on the next turn boundary, so we don't
    // accidentally suppress a legitimate next-turn mirror. Flipping
    // the flag back on Err would risk double-delivery if the actual
    // send eventually lands while the mirror also fires.
    crate::daemon::heartbeat_pair::update_with(instance_name, |p| {
        p.mirror_skip_until_next_turn = true;
    });
    match ch.send_from_agent(
        instance_name,
        crate::channel::AgentOutboundOp::Reply {
            text,
            task_id: reply_task_id,
            correlation_id: reply_correlation_id,
        },
    ) {
        Ok(msg) => {
            // A plain reply (no explicit `message_id`) answers the CURRENT user turn.
            // Capture that turn's persistent inbox-row ids NOW — BEFORE
            // `record_reply_outcome` below, which `take()`s `pending_user_turn` — so we
            // can settle them. The daemon drain armed the real delivery ids into
            // `group_msg_ids` (#2042: replying to any id settles the whole logical turn).
            let plain_reply_group_ids: Vec<String> = if message_id.is_none() {
                crate::daemon::heartbeat_pair::snapshot_for(instance_name)
                    .pending_user_turn
                    .map(|t| t.group_msg_ids)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            // #1665: reply delivered — closes the user-turn (no warn at sweep).
            crate::reply_ledger::record_reply_outcome(instance_name, true);
            // #2622 PR-3 Fork C: a targeted reply also settles the persistent
            // row (unconditionally — a `read` row is a no-op, an `unread`/
            // `delivering` row is marked read) so an old/reclaimed message
            // stops redelivering once it's actually been answered.
            if let Some(msg_id) = message_id {
                crate::inbox::storage::settle_read_by_id(home, instance_name, msg_id);
            } else {
                // Plain-reply path: settle the whole turn's group so the row can't stay
                // `delivering` and get reverted-to-unread + RE-DELIVERED by
                // `reclaim_stale_delivering` after the 600s TTL — the telegram
                // double-reply. Same action as the `message_id` path; id source differs.
                for id in &plain_reply_group_ids {
                    crate::inbox::storage::settle_read_by_id(home, instance_name, id);
                }
            }
            // Sprint 59 Wave 1 PR-4: surface the pending-decision /
            // resolved-decision IDs so caller observability is
            // complete (operator can reference IDs, agent can verify
            // its override-resolution landed).
            let mut response = json!({ "message_id": msg.id });
            if let Some(id) = decision_id {
                response["pending_decision_id"] = json!(id);
            }
            if let Some(id) = resolved_id {
                response["resolved_decision_id"] = json!(id);
            }
            response
        }
        Err(crate::channel::ChannelError::NotSupported(op)) => {
            tracing::warn!(
                from = %instance_name, channel = %ch.kind(),
                "reply capability unsupported on tagged channel — divergence"
            );
            // #1665 Gap D: send failed (capability) — record send-failure.
            crate::reply_ledger::record_reply_outcome(instance_name, false);
            json!({
                "error": format!("channel '{}' does not support {}", ch.kind(), op),
                "code": "channel_capability_unsupported"
            })
        }
        Err(e) => {
            // #1665 Gap D: send failed — record send-failure.
            crate::reply_ledger::record_reply_outcome(instance_name, false);
            json!({"error": format!("{e}")})
        }
    }
}

/// #2622 PR-2: agent self-discharge of a channel-reply obligation — the
/// deliberate exit for a message that will not be (or no longer needs to be)
/// answered on its channel (the live case: `m-…-125`, an operator message 13
/// days stale). Durably records the discharge so `reply_ledger::arm` never
/// re-opens the obligation (even across redelivery + daemon restart), stops the
/// currently-armed nudge ladder, settles the persistent inbox row, and LOUDLY
/// notifies the operator (channel fan-out; inbox fallback) so this can never be
/// a silent backdoor.
///
/// ## Why this is always the AGENT path (architecture note for review)
///
/// An MCP tool handler is reached ONLY via `method::MCP_TOOL` — agent transport
/// (`operator_gate.rs:145`). The operator's surface is direct API methods (they
/// early-`Ok` at the gate, never reaching MCP dispatch) and telegram inbound
/// (routed to an agent inbox/PTY, not an MCP call). So this handler's caller is
/// ALWAYS an agent; there is no operator branch to make here, and testing
/// `instance_name == "operator"` would be a forgeable backdoor (an agent can
/// name itself `operator` — the exact #1575 identity-trust bypass the gate
/// exists to close). Operator-authorized (unrestricted) discharge is therefore
/// a separate, deferred surface (a telegram operator keyword / direct method),
/// per the vetted design. Consequently self-discharge is CONSTRAINED: a
/// non-empty `reason` is mandatory and the operator is always notified —
/// converting "silent no-reply" into "explicit, reasoned, operator-notified
/// no-reply."
pub(super) fn handle_discharge(home: &Path, args: &Value, instance_name: &str) -> Value {
    let msg_id = match args["message_id"].as_str().filter(|s| !s.is_empty()) {
        Some(m) => m,
        None => {
            return json!({
                "error": "discharge requires a non-empty 'message_id'",
                "code": "missing_message_id"
            })
        }
    };
    // Anti-backdoor gate: agent self-discharge is LOUD — a reason is mandatory
    // so the operator learns WHY their message was closed reply-less.
    let reason = match args["reason"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(r) => r,
        None => {
            return json!({
                "error": "discharge requires a non-empty 'reason' \
                          (self-discharge is audited + operator-notified)",
                "code": "missing_reason"
            })
        }
    };

    // Resolve the obligation identity from the message row — the discharge must
    // name a real message. `group_key` is computed the SAME way `arm` does, so
    // the durable record keys the arm-guard exactly (incl. an operator resend of
    // the same text: same group_key, new id).
    let Some(row) = crate::inbox::storage::find_message(home, msg_id) else {
        return json!({
            "error": format!("message '{msg_id}' not found in any inbox"),
            "code": "message_not_found"
        });
    };
    let group_key = crate::reply_ledger::group_key(Some(&row.from), Some(&row.text));

    // (1) Durable ledger — the structural exit (arm never re-opens this group).
    // Keyed by `instance_name` too (#2622 reviewer4 r0): self-discharge is
    // always the caller closing its OWN obligation, so the caller IS the
    // recipient-agent dimension that scopes the key — a different agent's
    // same-sender/same-text obligation must never be suppressed by this call.
    if let Err(e) = crate::daemon::channel_reply_discharge::record_discharge(
        home,
        instance_name,
        group_key.as_deref(),
        msg_id,
        instance_name,
        Some(reason),
    ) {
        return json!({
            "error": format!("failed to record discharge: {e}"),
            "code": "ledger_write_failed"
        });
    }
    // (2) Stop the CURRENTLY-armed ladder now (strict no-op if the live turn is
    // a DIFFERENT obligation — never clobber an unrelated pending reply).
    let cleared_turn =
        crate::reply_ledger::discharge_turn_if_matches(instance_name, msg_id, group_key.as_deref());
    // (3) Settle the persistent row so it stops redelivering (handles the
    // typical `unread` state, unlike `ack` which is `delivering`-only).
    let settled_row = crate::inbox::storage::settle_read_by_id(home, instance_name, msg_id);

    // (3b) #35896-11 ②: if the discharged message is a `ci-ready-for-action`
    // handoff, this explicit discharge ALSO resolves the caller's
    // ci_handoff_track — so the ONE agent-facing verb silences BOTH the
    // poll_reminder (row settled above) AND the renudge/escalation watchdog.
    // Before this wire the only ways to stop a ci-ready renudge were `ci
    // unwatch` (blunt — tombstones the whole watch) or waiting out the 24h
    // backstop; a `send triaged=` was a silent dead-write (#2537 ledger is
    // ci-fail-only). Keyed on the message's own `correlation_id` (a ci-ready
    // message always carries `repo@branch`, the track's key — poller.rs) and
    // TARGET-scoped to the caller via `resolve_for_target_correlation` (the same
    // precise dismiss `ci unwatch` uses), so a co-subscriber's handoff for the
    // same branch is left intact and a discharge clears only the obligation the
    // discharged message names. An explicit gesture (not an inbox read) →
    // #1888's stuck-reviewer escalation is preserved for handoffs never acted on.
    let handoff_resolved = if row.kind.as_deref() == Some("ci-ready-for-action") {
        match (
            row.correlation_id.as_deref(),
            row.ci_handoff_episode.as_deref(),
            row.ci_handoff_class,
        ) {
            (Some(corr), Some(episode), Some(crate::inbox::CiHandoffClass::Protected)) => {
                crate::daemon::ci_handoff_track::resolve_protected_episode(
                    home,
                    instance_name,
                    corr,
                    episode,
                    "discharge",
                )
            }
            // Feature handoffs retain the legacy explicit-discharge path; the
            // protected resolver must never infer identity from classless rows.
            (Some(corr), _, Some(crate::inbox::CiHandoffClass::Feature)) => {
                crate::daemon::ci_handoff_track::resolve_legacy_for_target_correlation_reason(
                    home,
                    instance_name,
                    corr,
                    "discharge",
                )
            }
            // Classless/episode-less rows predate protected handoff identity.
            // Preserve their explicit-discharge compatibility path while
            // keeping protected ACK/reconciliation fail-closed on identity.
            (Some(corr), _, _) => {
                crate::daemon::ci_handoff_track::resolve_legacy_for_target_correlation_reason(
                    home,
                    instance_name,
                    corr,
                    "discharge",
                )
            }
            _ => 0,
        }
    } else {
        0
    };

    // (4) LOUD notice — the operator owns the right-to-know. Primary: fan out to
    // every operator channel (channel send → no inbox row → structurally zero
    // obligation loop). Fallback (no channel registered → returns 0): an inbox
    // row to the discharging agent's team lead, kind=`channel-reply-discharged`
    // (fire-and-forget from birth — it can't itself become an obligation).
    let notice = format!(
        "[channel-reply-discharged] {instance_name} closed message {msg_id} \
         without a channel reply — reason: {reason}"
    );
    let dispatched = crate::channel::notify_all_escalation_channels(
        instance_name,
        crate::channel::NotifySeverity::Info,
        &notice,
        false,
    );
    let notified_via = if dispatched > 0 {
        "channel"
    } else {
        if let Some(lead) =
            crate::fleet::team_orchestrator_for(home, instance_name).filter(|l| l != instance_name)
        {
            let _ = crate::inbox::notify_system(
                home,
                &lead,
                "system:channel-reply-discharge",
                "channel-reply-discharged",
                notice.clone(),
                None,
                None,
            );
        }
        "inbox_fallback"
    };
    // (5) event_log — the third independent audit copy (ledger + notice + log).
    crate::event_log::log(
        home,
        "channel_reply_discharged",
        instance_name,
        &format!(
            "msg_id={msg_id} cleared_turn={cleared_turn} settled_row={settled_row} \
             handoff_resolved={handoff_resolved} notified_via={notified_via} reason={reason}"
        ),
    );

    json!({
        "discharged": true,
        "message_id": msg_id,
        "cleared_turn": cleared_turn,
        "settled_row": settled_row,
        "handoff_resolved": handoff_resolved,
        "notified_via": notified_via
    })
}

pub(super) fn handle_download_attachment(home: &Path, args: &Value, instance_name: &str) -> Value {
    let file_id = match args["file_id"].as_str() {
        Some(f) => f,
        None => return json!({"error": "missing 'file_id'"}),
    };
    match telegram::try_download_attachment(home, instance_name, file_id) {
        Ok(path) => json!({"path": path}),
        Err(e) => json!({"error": format!("{e}")}),
    }
}

// Sprint 55 P0-A — handle_reply prefer-chain tests in sibling file.
#[cfg(test)]
#[path = "channel_p0a_tests.rs"]
mod p0a_tests;
