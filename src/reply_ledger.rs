//! #1665 reply-ledger (Phase 1) — a WARN-only delivery-closure audit.
//!
//! Tracks, per agent, the in-flight user channel message turn and emits a
//! `tracing::warn!` (plus an event-log line and a best-effort same-channel
//! alert) when a turn ends with the user's message un-replied. It is an AUDIT,
//! not a second delivery path: it never re-delivers the agent's missing reply,
//! and — the IRON RULE — it never blocks/rejects the agent. Every ledger op is
//! infallible (a lock update) or swallows its error.
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
/// (channel, chat, message) identity so a WARN routes back to the RIGHT channel
/// and multiple channels never cross-talk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUserTurn {
    pub inbound_msg_id: Option<String>,
    pub channel: ChannelKind,
    /// Telegram topic / Discord thread id — the per-chat scope of the key.
    pub chat_id: Option<String>,
    pub inbound_kind: Option<String>,
    pub armed_at_ms: i64,
    pub reply_outcome: ReplyOutcome,
}

/// Closure verdict for a turn being evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closure {
    /// User got a reply or a mirror — never warn.
    Closed,
    /// Not reply-eligible (non-user kind) — never warn.
    NeverWarn,
    /// Un-closed but closeable — warn.
    Warn(WarnReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarnReason {
    /// Gap D — reply attempted, send failed.
    ReplySendFailed,
    /// No reply, no mirror — the user's message got nothing.
    SilentNoReply,
}

/// Grace window after arming before a turn is eligible for a silent-drop WARN —
/// gives the agent time to respond before we cry wolf. Combined with the
/// `agent_is_busy` gate in [`sweep`], this is the primary false-warn defense
/// (a slow-but-working agent is `busy`, and even a settled agent gets the grace).
const GRACE_MS: i64 = 30_000;

/// Inbound `kind`s that never expect a user-facing reply (defensive — user
/// channel messages usually carry no kind, but an agent/system message that
/// somehow armed must not warn). Mirrors the spike's never-warn boundary.
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

/// Arm (or supersede) the in-flight turn for `name`. Called from the inbox
/// dequeue arm site for a user channel message. Overwriting an existing turn IS
/// the supersede path (the old turn is dropped without a warn — the user moved
/// on). Infallible.
pub fn arm(
    name: &str,
    channel: ChannelKind,
    inbound_msg_id: Option<String>,
    chat_id: Option<String>,
    inbound_kind: Option<String>,
) {
    let turn = PendingUserTurn {
        inbound_msg_id,
        channel,
        chat_id,
        inbound_kind,
        armed_at_ms: crate::daemon::heartbeat_pair::now_ms() as i64,
        reply_outcome: ReplyOutcome::Pending,
    };
    crate::daemon::heartbeat_pair::update_with(name, |p| {
        p.pending_user_turn = Some(turn.clone());
    });
}

/// Record the agent's `reply` outcome (Gap D). Called from `handle_reply` on
/// every exit: `Ok` → Delivered, any send/lookup failure → SendFailed. No-op if
/// no turn is armed. Infallible — never affects the reply's return value.
pub fn record_reply_outcome(name: &str, delivered: bool) {
    crate::daemon::heartbeat_pair::update_with(name, |p| {
        if let Some(t) = p.pending_user_turn.as_mut() {
            t.reply_outcome = if delivered {
                ReplyOutcome::Delivered
            } else {
                ReplyOutcome::SendFailed
            };
        }
    });
}

/// Clear the in-flight turn WITHOUT warning — the never-warn closure paths:
/// mirror dispatched (user got the mirror), operator TUI takeover (can't tell
/// abandoned vs handled), supersede. Infallible.
pub fn clear_turn(name: &str) {
    crate::daemon::heartbeat_pair::update_with(name, |p| {
        p.pending_user_turn = None;
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

/// Supervisor-tick fallback: evaluate a turn that never hit a clear site. Warns
/// only when the turn has aged past the grace window AND the agent has settled
/// (not mid-generation) — so a slow-but-working agent never false-warns. Always
/// clears the turn afterward. Infallible (swallows everything).
pub fn sweep(home: &std::path::Path, name: &str) {
    let pair = crate::daemon::heartbeat_pair::snapshot_for(name);
    let Some(turn) = pair.pending_user_turn.clone() else {
        return;
    };
    let now = crate::daemon::heartbeat_pair::now_ms() as i64;
    if now.saturating_sub(turn.armed_at_ms) < GRACE_MS {
        return; // give the agent time to respond
    }
    if crate::snapshot::agent_is_busy(home, name) {
        return; // still generating — not a silent drop, don't cry wolf
    }
    if let Closure::Warn(reason) = classify(pair.mirror_dispatched_for_turn, &turn) {
        emit_warn(home, name, &turn, reason);
    }
    clear_turn(name);
}

/// Production WARN emission: a `tracing::warn!`, an event-log line (always — the
/// persistent record), and a best-effort same-channel alert through the channel
/// abstraction (NEVER hardcoded to telegram). When the channel is unavailable or
/// the send errors, the event-log line is the local persistent fallback. Never
/// blocks.
fn emit_warn(home: &std::path::Path, name: &str, turn: &PendingUserTurn, reason: WarnReason) {
    emit_warn_with(
        name,
        turn,
        reason,
        |kind, agent, text| {
            crate::channel::lookup_channel_by_name(kind)
                .map(|ch| {
                    ch.send_from_agent(agent, crate::channel::AgentOutboundOp::Reply { text })
                        .is_ok()
                })
                .unwrap_or(false)
        },
        |detail| crate::event_log::log(home, "reply_ledger_warn", name, detail),
    );
}

/// Testable core of [`emit_warn`] — the channel send and the log are injected.
/// `send_alert(channel_kind, agent, text) -> delivered?`; `log_sink(detail)` is
/// the always-on persistent record (and the fallback when the channel is down).
fn emit_warn_with<S, L>(
    name: &str,
    turn: &PendingUserTurn,
    reason: WarnReason,
    send_alert: S,
    log_sink: L,
) where
    S: FnOnce(&'static str, &str, String) -> bool,
    L: FnOnce(&str),
{
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
        reason = %reason_str,
        "user message turn ended un-replied (Phase-1 WARN-only audit; verdict delivered, agent not blocked)"
    );
    let alert = format!(
        "⚠ reply-ledger: a user message (msg {:?}) received NO reply — {reason_str}. \
         [#1665 Phase-1 WARN-only audit]",
        turn.inbound_msg_id
    );
    let delivered = send_alert(kind, name, alert);
    let detail = format!(
        "channel={kind} chat={:?} msg={:?} reason={reason_str} alert_delivered={delivered}",
        turn.chat_id, turn.inbound_msg_id
    );
    // event-log ALWAYS — it is both the audit record and the channel-unavailable
    // local-persistent fallback (acceptance ②).
    log_sink(&detail);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn turn(channel: ChannelKind, outcome: ReplyOutcome, kind: Option<&str>) -> PendingUserTurn {
        PendingUserTurn {
            inbound_msg_id: Some("m-1".into()),
            channel,
            chat_id: Some("chat-9".into()),
            inbound_kind: kind.map(str::to_string),
            armed_at_ms: 0,
            reply_outcome: outcome,
        }
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

    // ③ short ack / non-query (non-user kind) does NOT trigger a warn.
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

    // ① query un-replied → alert routed to the SAME channel (abstraction, not
    //   hardcoded telegram) + the audit log fires.
    #[test]
    fn warn_alerts_same_channel_and_logs_1() {
        let got_kind: Cell<&'static str> = Cell::new("");
        let logged = Cell::new(false);
        emit_warn_with(
            "ag",
            &turn(ChannelKind::Discord, ReplyOutcome::Pending, None),
            WarnReason::SilentNoReply,
            |kind, _agent, _text| {
                got_kind.set(kind);
                true // channel available, delivered
            },
            |_detail| logged.set(true),
        );
        assert_eq!(
            got_kind.get(),
            "discord",
            "alert routed to the originating channel"
        );
        assert!(logged.get(), "audit log always fires");
    }

    // ② original channel unavailable → local persistent log is the fallback
    //   (and the agent is never blocked by the failed send).
    #[test]
    fn warn_falls_back_to_local_log_when_channel_unavailable_2() {
        let logged = Cell::new(false);
        emit_warn_with(
            "ag",
            &turn(ChannelKind::Telegram, ReplyOutcome::SendFailed, None),
            WarnReason::ReplySendFailed,
            |_kind, _agent, _text| false, // channel down / send failed
            |_detail| logged.set(true),
        );
        assert!(
            logged.get(),
            "channel-unavailable WARN must still land in the local persistent log"
        );
    }

    // ④ multi-channel: two turns on different channels route to their OWN
    //   channel — no cross-talk.
    #[test]
    fn multi_channel_no_cross_talk_4() {
        for ch in [ChannelKind::Telegram, ChannelKind::Discord] {
            let got: Cell<&'static str> = Cell::new("");
            emit_warn_with(
                "ag",
                &turn(ch, ReplyOutcome::Pending, None),
                WarnReason::SilentNoReply,
                |kind, _a, _t| {
                    got.set(kind);
                    true
                },
                |_d| {},
            );
            assert_eq!(
                got.get(),
                channel_kind_str(ch),
                "{ch:?} must route to its own channel"
            );
        }
    }

    // ⑤ audit failure does not block: a failing alert send still completes
    //   emit_warn_with normally (it returns `()`, swallows the failure) — the
    //   ledger never propagates an error into the agent path.
    #[test]
    fn audit_failure_never_blocks_5() {
        // send_alert reports failure; log_sink also a no-op — emit must not panic
        // or propagate. (record_reply_outcome / clear_turn / arm are infallible
        // update_with calls by construction — no Result to block on.)
        emit_warn_with(
            "ag",
            &turn(ChannelKind::Telegram, ReplyOutcome::Pending, None),
            WarnReason::SilentNoReply,
            |_k, _a, _t| false,
            |_d| {},
        );
        // reached here ⟹ no block / no panic.
    }
}
