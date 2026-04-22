//! Outbound semantic events — emitted by the daemon, consumed by channel
//! adapters, translated into platform actions via capability-gated
//! degradation.
//!
//! Per `docs/PLAN-channel-ux-layer.md` §4 + §6, `UxEvent` is what the daemon
//! observes about user ↔ agent interaction (message received, picked up,
//! replied). It is NOT a Telegram / Discord / Slack API return — the trigger
//! is a daemon-side state change. Adapters sit behind the [`UxEventSink`]
//! trait and, via [`select_action`], pick the strongest primitive their
//! capabilities support: react > edit > send > noop.
//!
//! ## Scope of this file
//!
//! Q1 delivery-confirmation subset: [`UxEvent::UserMsgReceived`],
//! [`UxEvent::AgentPickedUp`], and [`UxEvent::AgentReplied`] (T3).
//! Q2 fleet-visibility subset: [`UxEvent::Fleet`] wrapping a
//! [`FleetEvent`] — routed by `sink_registry` to any registered sink,
//! rendered per adapter (PR-B lands the Telegram renderer). Stage B-UX
//! design: `docs/DESIGN-stage-b-ux.md` §4.
//!
//! `AgentThinking` / `AgentIdle` / `AgentRateLimited` / `AgentCrashed` /
//! `AgentRestarted` (remaining Q1 events) are deferred — they do not
//! touch the Telegram send / edit / react paths in Q1, so landing
//! them here would be speculative dead code.
//!
//! ## Plan reference
//!
//! The Q1 rendering table comes straight from `PLAN-channel-ux-layer.md`
//! §6:
//!
//! | Event            | `react`  | `edit` only        | None         |
//! |------------------|----------|--------------------|--------------|
//! | UserMsgReceived  | 👀 on origin | edit origin → `[delivered]` | `✓ delivered` |
//! | AgentPickedUp    | stack ✅ on origin | edit origin → `[read]` | no-op        |
//! | AgentReplied     | send reply | send reply     | send reply  |
//!
//! `typing_indicator` is not part of T3 (no `AgentThinking` variant here).

use super::{BindingRef, ChannelCapabilities, MsgRef};

/// Semantic event describing what the daemon just observed. Adapters
/// consume these via [`UxEventSink::emit`] and render into a
/// platform-specific action picked by [`select_action`].
#[derive(Debug, Clone)]
pub enum UxEvent {
    /// A user message was received and routed to an agent. Trigger site:
    /// dispatcher ingress (not yet wired in T3; emission-producing code
    /// lands with the dispatcher PR).
    UserMsgReceived {
        /// Reference to the user's original message, so the adapter can
        /// react / edit against it.
        origin_msg: MsgRef,
        /// Receiving agent's name — carried for logging / renderer
        /// context; not required by the Q1 table itself.
        agent: String,
    },
    /// The agent's inbox dequeued the message. Trigger site: per-agent
    /// dispatcher (also out of scope for T3 wiring).
    AgentPickedUp { origin_msg: MsgRef, agent: String },
    /// The agent produced a reply intended for a specific binding.
    /// Trigger site: agent output path.
    AgentReplied {
        agent: String,
        /// Binding the reply is destined for (e.g. the user's topic).
        binding: BindingRef,
        /// Rendered reply text. The content *is* the primitive for this
        /// event, so there is no degradation ladder — every capability
        /// combination renders via `send`.
        text: String,
    },
    /// Cross-instance fleet activity — delegations, result reports,
    /// decisions, and broadcasts observed on the daemon's MCP surface.
    /// Rendered into a separate `fleet_binding` (see plan §6 S2c),
    /// NOT into the origin user's thread. See [`FleetEvent`] and
    /// `docs/DESIGN-stage-b-ux.md` §4 for the producer hook table.
    Fleet(FleetEvent),
}

/// Cross-instance activity events. These travel from MCP handlers to
/// `sink_registry::registry()`, which fans them out to every registered
/// sink. Unlike Q1 events, they have no capability-degradation ladder —
/// the target is always the configured `fleet_binding`, and rendering is
/// pure format (see [`select_action`] Fleet arm for why).
///
/// The enum shape is locked by `docs/PLAN-channel-ux-layer.md` §4 with
/// one deviation: `task_id` is `Option<String>` rather than a newtype
/// `TaskId`. Rationale: the `correlation_id` arg on `report_result` is
/// an ad-hoc caller-chosen string (e.g. `"AGD-42"`); making it required
/// with a typed wrapper overstates what the MCP surface actually
/// guarantees. Renderers display the id when present and omit it
/// otherwise. Decision: `docs/DESIGN-stage-b-ux.md` §9 Q1, by general.
#[derive(Debug, Clone)]
pub enum FleetEvent {
    /// `delegate_task` MCP call landed.
    DelegateTask {
        from: String,
        to: String,
        summary: String,
        task_id: Option<String>,
    },
    /// `report_result` MCP call landed. `task_id` mirrors the caller's
    /// `correlation_id` arg when it was supplied.
    ReportResult {
        from: String,
        to: String,
        summary: String,
        task_id: Option<String>,
    },
    /// `post_decision` MCP call succeeded. Anonymous posts (no
    /// `AGEND_INSTANCE_NAME` / explicit `instance_name`) are
    /// deliberately NOT emitted — fleet provenance requires an
    /// identified author (see `docs/DESIGN-stage-b-ux.md` §4.3).
    PostDecision {
        by: String,
        title: String,
        decision_id: String,
    },
    /// `broadcast` MCP call completed its fan-out. `recipients` lists
    /// every instance the broadcast was actually sent to (after
    /// self-filter).
    Broadcast {
        from: String,
        recipients: Vec<String>,
        summary: String,
    },
}

/// Platform-agnostic action chosen by [`select_action`] given a
/// `(UxEvent × ChannelCapabilities)` pair. Adapters match on this and
/// call their own native primitive.
///
/// Note: no `PartialEq` derive — `BindingRef` and `MsgRef` are
/// deliberately opaque (payload is `Arc<dyn Any>`). Tests assert via
/// pattern-matching and field-by-field checks instead.
#[derive(Debug, Clone)]
pub enum UxAction {
    /// Apply a reaction emoji on an existing message.
    ///
    /// `instance` is the receiving agent's name — the key used by
    /// `try_telegram_*` helpers to resolve the routing topic. It is
    /// copied from the originating `UxEvent`'s `agent` field, NOT
    /// derived from `BindingRef::display_tag` (which is a
    /// human-readable label like "TG#229", not a lookup key — see
    /// `config.instances.get(instance_name)` in
    /// `channel::telegram::try_telegram_reply`).
    React {
        instance: String,
        msg: MsgRef,
        emoji: &'static str,
    },
    /// Edit an existing message to a new text body. See `React` for
    /// `instance` semantics.
    EditText {
        instance: String,
        msg: MsgRef,
        text: String,
    },
    /// Send a new text message to a binding. See `React` for
    /// `instance` semantics — this is the variant where getting it
    /// wrong matters: `try_telegram_reply` bails if the instance
    /// name does not match a fleet entry.
    SendText {
        instance: String,
        binding: BindingRef,
        text: String,
    },
    /// Do nothing — the adapter's capability matrix has no way to
    /// express this event without resorting to a noisier fallback the
    /// plan explicitly forbids (plan §6 anti-feature on status text).
    Noop,
}

/// Render a [`FleetEvent`] into the one-liner log shape defined in
/// `docs/DESIGN-stage-b-ux.md` §5.1 (which in turn mirrors the `S2c`
/// exemplars in `docs/PLAN-channel-ux-layer.md` §6). Pure fn — no I/O,
/// no time, snapshot-testable.
///
/// Format shape by variant:
///
/// ```text
/// [from → to]         DELEGATE   <summary> (#<task_id>)
/// [from → to]         REPORT     <summary> (#<task_id>)
/// [by solo]           DECISION   <title> (D-<decision_id>)
/// [from → *N]         BROADCAST  <summary>            // N recipients, compact
/// [from → a,b,c]      BROADCAST  <summary>            // ≤3 recipients, named
/// [from → a,b,…+N]    BROADCAST  <summary>            // >3 recipients, elided
/// ```
///
/// - The prefix (everything up to and including the trailing double space
///   before `<summary>`) is reserved for the bracket tag and verb column.
///   Summary is truncated to `max_bytes - prefix_len` and an ellipsis
///   `…` is appended; `max_bytes = 0` is treated as "no cap" because
///   Telegram's actual cap (4096 bytes) is always plenty for a one-liner
///   and the 0-case is easier for callers than `Option<usize>`.
/// - `task_id` / `decision_id` suffixes are appended *after* truncation so
///   provenance ids never get eaten by the summary cap; in practice the
///   combined length is always far below `max_bytes`.
pub fn format_fleet_oneliner(fe: &FleetEvent, max_bytes: usize) -> String {
    // Render the `[from → to]` / `[by solo]` / `[from → recipients]`
    // bracket tag. Kept inline (not a separate fn) because the verb +
    // ID-suffix shape differs per variant and splitting would just push
    // noise back up.
    let (tag, verb, body, suffix) = match fe {
        FleetEvent::DelegateTask {
            from,
            to,
            summary,
            task_id,
        } => (
            format!("[{from} → {to}]"),
            "DELEGATE",
            summary.clone(),
            task_id
                .as_deref()
                .map(|id| format!(" (#{id})"))
                .unwrap_or_default(),
        ),
        FleetEvent::ReportResult {
            from,
            to,
            summary,
            task_id,
        } => (
            format!("[{from} → {to}]"),
            "REPORT",
            summary.clone(),
            task_id
                .as_deref()
                .map(|id| format!(" (#{id})"))
                .unwrap_or_default(),
        ),
        FleetEvent::PostDecision {
            by,
            title,
            decision_id,
        } => (
            format!("[{by} solo]"),
            "DECISION",
            title.clone(),
            format!(" (D-{decision_id})"),
        ),
        FleetEvent::Broadcast {
            from,
            recipients,
            summary,
        } => {
            let tag = format_broadcast_tag(from, recipients);
            (tag, "BROADCAST", summary.clone(), String::new())
        }
    };

    // `tag  VERB  <summary>`: verb column is separated by two spaces on
    // each side to keep the §5.1 exemplars diff-visible.
    let prefix = format!("{tag}  {verb}  ");
    let prefix_len = prefix.len();
    let available = if max_bytes == 0 {
        usize::MAX
    } else {
        max_bytes.saturating_sub(prefix_len + suffix.len())
    };
    let body_rendered = truncate_with_ellipsis(&body, available);
    format!("{prefix}{body_rendered}{suffix}")
}

/// Render the broadcast recipient tag. Extracted to keep the variant
/// arm of [`format_fleet_oneliner`] focused on shape; the rules below
/// map the §5.1 exemplar table:
///
/// - empty        → `[from → *]`  (matches the "DECISION solo" feel —
///   nothing to fan out to; kept parallel to `[from → *N]` so no broadcast
///   tag ever reads like a bare `[from → ]` with a dangling arrow)
/// - ≤3 named     → `[from → a,b,c]`
/// - >3 named     → `[from → a,b,…+N]`
fn format_broadcast_tag(from: &str, recipients: &[String]) -> String {
    match recipients.len() {
        0 => format!("[{from} → *]"),
        n if n <= 3 => format!("[{from} → {}]", recipients.join(",")),
        n => {
            let head = recipients[..2].join(",");
            let rest = n - 2;
            format!("[{from} → {head},…+{rest}]")
        }
    }
}

/// Truncate `body` to fit in `max_bytes`, appending a single `…` (which
/// itself costs 3 bytes in UTF-8). Zero-cost when `body` already fits.
/// `body` is sliced on a UTF-8 char boundary walked from the left to
/// avoid ever producing invalid UTF-8 in the output.
fn truncate_with_ellipsis(body: &str, max_bytes: usize) -> String {
    if body.len() <= max_bytes {
        return body.to_string();
    }
    const ELLIPSIS: &str = "…";
    // Reserve space for the ellipsis suffix. If `max_bytes` is
    // itself < 3 bytes (no room even for the ellipsis), just emit
    // the ellipsis; prefix + suffix already consumed the budget and
    // losing the body entirely is preferable to panicking.
    if max_bytes < ELLIPSIS.len() {
        return ELLIPSIS.to_string();
    }
    let budget = max_bytes - ELLIPSIS.len();
    // Walk char boundaries from the left so we never split in the
    // middle of a multi-byte codepoint.
    let mut end = 0;
    for (i, _) in body.char_indices() {
        if i > budget {
            break;
        }
        end = i;
    }
    format!("{}{ELLIPSIS}", &body[..end])
}

/// Sink trait any consumer of `UxEvent` implements. `TelegramChannel`
/// is the only real impl in T3; the daemon-wide sink registry / merged
/// stream of sinks is a follow-up PR once there's a real producer.
pub trait UxEventSink: Send + Sync {
    /// Fire-and-forget delivery. Impls must not panic; transport errors
    /// are logged, not propagated (a failed reaction is never a reason
    /// to crash the daemon).
    fn emit(&self, event: &UxEvent);
}

/// Default sink that records events to `tracing::debug!` only. Used
/// by tests / early wiring where no real adapter is present.
pub struct NoopUxSink;

impl UxEventSink for NoopUxSink {
    fn emit(&self, event: &UxEvent) {
        tracing::debug!(?event, "NoopUxSink::emit (drop)");
    }
}

/// Pure decision function: given an event and the adapter's caps,
/// pick the strongest primitive the adapter can express.
///
/// Caller is cap-blind by design (plan §6 anti-feature: "never mirror
/// AgentThinking into a text 'agent is typing...' message when no
/// typing-indicator capability exists"). The silence fallback is the
/// adapter's call; this function is where that call is made.
///
/// Exhaustive on the event axis so reviewers can diff the Q1 table
/// (`PLAN-channel-ux-layer.md` §6) against the code 1-to-1.
pub fn select_action(event: &UxEvent, caps: &ChannelCapabilities) -> UxAction {
    match event {
        UxEvent::UserMsgReceived { origin_msg, agent } => {
            if caps.react {
                UxAction::React {
                    instance: agent.clone(),
                    msg: origin_msg.clone(),
                    emoji: "👀",
                }
            } else if caps.edit {
                UxAction::EditText {
                    instance: agent.clone(),
                    msg: origin_msg.clone(),
                    text: "[delivered]".to_string(),
                }
            } else {
                // Plan §6 last column: short ack message to the same
                // binding. `origin_msg.binding` is the right target
                // because the ack is a reply into the user's thread.
                UxAction::SendText {
                    instance: agent.clone(),
                    binding: origin_msg.binding.clone(),
                    text: "✓ delivered".to_string(),
                }
            }
        }
        UxEvent::AgentPickedUp { origin_msg, agent } => {
            if caps.react {
                UxAction::React {
                    instance: agent.clone(),
                    msg: origin_msg.clone(),
                    // Plan §6: "stack ✅ on origin" alongside the earlier 👀.
                    emoji: "✅",
                }
            } else if caps.edit {
                UxAction::EditText {
                    instance: agent.clone(),
                    msg: origin_msg.clone(),
                    text: "[read]".to_string(),
                }
            } else {
                // Plan §6 last column: "no-op (already acked)" — the
                // UserMsgReceived render already put a `✓ delivered`
                // message in the thread; a second one would be noise.
                UxAction::Noop
            }
        }
        UxEvent::AgentReplied {
            agent,
            binding,
            text,
        } => {
            // No degradation: the reply text *is* the primitive, and
            // every capability combination renders it as a send. This
            // arm exists explicitly so the outer match is exhaustive
            // and reviewers can see the "never degraded" decision.
            //
            // `instance` comes from `agent` (the fleet instance key),
            // NOT from `binding.display_tag()` — the latter is a
            // human-readable label and will miss the `config.instances`
            // lookup in `try_telegram_reply`.
            UxAction::SendText {
                instance: agent.clone(),
                binding: binding.clone(),
                text: text.clone(),
            }
        }
        UxEvent::Fleet(_) => {
            // Fleet events do NOT participate in the Q1 cap-degradation
            // ladder. They target a configured `fleet_binding`, not the
            // originating user's thread, and their rendering is a plain
            // one-liner format (no react / edit option). Adapters that
            // want to consume Fleet events dispatch on the `UxEvent`
            // variant *before* calling this function (see
            // `docs/DESIGN-stage-b-ux.md` §4.4 dispatch-split). Adapters
            // that ignore Fleet events — or haven't wired a renderer
            // yet (e.g. PR-A Telegram, where the renderer lands in
            // PR-B) — correctly no-op via this arm.
            UxAction::Noop
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{binding::BindingRef, caps::ChannelCapabilities};

    fn binding(tag: &str) -> BindingRef {
        BindingRef::new(
            "test",
            Some(tag.to_string()),
            // Payload shape is irrelevant for select_action.
            (),
        )
    }

    fn msg(tag: &str, id: &str) -> MsgRef {
        MsgRef {
            binding: binding(tag),
            id: id.to_string(),
        }
    }

    /// Caps combos we exercise. Matches the Q1 table columns.
    fn caps_react_only() -> ChannelCapabilities {
        ChannelCapabilities {
            react: true,
            edit: false,
            ..Default::default()
        }
    }
    fn caps_edit_only() -> ChannelCapabilities {
        ChannelCapabilities {
            react: false,
            edit: true,
            ..Default::default()
        }
    }
    fn caps_react_and_edit() -> ChannelCapabilities {
        // react wins the ladder.
        ChannelCapabilities {
            react: true,
            edit: true,
            ..Default::default()
        }
    }
    fn caps_neither() -> ChannelCapabilities {
        ChannelCapabilities::default() // both false.
    }

    // Helper: assert React action with expected instance + message-id + emoji.
    fn assert_react(
        action: &UxAction,
        expected_instance: &str,
        expected_msg_id: &str,
        expected_emoji: &str,
    ) {
        match action {
            UxAction::React {
                instance,
                msg,
                emoji,
            } => {
                assert_eq!(instance, expected_instance, "instance");
                assert_eq!(msg.id, expected_msg_id, "msg id");
                assert_eq!(*emoji, expected_emoji, "emoji");
            }
            other => panic!("expected React, got {other:?}"),
        }
    }

    // Helper: assert EditText action with expected instance + message-id + body.
    fn assert_edit(
        action: &UxAction,
        expected_instance: &str,
        expected_msg_id: &str,
        expected_text: &str,
    ) {
        match action {
            UxAction::EditText {
                instance,
                msg,
                text,
            } => {
                assert_eq!(instance, expected_instance, "instance");
                assert_eq!(msg.id, expected_msg_id, "msg id");
                assert_eq!(text, expected_text, "edit text");
            }
            other => panic!("expected EditText, got {other:?}"),
        }
    }

    // Helper: assert SendText action with expected instance + binding-tag + text.
    fn assert_send(
        action: &UxAction,
        expected_instance: &str,
        expected_tag: &str,
        expected_text: &str,
    ) {
        match action {
            UxAction::SendText {
                instance,
                binding,
                text,
            } => {
                assert_eq!(instance, expected_instance, "instance");
                assert_eq!(binding.display_tag(), Some(expected_tag), "binding tag");
                assert_eq!(text, expected_text, "send text");
            }
            other => panic!("expected SendText, got {other:?}"),
        }
    }

    // ─── UserMsgReceived row ─────────────────────────────────────────

    #[test]
    fn user_msg_received_react_emits_eyes_reaction() {
        let ev = UxEvent::UserMsgReceived {
            origin_msg: msg("tg#1", "42"),
            agent: "agent-a".into(),
        };
        assert_react(
            &select_action(&ev, &caps_react_only()),
            "agent-a",
            "42",
            "👀",
        );
    }

    #[test]
    fn user_msg_received_edit_only_annotates_origin() {
        let ev = UxEvent::UserMsgReceived {
            origin_msg: msg("tg#1", "42"),
            agent: "agent-a".into(),
        };
        assert_edit(
            &select_action(&ev, &caps_edit_only()),
            "agent-a",
            "42",
            "[delivered]",
        );
    }

    #[test]
    fn user_msg_received_neither_cap_sends_short_ack() {
        let ev = UxEvent::UserMsgReceived {
            origin_msg: msg("tg#1", "42"),
            agent: "agent-a".into(),
        };
        assert_send(
            &select_action(&ev, &caps_neither()),
            "agent-a",
            "tg#1",
            "✓ delivered",
        );
    }

    #[test]
    fn user_msg_received_react_beats_edit_when_both() {
        let ev = UxEvent::UserMsgReceived {
            origin_msg: msg("tg#1", "42"),
            agent: "agent-a".into(),
        };
        assert!(matches!(
            select_action(&ev, &caps_react_and_edit()),
            UxAction::React { .. }
        ));
    }

    // ─── AgentPickedUp row ───────────────────────────────────────────

    #[test]
    fn agent_picked_up_react_emits_check_reaction() {
        let ev = UxEvent::AgentPickedUp {
            origin_msg: msg("tg#1", "42"),
            agent: "agent-a".into(),
        };
        assert_react(
            &select_action(&ev, &caps_react_only()),
            "agent-a",
            "42",
            "✅",
        );
    }

    #[test]
    fn agent_picked_up_edit_only_annotates_origin() {
        let ev = UxEvent::AgentPickedUp {
            origin_msg: msg("tg#1", "42"),
            agent: "agent-a".into(),
        };
        assert_edit(
            &select_action(&ev, &caps_edit_only()),
            "agent-a",
            "42",
            "[read]",
        );
    }

    #[test]
    fn agent_picked_up_neither_cap_is_noop() {
        let ev = UxEvent::AgentPickedUp {
            origin_msg: msg("tg#1", "42"),
            agent: "agent-a".into(),
        };
        assert!(matches!(
            select_action(&ev, &caps_neither()),
            UxAction::Noop
        ));
    }

    #[test]
    fn agent_picked_up_react_beats_edit_when_both() {
        let ev = UxEvent::AgentPickedUp {
            origin_msg: msg("tg#1", "42"),
            agent: "agent-a".into(),
        };
        assert!(matches!(
            select_action(&ev, &caps_react_and_edit()),
            UxAction::React { .. }
        ));
    }

    // ─── AgentReplied row — never degraded ───────────────────────────

    #[test]
    fn agent_replied_always_sends_regardless_of_caps() {
        let ev = UxEvent::AgentReplied {
            agent: "agent-a".into(),
            binding: binding("tg#1"),
            text: "hello".into(),
        };
        // Every cap combo must return the same SendText shape.
        for caps in [
            caps_neither(),
            caps_react_only(),
            caps_edit_only(),
            caps_react_and_edit(),
        ] {
            assert_send(&select_action(&ev, &caps), "agent-a", "tg#1", "hello");
        }
    }

    /// Regression for the PR #49 review finding: `UxAction::SendText`
    /// was sourcing `instance` from `binding.display_tag()`, which is
    /// a human-readable label like "TG#42" and will fail the
    /// `config.instances.get(instance_name)` lookup in
    /// `try_telegram_reply`. This test pins that `instance` MUST come
    /// from the event's `agent` field regardless of what the binding
    /// renders as.
    #[test]
    fn agent_replied_instance_comes_from_agent_not_display_tag() {
        let ev = UxEvent::AgentReplied {
            agent: "instance-a".into(),
            // display_tag deliberately does NOT match `agent` — a
            // legacy render string that would fail a fleet lookup.
            binding: binding("TG#42"),
            text: "hi".into(),
        };
        match select_action(&ev, &ChannelCapabilities::default()) {
            UxAction::SendText {
                instance, binding, ..
            } => {
                assert_eq!(instance, "instance-a", "instance must be the agent name");
                assert_ne!(
                    instance,
                    binding.display_tag().unwrap_or_default(),
                    "instance must NOT equal display_tag — that was the bug"
                );
            }
            other => panic!("expected SendText, got {other:?}"),
        }
    }

    /// Same pin for the degradation paths: when UserMsgReceived falls
    /// through to the SendText ack column, `instance` must still be
    /// the agent name, not a display-tag scrape.
    #[test]
    fn user_msg_received_send_ack_instance_comes_from_agent() {
        let ev = UxEvent::UserMsgReceived {
            origin_msg: msg("TG#42", "100"),
            agent: "instance-a".into(),
        };
        match select_action(&ev, &caps_neither()) {
            UxAction::SendText { instance, .. } => {
                assert_eq!(instance, "instance-a");
            }
            other => panic!("expected SendText, got {other:?}"),
        }
    }

    /// And for react/edit — even though today's `try_telegram_react`
    /// uses `instance_name` only as a metadata fallback when
    /// `message_id` is None (we always pass Some), the field should
    /// still carry the agent name so future call sites that DO rely
    /// on the lookup don't regress.
    #[test]
    fn react_and_edit_instance_comes_from_agent() {
        let ev = UxEvent::UserMsgReceived {
            origin_msg: msg("TG#42", "100"),
            agent: "instance-a".into(),
        };
        match select_action(&ev, &caps_react_only()) {
            UxAction::React { instance, .. } => assert_eq!(instance, "instance-a"),
            other => panic!("expected React, got {other:?}"),
        }
        match select_action(&ev, &caps_edit_only()) {
            UxAction::EditText { instance, .. } => assert_eq!(instance, "instance-a"),
            other => panic!("expected EditText, got {other:?}"),
        }
    }

    // ─── Sink trait sanity ───────────────────────────────────────────

    #[test]
    fn noop_sink_does_not_panic_on_any_variant() {
        let sink = NoopUxSink;
        sink.emit(&UxEvent::UserMsgReceived {
            origin_msg: msg("tg#1", "1"),
            agent: "a".into(),
        });
        sink.emit(&UxEvent::AgentPickedUp {
            origin_msg: msg("tg#1", "1"),
            agent: "a".into(),
        });
        sink.emit(&UxEvent::AgentReplied {
            agent: "a".into(),
            binding: binding("tg#1"),
            text: "x".into(),
        });
        sink.emit(&UxEvent::Fleet(FleetEvent::DelegateTask {
            from: "a".into(),
            to: "b".into(),
            summary: "s".into(),
            task_id: None,
        }));
    }

    // ─── FleetEvent ──────────────────────────────────────────────────

    /// Fleet variants construct and carry data correctly. This is a
    /// smoke test — the producer wiring that fills these fields lives
    /// in `src/mcp/handlers.rs` and is tested there against a
    /// `RecordingSink`.
    #[test]
    fn fleet_event_variants_construct() {
        let delegate = FleetEvent::DelegateTask {
            from: "a".into(),
            to: "b".into(),
            summary: "pick this up".into(),
            task_id: Some("AGD-7".into()),
        };
        match delegate {
            FleetEvent::DelegateTask {
                from,
                to,
                summary,
                task_id,
            } => {
                assert_eq!(from, "a");
                assert_eq!(to, "b");
                assert_eq!(summary, "pick this up");
                assert_eq!(task_id.as_deref(), Some("AGD-7"));
            }
            _ => panic!("wrong variant"),
        }

        // `task_id: None` is the default reflecting the MCP handlers'
        // actual contract — correlation_id is optional on delegate_task.
        let delegate_anon = FleetEvent::DelegateTask {
            from: "a".into(),
            to: "b".into(),
            summary: "anon".into(),
            task_id: None,
        };
        assert!(matches!(
            delegate_anon,
            FleetEvent::DelegateTask { task_id: None, .. }
        ));

        let report = FleetEvent::ReportResult {
            from: "b".into(),
            to: "a".into(),
            summary: "done".into(),
            task_id: Some("AGD-7".into()),
        };
        assert!(matches!(report, FleetEvent::ReportResult { .. }));

        let decision = FleetEvent::PostDecision {
            by: "a".into(),
            title: "use X".into(),
            decision_id: "d-123".into(),
        };
        if let FleetEvent::PostDecision {
            by,
            title,
            decision_id,
        } = decision
        {
            assert_eq!(by, "a");
            assert_eq!(title, "use X");
            assert_eq!(decision_id, "d-123");
        } else {
            panic!();
        }

        let broadcast = FleetEvent::Broadcast {
            from: "a".into(),
            recipients: vec!["b".into(), "c".into()],
            summary: "ship it".into(),
        };
        if let FleetEvent::Broadcast { recipients, .. } = broadcast {
            assert_eq!(recipients.len(), 2);
        } else {
            panic!();
        }
    }

    /// Pin Fleet events out of the Q1 cap-degradation ladder:
    /// [`select_action`] must return [`UxAction::Noop`] for any Fleet
    /// payload, regardless of caps. Dispatch-split callers (see
    /// `DESIGN-stage-b-ux.md` §4.4) steer Fleet events to a separate
    /// renderer before reaching this function; this arm guarantees
    /// naive callers that forget to dispatch-split don't accidentally
    /// render a Fleet event into the origin user's thread as a react /
    /// edit / send.
    #[test]
    fn select_action_fleet_is_always_noop_regardless_of_caps() {
        let fleet = UxEvent::Fleet(FleetEvent::DelegateTask {
            from: "a".into(),
            to: "b".into(),
            summary: "x".into(),
            task_id: None,
        });
        for caps in [
            caps_neither(),
            caps_react_only(),
            caps_edit_only(),
            caps_react_and_edit(),
        ] {
            assert!(matches!(select_action(&fleet, &caps), UxAction::Noop));
        }
    }

    // ─── format_fleet_oneliner — snapshot / shape pins ───────────────
    //
    // These are structural pins on the §5.1 exemplars. They do not use
    // an external snapshot tool (e.g. insta) — the substrings are small
    // enough to inline-assert, which keeps the test file self-contained
    // and diff-readable.

    #[test]
    fn format_fleet_oneliner_delegate_with_task_id() {
        let fe = FleetEvent::DelegateTask {
            from: "at-dev-1".into(),
            to: "at-dev-2".into(),
            summary: "task #9 Option C scoping".into(),
            task_id: Some("AGD-7".into()),
        };
        let out = format_fleet_oneliner(&fe, 4096);
        assert_eq!(
            out,
            "[at-dev-1 → at-dev-2]  DELEGATE  task #9 Option C scoping (#AGD-7)"
        );
    }

    #[test]
    fn format_fleet_oneliner_delegate_without_task_id_omits_suffix() {
        let fe = FleetEvent::DelegateTask {
            from: "a".into(),
            to: "b".into(),
            summary: "pick up".into(),
            task_id: None,
        };
        let out = format_fleet_oneliner(&fe, 4096);
        assert_eq!(out, "[a → b]  DELEGATE  pick up");
        assert!(!out.contains("(#"), "task_id=None must not emit (#…)");
    }

    #[test]
    fn format_fleet_oneliner_report_with_task_id() {
        let fe = FleetEvent::ReportResult {
            from: "at-dev-2".into(),
            to: "at-dev-1".into(),
            summary: "DONE  src/utils.rs consolidation landed".into(),
            task_id: Some("21".into()),
        };
        let out = format_fleet_oneliner(&fe, 4096);
        assert_eq!(
            out,
            "[at-dev-2 → at-dev-1]  REPORT  DONE  src/utils.rs consolidation landed (#21)"
        );
    }

    #[test]
    fn format_fleet_oneliner_decision_appends_d_id() {
        let fe = FleetEvent::PostDecision {
            by: "at-dev-1".into(),
            title: "task-board-ownership rules".into(),
            decision_id: "42".into(),
        };
        let out = format_fleet_oneliner(&fe, 4096);
        assert_eq!(
            out,
            "[at-dev-1 solo]  DECISION  task-board-ownership rules (D-42)"
        );
    }

    #[test]
    fn format_fleet_oneliner_broadcast_empty_recipients_uses_star() {
        let fe = FleetEvent::Broadcast {
            from: "at-dev-3".into(),
            recipients: vec![],
            summary: "CI green post-rebase".into(),
        };
        let out = format_fleet_oneliner(&fe, 4096);
        assert_eq!(out, "[at-dev-3 → *]  BROADCAST  CI green post-rebase");
    }

    #[test]
    fn format_fleet_oneliner_broadcast_one_recipient() {
        let fe = FleetEvent::Broadcast {
            from: "a".into(),
            recipients: vec!["b".into()],
            summary: "hi".into(),
        };
        let out = format_fleet_oneliner(&fe, 4096);
        assert_eq!(out, "[a → b]  BROADCAST  hi");
    }

    #[test]
    fn format_fleet_oneliner_broadcast_three_named() {
        let fe = FleetEvent::Broadcast {
            from: "a".into(),
            recipients: vec!["b".into(), "c".into(), "d".into()],
            summary: "s".into(),
        };
        let out = format_fleet_oneliner(&fe, 4096);
        assert_eq!(out, "[a → b,c,d]  BROADCAST  s");
    }

    #[test]
    fn format_fleet_oneliner_broadcast_five_elides_with_plus_n() {
        let fe = FleetEvent::Broadcast {
            from: "a".into(),
            recipients: vec!["b".into(), "c".into(), "d".into(), "e".into(), "f".into()],
            summary: "s".into(),
        };
        let out = format_fleet_oneliner(&fe, 4096);
        // First 2 named, then "…+3" for the remaining 3.
        assert_eq!(out, "[a → b,c,…+3]  BROADCAST  s");
    }

    /// Truncation pin: a summary longer than the available budget is
    /// truncated on a char boundary with an ellipsis appended. Fixed
    /// provenance suffix (`(#id)` / `(D-id)`) is preserved — the truncator
    /// only eats into the summary.
    #[test]
    fn format_fleet_oneliner_truncates_long_summary_preserving_suffix() {
        let long_summary = "x".repeat(500);
        let fe = FleetEvent::DelegateTask {
            from: "a".into(),
            to: "b".into(),
            summary: long_summary,
            task_id: Some("AGD-7".into()),
        };
        // Budget 80 bytes → total output must fit, suffix `(#AGD-7)` still present.
        let out = format_fleet_oneliner(&fe, 80);
        assert!(
            out.len() <= 80,
            "expected ≤80 bytes, got {}: {out}",
            out.len()
        );
        assert!(
            out.ends_with(" (#AGD-7)"),
            "provenance suffix must survive truncation: {out}"
        );
        assert!(
            out.contains('…'),
            "truncated body must carry an ellipsis: {out}"
        );
    }

    /// Zero `max_bytes` is treated as "no cap" so callers can skip the
    /// cap when it's not meaningful (e.g. TUI renderer). The internal
    /// `usize::MAX` branch is exercised here so any future refactor
    /// that changes the zero-semantics trips this pin.
    #[test]
    fn format_fleet_oneliner_max_bytes_zero_means_no_cap() {
        let fe = FleetEvent::DelegateTask {
            from: "a".into(),
            to: "b".into(),
            summary: "x".repeat(10_000),
            task_id: None,
        };
        let out = format_fleet_oneliner(&fe, 0);
        // 10k "x"s plus prefix, no ellipsis.
        assert!(!out.contains('…'), "no cap → no truncation");
        assert!(out.len() > 10_000);
    }

    /// Char-boundary pin: truncation walks `char_indices` from the left,
    /// so a multi-byte codepoint that would straddle the byte budget is
    /// dropped rather than split in half. Without this walk the output
    /// would be invalid UTF-8 when the budget lands mid-codepoint.
    #[test]
    fn format_fleet_oneliner_truncates_on_char_boundary() {
        // Summary contains a 3-byte '〇' glyph. Budget forces a boundary
        // inside the glyph if we sliced by bytes.
        let fe = FleetEvent::DelegateTask {
            from: "a".into(),
            to: "b".into(),
            summary: "x〇yz".into(),
            task_id: None,
        };
        // Prefix is "[a → b]  DELEGATE  " — compute a small cap so body
        // budget is only a few bytes, mid-codepoint.
        let prefix_len = "[a → b]  DELEGATE  ".len();
        let out = format_fleet_oneliner(&fe, prefix_len + 5);
        assert!(
            std::str::from_utf8(out.as_bytes()).is_ok(),
            "output must remain valid UTF-8: {out:?}"
        );
    }
}
