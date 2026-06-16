//! #1665 reply-ledger (Phase 1) + #2042 Phase 2 — delivery-closure audit with
//! an actionable escalation ladder.
//!
//! Tracks, per agent, the in-flight user channel message turn and, when a turn
//! ends with the user's message un-replied, routes the obligation to the party
//! that can act on it (#2008 (a) actionable-or-silent):
//!
//! 1. **Stage 1 — nudge the agent** that owes the reply (inject, with the
//!    message id and a "use the reply tool" instruction). The Phase-1
//!    `tracing::warn!` + event-log line stay (the audit record).
//! 2. **Stage 2 — escalate to its lead** when the nudge goes unanswered for
//!    another window (inbox message to the team orchestrator).
//! 3. **Stage 3 — operator, last resort only**, phrased for humans (never a
//!    ledger WARN dump), then the obligation is latched closed.
//!
//! Each stage fires AT MOST ONCE per obligation (escalate-don't-repeat).
//!
//! **Group settlement (#2042)**: duplicate deliveries of the same logical
//! message (same sender + normalized-content hash — e.g. an operator double
//! send, or a channel-side redelivery replay) join ONE obligation group;
//! replying to any member settles the whole group, and a redelivery arriving
//! AFTER settlement opens no new obligation (a recently-settled-groups memory
//! suppresses it). Without this, N delivery ids per logical message meant a
//! reply settled one id and the rest went stale → false no-reply escalations.
//!
//! Content-hash trade-off: a user sending the SAME text for a genuinely NEW
//! event within the TTL is theoretically settled-suppressed (review r1 scanned
//! 166 historical operator messages: zero such cases; the real duplicates it
//! merged were all true duplicates). The blast radius is capped at one missing
//! stage-1 nudge — the message itself is still delivered to the agent.
//! Scope: the settled-groups memory is in-memory (`HeartbeatPair`), so the
//! suppression covers channel-reconnect replay within ONE daemon lifetime; a
//! replay arriving after a daemon restart re-opens the obligation, with the
//! same capped consequence (one redundant nudge, not an operator page).
//!
//! It is an AUDIT, not a second delivery path: it never re-delivers the
//! agent's missing reply, and — the IRON RULE — it never blocks/rejects the
//! agent. Every ledger op is infallible (a lock update) or swallows its error.
//!
//! The state hangs on the existing turn state (`HeartbeatPair.pending_user_turn`)
//! — NOT a 5th lifecycle file (#922 single-signal). The turn boundary is the
//! existing `reply_to_channel` Some→None clear sites; a supervisor-tick sweep is
//! the fallback for turns that never clear. See `/tmp/1665-spike.md`.

use crate::channel::ChannelKind;

/// Outcome of the agent's explicit `reply` attempt this turn (Gap D tracking).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplyOutcome {
    /// No `reply` call recorded yet.
    #[default]
    Pending,
    /// `reply` succeeded (channel accepted the send).
    Delivered,
    /// `reply` was attempted but the send failed (Gap D — the sharpest hole:
    /// the agent THINKS it replied / got an error it may ignore, and the mirror
    /// backup is suppressed, so the user gets nothing).
    SendFailed,
}

/// One in-flight user-message turn for an agent. Carries its own
/// (channel, chat, message) identity so an escalation routes back to the RIGHT
/// channel and multiple channels never cross-talk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUserTurn {
    /// Primary (first-armed) delivery id of the obligation group.
    pub inbound_msg_id: Option<String>,
    /// #2042: every delivery id belonging to this logical obligation — the
    /// primary plus duplicates/redeliveries that group-joined while pending.
    /// Replying to ANY of them settles the whole group (the group IS one turn).
    pub group_msg_ids: Vec<String>,
    /// #2042 group identity: `sender|normalized-content-hash`. `None` when the
    /// inbound had no usable text (grouping disabled for that turn).
    pub group_key: Option<String>,
    pub channel: ChannelKind,
    /// Telegram topic / Discord thread id — the per-chat scope of the key.
    /// NOTE (#2042): deliberately NOT used as the group key — on telegram this
    /// is the agent's whole per-instance topic, so "same thread = same group"
    /// would collapse unrelated questions into one obligation.
    pub chat_id: Option<String>,
    pub inbound_kind: Option<String>,
    pub armed_at_ms: i64,
    pub reply_outcome: ReplyOutcome,
    /// #2042 ladder progress: 0 = not yet acted, 1 = agent nudged,
    /// 2 = lead escalated. Stage 3 (operator) clears the turn, so it never
    /// appears here. Monotonic — each stage fires at most once.
    pub stage: u8,
    /// When the last ladder stage fired (epoch ms); gates the follow-up window.
    pub last_stage_at_ms: i64,
}

/// Closure verdict for a turn being evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closure {
    /// User got a reply or a mirror — never escalate.
    Closed,
    /// Not reply-eligible (non-user kind) — never escalate.
    NeverWarn,
    /// Un-closed but closeable — run the ladder.
    Warn(WarnReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarnReason {
    /// Gap D — reply attempted, send failed.
    ReplySendFailed,
    /// No reply, no mirror — the user's message got nothing.
    SilentNoReply,
}

/// #2042: what the supervisor should do after a [`sweep`] tick. The audit WARN
/// (tracing + event-log) happens inside `sweep`; these are the ADDITIONAL
/// agent-/lead-/operator-facing actions the supervisor performs. Each variant
/// fires at most once per obligation (the turn's `stage` latch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepAction {
    /// Nothing to do this tick.
    None,
    /// Stage 1: inject a nudge into the agent that owes the reply, naming the
    /// message id and instructing it to use the reply tool. `gap_d` selects
    /// accurate wording: the agent DID call reply but the send failed, so
    /// "you didn't reply" would be wrong — it must RETRY instead.
    NudgeAgent {
        channel: &'static str,
        msg_id: Option<String>,
        gap_d: bool,
    },
    /// Stage 2: the nudge went unanswered for [`STAGE_FOLLOWUP_MS`] — notify
    /// the agent's lead (team orchestrator) via its inbox.
    EscalateLead {
        lead: String,
        channel: &'static str,
        msg_id: Option<String>,
        armed_at_ms: i64,
    },
    /// Stage 3, last resort: tell the operator in human phrasing (see
    /// [`operator_text`]) on the originating channel. The turn is cleared and
    /// the group recorded settled — latched, never repeated.
    NotifyOperator {
        channel: &'static str,
        msg_id: Option<String>,
        armed_at_ms: i64,
    },
}

/// Grace window after arming before a turn is eligible for the ladder — gives
/// the agent time to respond before we cry wolf. Combined with the
/// `agent_is_busy` gate in [`sweep`], this is the primary false-positive
/// defense (a slow-but-working agent is `busy`, and even a settled agent gets
/// the grace).
const GRACE_MS: i64 = 30_000;

/// #2042: window between ladder stages — how long a fired stage gets to
/// produce the missing reply before the next rung fires. Generous on purpose:
/// the nudged agent may queue the reply behind in-flight work, and the lead
/// needs human-scale time to act.
const STAGE_FOLLOWUP_MS: i64 = 180_000;

/// #2042: how long a settled group suppresses re-arming for the same logical
/// message (sender + content). Covers operator double-sends and channel-side
/// redelivery replays around restarts.
const SETTLED_GROUP_TTL_MS: i64 = 600_000;

/// #2042: bound on the per-agent settled-groups memory (oldest evicted).
const SETTLED_GROUPS_MAX: usize = 8;

/// Inbound `kind`s that never expect a user-facing reply (defensive — user
/// channel messages usually carry no kind, but an agent/system message that
/// somehow armed must not escalate). Mirrors the spike's never-warn boundary.
fn is_never_warn_kind(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some("update" | "report" | "ci-watch" | "ci_watch" | "system")
    )
}

fn channel_kind_str(c: ChannelKind) -> &'static str {
    match c {
        ChannelKind::Telegram => "telegram",
        ChannelKind::Discord => "discord",
    }
}

/// #2042: group identity for duplicate-delivery settlement —
/// `sender|hash(normalized text)`. Normalization: trim, collapse internal
/// whitespace, lowercase — so a resend with stray spacing still groups.
/// `None` (no usable text) disables grouping for the message.
fn group_key(from: Option<&str>, text: Option<&str>) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let text = text?.trim();
    if text.is_empty() {
        return None;
    }
    let normalized = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    normalized.hash(&mut h);
    Some(format!("{}|{:016x}", from.unwrap_or(""), h.finish()))
}

/// Record `key` as a settled group on the (locked) pair — re-arms of the same
/// logical message are suppressed for [`SETTLED_GROUP_TTL_MS`]. Bounded.
fn settle_group_locked(p: &mut crate::daemon::heartbeat_pair::HeartbeatPair, key: Option<&str>) {
    let now = crate::daemon::heartbeat_pair::now_ms() as i64;
    p.settled_reply_groups
        .retain(|(_, at)| now.saturating_sub(*at) < SETTLED_GROUP_TTL_MS);
    if let Some(k) = key {
        if !p.settled_reply_groups.iter().any(|(g, _)| g == k) {
            p.settled_reply_groups.push((k.to_string(), now));
        }
        while p.settled_reply_groups.len() > SETTLED_GROUPS_MAX {
            p.settled_reply_groups.remove(0);
        }
    }
}

/// Arm (or group-join, or supersede) the in-flight turn for `name`. Called
/// from the inbox dequeue arm site for a user channel message. Infallible.
///
/// #2042 three-way decision:
/// - same group as a RECENTLY SETTLED one → no new obligation (redelivery /
///   duplicate of an already-answered message);
/// - same group as the CURRENT pending turn → join it (id appended; ladder
///   state preserved — it is the same obligation, not a fresh one);
/// - otherwise → fresh obligation, superseding any different pending turn
///   (the old turn is dropped without escalation — the user moved on).
pub fn arm(
    name: &str,
    channel: ChannelKind,
    inbound_msg_id: Option<String>,
    chat_id: Option<String>,
    inbound_kind: Option<String>,
    from: Option<&str>,
    text: Option<&str>,
) {
    let key = group_key(from, text);
    let now = crate::daemon::heartbeat_pair::now_ms() as i64;
    let name_owned = name.to_string();
    crate::daemon::heartbeat_pair::update_with(name, move |p| {
        p.settled_reply_groups
            .retain(|(_, at)| now.saturating_sub(*at) < SETTLED_GROUP_TTL_MS);
        if let Some(k) = &key {
            // Redelivery / duplicate of an already-settled logical message —
            // it must NOT open a new obligation (#2042 element 3).
            if p.settled_reply_groups.iter().any(|(g, _)| g == k) {
                tracing::debug!(
                    target: "reply_ledger",
                    agent = %name_owned,
                    msg_id = ?inbound_msg_id,
                    "duplicate of a settled group — not re-armed"
                );
                return;
            }
            // Duplicate of the CURRENT pending obligation — join the group.
            if let Some(t) = p.pending_user_turn.as_mut() {
                if t.group_key.as_deref() == Some(k.as_str()) {
                    if let Some(id) = inbound_msg_id.clone() {
                        if !t.group_msg_ids.contains(&id) {
                            t.group_msg_ids.push(id);
                        }
                    }
                    return;
                }
            }
        }
        // Fresh obligation (supersede: a different pending turn is dropped
        // without escalation — the user moved on).
        p.pending_user_turn = Some(PendingUserTurn {
            group_msg_ids: inbound_msg_id.clone().into_iter().collect(),
            inbound_msg_id: inbound_msg_id.clone(),
            group_key: key.clone(),
            channel,
            chat_id: chat_id.clone(),
            inbound_kind: inbound_kind.clone(),
            armed_at_ms: now,
            reply_outcome: ReplyOutcome::Pending,
            stage: 0,
            last_stage_at_ms: 0,
        });
    });
}

/// Record the agent's `reply` outcome (Gap D). Called from `handle_reply` on
/// every exit. `Ok` → the turn is settled: the WHOLE group closes (#2042 —
/// replying to any member settles every delivery id) and the group key is
/// remembered so a late redelivery doesn't re-open it. Any send/lookup failure
/// → SendFailed (turn stays for the ladder). No-op if no turn is armed.
/// Infallible — never affects the reply's return value.
pub fn record_reply_outcome(name: &str, delivered: bool) {
    crate::daemon::heartbeat_pair::update_with(name, |p| {
        if delivered {
            if let Some(t) = p.pending_user_turn.take() {
                settle_group_locked(p, t.group_key.as_deref());
            }
        } else if let Some(t) = p.pending_user_turn.as_mut() {
            t.reply_outcome = ReplyOutcome::SendFailed;
        }
    });
}

/// Clear the in-flight turn WITHOUT escalation — the never-warn closure paths:
/// mirror dispatched (user got the mirror), operator TUI takeover (can't tell
/// abandoned vs handled), supersede. #2042: the group is recorded settled, so
/// a duplicate/redelivery of the same logical message arriving later does not
/// re-open the obligation. Infallible.
pub fn clear_turn(name: &str) {
    crate::daemon::heartbeat_pair::update_with(name, |p| {
        if let Some(t) = p.pending_user_turn.take() {
            settle_group_locked(p, t.group_key.as_deref());
        }
    });
}

/// Pure closure classifier — no side effects, fully unit-testable.
fn classify(mirror_dispatched_for_turn: bool, turn: &PendingUserTurn) -> Closure {
    if mirror_dispatched_for_turn || turn.reply_outcome == ReplyOutcome::Delivered {
        return Closure::Closed;
    }
    if is_never_warn_kind(turn.inbound_kind.as_deref()) {
        return Closure::NeverWarn;
    }
    match turn.reply_outcome {
        ReplyOutcome::SendFailed => Closure::Warn(WarnReason::ReplySendFailed),
        _ => Closure::Warn(WarnReason::SilentNoReply),
    }
}

/// Supervisor-tick evaluation: run the #2042 escalation ladder for a turn that
/// never hit a clear site. Stage 1 (agent nudge) fires only past the grace
/// window AND when the agent has settled (not mid-generation) — so a
/// slow-but-working agent never false-escalates; the same busy gate holds
/// between stages (don't climb the ladder while the agent is visibly working).
///
/// `lead_of` resolves an agent's lead (team orchestrator) — injected for
/// testability; production passes a `crate::teams` lookup. A missing lead (or
/// the agent being its own lead) skips stage 2 straight to the operator rung.
///
/// Infallible (swallows everything). The Phase-1 WARN (tracing + event-log)
/// fires once at stage 1 — the audit record stays in the logs (#2042 keeps
/// WARN in log); the operator channel is only touched at stage 3.
///
/// Stage-1 delivery is fire-and-forget: the stage advances when the action is
/// RETURNED, not when the supervisor's inject succeeds — a failed/deferred
/// inject is NOT retried at this rung. That is deliberate: the ladder itself
/// is the retry (the lead is escalated `STAGE_FOLLOWUP_MS` later, and its
/// message carries the msg id so the obligation stays traceable).
pub fn sweep(
    home: &std::path::Path,
    name: &str,
    lead_of: &dyn Fn(&str) -> Option<String>,
) -> SweepAction {
    let pair = crate::daemon::heartbeat_pair::snapshot_for(name);
    let Some(turn) = pair.pending_user_turn.clone() else {
        return SweepAction::None;
    };
    let reason = match classify(pair.mirror_dispatched_for_turn, &turn) {
        Closure::Closed => {
            clear_turn(name); // settles the group — closure already happened
            return SweepAction::None;
        }
        Closure::NeverWarn => {
            // Non-user kind: drop the turn without settling the group (no
            // user-facing obligation existed).
            crate::daemon::heartbeat_pair::update_with(name, |p| {
                p.pending_user_turn = None;
            });
            return SweepAction::None;
        }
        Closure::Warn(reason) => reason,
    };
    if crate::snapshot::agent_is_busy(home, name) {
        return SweepAction::None; // still generating — not a silent drop, don't cry wolf
    }
    let now = crate::daemon::heartbeat_pair::now_ms() as i64;
    let channel = channel_kind_str(turn.channel);
    match turn.stage {
        0 => {
            if now.saturating_sub(turn.armed_at_ms) < GRACE_MS {
                return SweepAction::None; // give the agent time to respond
            }
            // The audit record (kept per #2042): WARN in the log + event-log
            // line. No operator channel send here any more.
            emit_warn(home, name, &turn, reason);
            advance_stage(name, 1, now);
            SweepAction::NudgeAgent {
                channel,
                msg_id: turn.inbound_msg_id.clone(),
                gap_d: reason == WarnReason::ReplySendFailed,
            }
        }
        1 => {
            if now.saturating_sub(turn.last_stage_at_ms) < STAGE_FOLLOWUP_MS {
                return SweepAction::None;
            }
            match lead_of(name).filter(|l| l != name) {
                Some(lead) => {
                    advance_stage(name, 2, now);
                    SweepAction::EscalateLead {
                        lead,
                        channel,
                        msg_id: turn.inbound_msg_id.clone(),
                        armed_at_ms: turn.armed_at_ms,
                    }
                }
                // No lead to escalate to (solo agent / self-orchestrator) —
                // the operator IS the next rung.
                None => {
                    clear_turn(name); // latch: settled group, never repeated
                    SweepAction::NotifyOperator {
                        channel,
                        msg_id: turn.inbound_msg_id.clone(),
                        armed_at_ms: turn.armed_at_ms,
                    }
                }
            }
        }
        _ => {
            if now.saturating_sub(turn.last_stage_at_ms) < STAGE_FOLLOWUP_MS {
                return SweepAction::None;
            }
            clear_turn(name); // latch: settled group, never repeated
            SweepAction::NotifyOperator {
                channel,
                msg_id: turn.inbound_msg_id.clone(),
                armed_at_ms: turn.armed_at_ms,
            }
        }
    }
}

/// Advance the ladder latch on the live turn (stage + timestamp).
fn advance_stage(name: &str, stage: u8, now: i64) {
    crate::daemon::heartbeat_pair::update_with(name, |p| {
        if let Some(t) = p.pending_user_turn.as_mut() {
            t.stage = stage;
            t.last_stage_at_ms = now;
        }
    });
}

/// Format an epoch-ms timestamp as local `HH:MM` for human-facing text.
fn format_hhmm(epoch_ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(epoch_ms)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// #2042 stage-3 operator text — phrased for a human, never maintainer
/// language: no "ledger", no WARN dump, no `Some("m-...")` debug formats.
pub fn operator_text(agent: &str, channel: &str, armed_at_ms: i64) -> String {
    format!(
        "Your {channel} message to {agent} (received around {}) still hasn't been \
         answered — it was nudged and its lead was notified, but no reply landed yet. \
         It may need a manual look.",
        format_hhmm(armed_at_ms)
    )
}

/// #2042 stage-2 lead text — actionable for the lead, message id included so
/// it can be traced in the agent's inbox.
pub fn lead_text(agent: &str, channel: &str, msg_id: Option<&str>, armed_at_ms: i64) -> String {
    format!(
        "[reply-ledger] {agent} owes the operator a reply to a {channel} message \
         ({}, received {}) — it was nudged once and the reply is still missing. \
         Please check it / re-prime it to answer via the reply tool.",
        msg_id.unwrap_or("id unknown"),
        format_hhmm(armed_at_ms)
    )
}

/// #2042 stage-1 agent nudge text. `gap_d` selects accurate wording — the
/// agent DID call reply but the send failed, so it must RETRY (telling it
/// "you didn't reply" would be wrong).
pub fn nudge_text(channel: &str, msg_id: Option<&str>, gap_d: bool) -> String {
    let id = msg_id.unwrap_or("id unknown");
    if gap_d {
        format!(
            "Your reply to {channel} message {id} FAILED to send — the operator \
             received nothing. Retry it via the reply tool now."
        )
    } else {
        format!(
            "The {channel} message {id} has not been answered on its channel — \
             your output went to the TUI only. Send your answer via the reply \
             tool so the operator receives it."
        )
    }
}

/// #2090 M3 (report mode) progress-backstop nudge text — prods the AGENT to
/// self-report a brief progress update on its in-flight `channel` request after
/// a long quiet stretch. One short line; the daemon prepends the
/// `[AGEND-AUTO kind=progress-backstop]` marker via `inject_with_target_gated`.
/// NOT sent to the channel directly — it asks the agent to post its own (clean,
/// agent-authored) update, so the daemon never relays raw output.
pub fn backstop_nudge_text(channel: &str, armed_at_ms: i64, now_ms: i64) -> String {
    let secs = now_ms.saturating_sub(armed_at_ms).max(0) / 1000;
    format!(
        "[progress] You've been working on the user's {channel} request for ~{secs}s \
         with no update yet — send a brief progress reply now (what you're doing / \
         how far along), then continue."
    )
}

/// #2042 stage-3 delivery: send the human-phrased operator notice through the
/// originating channel (NEVER hardcoded to telegram); the event-log line is the
/// local persistent fallback when the channel is unavailable. Never blocks.
pub fn notify_operator_last_resort(
    home: &std::path::Path,
    name: &str,
    channel: &str,
    armed_at_ms: i64,
) {
    notify_operator_with(
        name,
        channel,
        armed_at_ms,
        |kind, agent, text| {
            crate::channel::lookup_channel_by_name(kind)
                .map(|ch| {
                    ch.send_from_agent(agent, crate::channel::AgentOutboundOp::Reply { text })
                        .is_ok()
                })
                .unwrap_or(false)
        },
        |detail| crate::event_log::log(home, "reply_ledger_operator_notice", name, detail),
    );
}

/// Testable core of [`notify_operator_last_resort`] — the channel send and the
/// log are injected. `send_alert(channel_kind, agent, text) -> delivered?`;
/// `log_sink(detail)` is the always-on persistent record (and the fallback when
/// the channel is down).
fn notify_operator_with<S, L>(
    name: &str,
    channel: &str,
    armed_at_ms: i64,
    send_alert: S,
    log_sink: L,
) where
    S: FnOnce(&str, &str, String) -> bool,
    L: FnOnce(&str),
{
    let text = operator_text(name, channel, armed_at_ms);
    let delivered = send_alert(channel, name, text);
    let detail = format!("channel={channel} armed_at_ms={armed_at_ms} delivered={delivered}");
    log_sink(&detail);
}

/// The Phase-1 audit record (#2042 keeps it): a `tracing::warn!` and an
/// event-log line. The operator-channel alert that Phase-1 sent from here was
/// REMOVED in #2042 — the operator is only contacted at ladder stage 3, in
/// human phrasing ([`operator_text`]).
fn emit_warn(home: &std::path::Path, name: &str, turn: &PendingUserTurn, reason: WarnReason) {
    let kind = channel_kind_str(turn.channel);
    let reason_str = match reason {
        WarnReason::ReplySendFailed => "reply attempted but send failed (Gap D)",
        WarnReason::SilentNoReply => "no reply and no mirror",
    };
    tracing::warn!(
        target: "reply_ledger",
        agent = %name,
        channel = %kind,
        chat = ?turn.chat_id,
        msg_id = ?turn.inbound_msg_id,
        group = ?turn.group_msg_ids,
        reason = %reason_str,
        "user message turn ended un-replied (audit record; #2042 ladder is nudging the agent)"
    );
    let detail = format!(
        "channel={kind} chat={:?} msg={:?} group={:?} reason={reason_str}",
        turn.chat_id, turn.inbound_msg_id, turn.group_msg_ids
    );
    crate::event_log::log(home, "reply_ledger_warn", name, detail.as_str());
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;

    fn turn(channel: ChannelKind, outcome: ReplyOutcome, kind: Option<&str>) -> PendingUserTurn {
        PendingUserTurn {
            inbound_msg_id: Some("m-1".into()),
            group_msg_ids: vec!["m-1".into()],
            group_key: Some("user:u|abc".into()),
            channel,
            chat_id: Some("chat-9".into()),
            inbound_kind: kind.map(str::to_string),
            armed_at_ms: 0,
            reply_outcome: outcome,
            stage: 0,
            last_stage_at_ms: 0,
        }
    }

    fn tmp_home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agend-rl-2042-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&d).ok();
        d
    }

    /// Age the armed turn past GRACE_MS so `sweep` evaluates it this tick.
    fn backdate(name: &str) {
        let past = crate::daemon::heartbeat_pair::now_ms() as i64 - GRACE_MS - 5_000;
        crate::daemon::heartbeat_pair::update_with(name, |p| {
            if let Some(t) = p.pending_user_turn.as_mut() {
                t.armed_at_ms = past;
            }
        });
    }

    /// Age the last ladder stage past STAGE_FOLLOWUP_MS so the next rung fires.
    fn backdate_stage(name: &str) {
        let past = crate::daemon::heartbeat_pair::now_ms() as i64 - STAGE_FOLLOWUP_MS - 5_000;
        crate::daemon::heartbeat_pair::update_with(name, |p| {
            if let Some(t) = p.pending_user_turn.as_mut() {
                t.last_stage_at_ms = past;
            }
        });
    }

    fn no_lead(_: &str) -> Option<String> {
        None
    }

    fn arm_user(name: &str, msg_id: &str, text: &str) {
        arm(
            name,
            ChannelKind::Telegram,
            Some(msg_id.into()),
            Some("chat1".into()),
            None,
            Some("user:op"),
            Some(text),
        );
    }

    // ── classify (pure) ─────────────────────────────────────────────────
    #[test]
    fn classify_closed_on_mirror_or_delivered() {
        assert_eq!(
            classify(
                true,
                &turn(ChannelKind::Telegram, ReplyOutcome::Pending, None)
            ),
            Closure::Closed,
            "mirror dispatched ⟹ closed"
        );
        assert_eq!(
            classify(
                false,
                &turn(ChannelKind::Telegram, ReplyOutcome::Delivered, None)
            ),
            Closure::Closed,
            "successful reply ⟹ closed"
        );
    }

    #[test]
    fn classify_warns_gap_d_and_silent() {
        assert_eq!(
            classify(
                false,
                &turn(ChannelKind::Telegram, ReplyOutcome::SendFailed, None)
            ),
            Closure::Warn(WarnReason::ReplySendFailed),
            "Gap D: reply attempted, send failed ⟹ warn"
        );
        assert_eq!(
            classify(
                false,
                &turn(ChannelKind::Telegram, ReplyOutcome::Pending, None)
            ),
            Closure::Warn(WarnReason::SilentNoReply),
            "no reply, no mirror ⟹ warn"
        );
    }

    // Short ack / non-query (non-user kind) does NOT trigger the ladder.
    #[test]
    fn classify_never_warns_non_user_kind_3() {
        for k in ["update", "report", "ci-watch", "system"] {
            assert_eq!(
                classify(
                    false,
                    &turn(ChannelKind::Telegram, ReplyOutcome::Pending, Some(k))
                ),
                Closure::NeverWarn,
                "kind={k} must never warn"
            );
        }
    }

    // ── #2042 group identity (pure) ─────────────────────────────────────
    #[test]
    fn group_key_normalizes_whitespace_and_case_2042() {
        assert_eq!(
            group_key(Some("user:op"), Some("  Fix   the BUG ")),
            group_key(Some("user:op"), Some("fix the bug")),
            "normalized duplicates must share a key"
        );
        assert_ne!(
            group_key(Some("user:op"), Some("fix the bug")),
            group_key(Some("user:other"), Some("fix the bug")),
            "different senders must not group"
        );
        assert_eq!(group_key(Some("user:op"), Some("   ")), None);
        assert_eq!(group_key(Some("user:op"), None), None);
    }

    // ── #2042 §3.9 ① no reply → agent nudge (with msg id), latched ──────
    #[test]
    fn sweep_stage1_nudges_agent_once_2042() {
        let home = tmp_home("stage1");
        let n = "rl2042-stage1";
        arm_user(n, "m-s1", "please check the deploy");
        backdate(n);
        assert_eq!(
            sweep(&home, n, &no_lead),
            SweepAction::NudgeAgent {
                channel: "telegram",
                msg_id: Some("m-s1".into()),
                gap_d: false,
            },
            "① un-replied user message must nudge the agent with the msg id"
        );
        // Latch: within the follow-up window the ladder does NOT repeat.
        assert_eq!(
            sweep(&home, n, &no_lead),
            SweepAction::None,
            "stage 1 must fire at most once (escalate-don't-repeat)"
        );
        clear_turn(n);
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #2042 §3.9 ② reply to ANY member settles the whole group ────────
    #[test]
    fn reply_settles_whole_group_2042() {
        let home = tmp_home("group");
        let n = "rl2042-group";
        arm_user(n, "m-g1", "same logical question");
        arm_user(n, "m-g2", "same  logical   QUESTION"); // duplicate → joins
        let pair = crate::daemon::heartbeat_pair::snapshot_for(n);
        let t = pair.pending_user_turn.expect("turn armed");
        assert_eq!(
            t.group_msg_ids,
            vec!["m-g1".to_string(), "m-g2".to_string()],
            "duplicate delivery must JOIN the group, not supersede"
        );
        assert_eq!(t.stage, 0, "join must not touch ladder state");

        record_reply_outcome(n, true); // reply settles the group
        backdate(n);
        assert_eq!(
            sweep(&home, n, &no_lead),
            SweepAction::None,
            "② a reply settles EVERY delivery id in the group — no escalation"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #2042 §3.9 ③ redelivery does not open a new obligation ──────────
    #[test]
    fn redelivery_after_settlement_not_rearmed_2042() {
        let home = tmp_home("redelivery");
        let n = "rl2042-redelivery";
        arm_user(n, "m-r1", "deploy the fix");
        record_reply_outcome(n, true); // answered

        // Redelivery / duplicate resend of the SAME logical message.
        arm_user(n, "m-r3", "deploy   the fix");
        let pair = crate::daemon::heartbeat_pair::snapshot_for(n);
        assert!(
            pair.pending_user_turn.is_none(),
            "③ a redelivery of a settled message must NOT re-arm an obligation"
        );
        backdate(n);
        assert_eq!(sweep(&home, n, &no_lead), SweepAction::None);

        // A genuinely NEW message still arms normally.
        arm_user(n, "m-r4", "a different question");
        assert!(
            crate::daemon::heartbeat_pair::snapshot_for(n)
                .pending_user_turn
                .is_some(),
            "new content must still arm a fresh obligation"
        );
        clear_turn(n);
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #2042 ladder: stage 2 lead escalation, stage 3 operator latch ───
    #[test]
    fn sweep_ladder_lead_then_operator_2042() {
        let home = tmp_home("ladder");
        let n = "rl2042-ladder";
        let lead = |_: &str| Some("the-lead".to_string());
        arm_user(n, "m-l1", "needs an answer");
        backdate(n);
        assert!(matches!(
            sweep(&home, n, &lead),
            SweepAction::NudgeAgent { .. }
        ));
        // Within the follow-up window: nothing.
        assert_eq!(sweep(&home, n, &lead), SweepAction::None);
        // Past the window: lead escalation, exactly once.
        backdate_stage(n);
        assert_eq!(
            sweep(&home, n, &lead),
            SweepAction::EscalateLead {
                lead: "the-lead".into(),
                channel: "telegram",
                msg_id: Some("m-l1".into()),
                armed_at_ms: crate::daemon::heartbeat_pair::snapshot_for(n)
                    .pending_user_turn
                    .as_ref()
                    .unwrap()
                    .armed_at_ms,
            },
            "second miss must escalate to the lead"
        );
        assert_eq!(
            sweep(&home, n, &lead),
            SweepAction::None,
            "lead escalation must fire at most once per obligation"
        );
        // Past another window: operator last resort, then the turn is latched.
        backdate_stage(n);
        assert!(
            matches!(
                sweep(&home, n, &lead),
                SweepAction::NotifyOperator {
                    channel: "telegram",
                    ..
                }
            ),
            "third rung is the operator, last resort"
        );
        assert!(
            crate::daemon::heartbeat_pair::snapshot_for(n)
                .pending_user_turn
                .is_none(),
            "operator stage clears (latches) the obligation"
        );
        assert_eq!(
            sweep(&home, n, &lead),
            SweepAction::None,
            "nothing ever repeats after the operator stage"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #2042: no lead → stage 2 skips straight to the operator ─────────
    #[test]
    fn sweep_no_lead_skips_to_operator_2042() {
        let home = tmp_home("nolead");
        let n = "rl2042-nolead";
        arm_user(n, "m-n1", "solo agent question");
        backdate(n);
        assert!(matches!(
            sweep(&home, n, &no_lead),
            SweepAction::NudgeAgent { .. }
        ));
        backdate_stage(n);
        assert!(
            matches!(
                sweep(&home, n, &no_lead),
                SweepAction::NotifyOperator { .. }
            ),
            "no lead to escalate to — the operator is the next rung"
        );
        assert_eq!(sweep(&home, n, &no_lead), SweepAction::None);
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #2042: a self-led agent (lead == self) also skips to operator ───
    #[test]
    fn sweep_self_lead_skips_to_operator_2042() {
        let home = tmp_home("selflead");
        let n = "rl2042-selflead";
        let self_lead = |a: &str| Some(a.to_string());
        arm_user(n, "m-sl1", "orchestrator question");
        backdate(n);
        assert!(matches!(
            sweep(&home, n, &self_lead),
            SweepAction::NudgeAgent { .. }
        ));
        backdate_stage(n);
        assert!(
            matches!(
                sweep(&home, n, &self_lead),
                SweepAction::NotifyOperator { .. }
            ),
            "an agent that is its own lead must not escalate to itself"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #2042 Gap D: send-failed reply is nudged with RETRY wording ─────
    #[test]
    fn sweep_gap_d_nudges_with_retry_wording_2042() {
        let home = tmp_home("gapd");
        let n = "rl2042-gapd";
        arm_user(n, "m-d1", "gap d case");
        record_reply_outcome(n, false); // reply attempted, send failed
        backdate(n);
        assert_eq!(
            sweep(&home, n, &no_lead),
            SweepAction::NudgeAgent {
                channel: "telegram",
                msg_id: Some("m-d1".into()),
                gap_d: true,
            },
            "Gap D must nudge with gap_d=true (retry wording, not 'you didn't reply')"
        );
        assert!(
            nudge_text("telegram", Some("m-d1"), true).contains("Retry"),
            "Gap D nudge text must instruct a RETRY"
        );
        clear_turn(n);
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #2042: takeover/mirror clear settles the group too ──────────────
    #[test]
    fn clear_turn_settles_group_2042() {
        let n = "rl2042-clear";
        arm_user(n, "m-c1", "handled in tui");
        clear_turn(n); // TUI takeover / mirror closure path
        arm_user(n, "m-c2", "handled   in TUI"); // redelivery of same content
        assert!(
            crate::daemon::heartbeat_pair::snapshot_for(n)
                .pending_user_turn
                .is_none(),
            "a cleared (taken-over/mirrored) group must suppress duplicate re-arms"
        );
    }

    // ── human-facing texts: no maintainer language ───────────────────────
    #[test]
    fn operator_text_is_human_phrased_2042() {
        let text = operator_text("general", "telegram", 0);
        assert!(text.contains("general"), "names the agent");
        assert!(
            !text.to_lowercase().contains("ledger") && !text.contains("WARN"),
            "no maintainer language: {text}"
        );
        assert!(
            !text.contains("Some("),
            "no debug formats in operator-facing text: {text}"
        );
        // HH:MM (or the explicit '?') is present.
        assert!(
            text.contains(':') || text.contains('?'),
            "carries the received time: {text}"
        );
    }

    #[test]
    fn nudge_and_lead_texts_carry_msg_id_2042() {
        let nudge = nudge_text("telegram", Some("m-77"), false);
        assert!(nudge.contains("m-77"), "nudge names the message id");
        assert!(
            nudge.contains("reply tool"),
            "nudge instructs the reply tool"
        );
        let lead = lead_text("dev-1", "telegram", Some("m-77"), 0);
        assert!(lead.contains("dev-1") && lead.contains("m-77"));
    }

    // ── #2090 M3 progress-backstop nudge text ───────────────────────────
    #[test]
    fn backstop_nudge_text_formats_elapsed_and_mentions_progress() {
        // 45s elapsed (45_000ms) on the telegram channel.
        let text = backstop_nudge_text("telegram", 0, 45_000);
        assert!(
            text.to_lowercase().contains("progress"),
            "must mention progress: {text}"
        );
        assert!(text.contains("telegram"), "names the channel: {text}");
        assert!(
            text.contains("~45s"),
            "formats elapsed seconds (45_000ms → ~45s): {text}"
        );
        // One short line — no embedded newlines (single channel reply).
        assert!(!text.contains('\n'), "single line: {text}");
        // Clamps a negative/clock-skew elapsed to 0 rather than underflowing.
        let skewed = backstop_nudge_text("telegram", 99_000, 1_000);
        assert!(skewed.contains("~0s"), "clamps negative elapsed: {skewed}");
    }

    // ── operator notify plumbing: same-channel routing + log fallback ───
    #[test]
    fn operator_notice_routes_channel_and_logs_2042() {
        let got_kind: Cell<&'static str> = Cell::new("");
        let logged = Cell::new(false);
        notify_operator_with(
            "ag",
            "discord",
            0,
            |kind, _agent, _text| {
                got_kind.set(if kind == "discord" {
                    "discord"
                } else {
                    "other"
                });
                true
            },
            |_detail| logged.set(true),
        );
        assert_eq!(
            got_kind.get(),
            "discord",
            "operator notice routed to the originating channel"
        );
        assert!(logged.get(), "audit log always fires");

        // Channel unavailable → the local persistent log is the fallback and
        // nothing blocks/panics.
        let logged2 = Cell::new(false);
        notify_operator_with(
            "ag",
            "telegram",
            0,
            |_k, _a, _t| false,
            |_d| logged2.set(true),
        );
        assert!(logged2.get(), "channel-down notice still lands in the log");
    }
}
