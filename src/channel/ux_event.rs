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
//! ## Scope of this file (T3 narrow scope)
//!
//! Only the Q1 delivery-confirmation subset is defined here:
//! [`UxEvent::UserMsgReceived`], [`UxEvent::AgentPickedUp`], and
//! [`UxEvent::AgentReplied`]. `AgentThinking` / `AgentIdle` /
//! `AgentRateLimited` / `AgentCrashed` / `AgentRestarted` and the
//! `Fleet(FleetEvent)` variant are deferred to a follow-up PR — they do
//! not touch the Telegram send / edit / react paths in Q1, so landing
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
    }
}
