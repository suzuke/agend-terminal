//! UxEventSink implementation for TelegramChannel.

use super::adapter::TelegramChannel;
use super::bot_api::{try_telegram_edit, try_telegram_react};
use super::reply::try_telegram_reply;
use super::state::lock_state;

impl crate::channel::ux_event::UxEventSink for TelegramChannel {
    fn emit(&self, event: &crate::channel::ux_event::UxEvent) {
        use crate::channel::ux_event::{select_action, UxAction, UxEvent};
        if let UxEvent::Fleet(fe) = event {
            self.apply_fleet_action(fe);
            return;
        }
        let home = lock_state(&self.state).home.clone();
        let action = select_action(event, &self.caps);
        match action {
            UxAction::React {
                instance,
                msg,
                emoji,
            } => {
                if let Err(e) = try_telegram_react(&home, &instance, emoji, Some(&msg.id)) {
                    tracing::warn!(%e, %instance, msg_id = %msg.id, emoji, "UxEventSink: react failed");
                }
            }
            UxAction::EditText {
                instance,
                msg,
                text,
            } => {
                if let Err(e) = try_telegram_edit(&home, &instance, &msg.id, &text) {
                    tracing::warn!(%e, %instance, msg_id = %msg.id, "UxEventSink: edit failed");
                }
            }
            UxAction::SendText {
                instance,
                binding: _,
                text,
            } => {
                if let Err(e) = try_telegram_reply(&instance, &text) {
                    tracing::warn!(%e, %instance, "UxEventSink: send failed");
                }
            }
            UxAction::Noop => {
                tracing::debug!(?event, "UxEventSink: Noop (caps do not support)");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::channel::telegram::state::TelegramState;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use teloxide::types::ChatId;

    #[test]
    fn telegram_channel_emit_noop_when_caps_reject() {
        use crate::channel::{
            ux_event::{UxEvent, UxEventSink},
            BindingRef, ChannelCapabilities, MsgRef,
        };
        let caps = ChannelCapabilities {
            react: false,
            edit: false,
            ..Default::default()
        };
        let state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        let channel = TelegramChannel::with_caps(Arc::new(Mutex::new(state)), caps);
        let origin = MsgRef {
            binding: BindingRef::new("telegram", Some("test-agent".into()), ()),
            id: "1".into(),
        };
        let ev = UxEvent::AgentPickedUp {
            origin_msg: origin,
            agent: "test-agent".into(),
        };
        (&channel as &dyn UxEventSink).emit(&ev);
    }

    #[test]
    fn fleet_send_target_reads_fleet_binding_topic_id_not_general() {
        let mut topic_map = HashMap::new();
        topic_map.insert("general".to_string(), 1);
        topic_map.insert("at-dev-1".to_string(), 100);
        let mut state = TelegramState::new(
            "tok",
            -12345,
            topic_map,
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        state.fleet_binding_topic_id = Some(42);
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));
        let (chat_id, topic_id) = channel.fleet_send_target().expect("Some(target)");
        assert_eq!(topic_id, 42);
        assert_ne!(topic_id, 1);
        assert_ne!(topic_id, 100);
        assert_eq!(chat_id, ChatId(-12345));
    }

    #[test]
    fn fleet_send_target_is_none_when_binding_unresolved() {
        let state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));
        assert!(channel.fleet_send_target().is_none());
    }

    #[test]
    fn emit_fleet_event_does_not_panic_without_binding_or_bot() {
        use crate::channel::{
            ux_event::{FleetEvent, UxEvent, UxEventSink},
            ChannelCapabilities,
        };
        let state = TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        let channel =
            TelegramChannel::with_caps(Arc::new(Mutex::new(state)), ChannelCapabilities::default());
        let fleet_ev = UxEvent::Fleet(FleetEvent::DelegateTask {
            from: "a".into(),
            to: "b".into(),
            summary: "s".into(),
            task_id: None,
        });
        (&channel as &dyn UxEventSink).emit(&fleet_ev);
    }

    #[test]
    fn telegram_channel_caps_are_populated() {
        use crate::channel::{Channel, MarkdownDialect, MentionStyle};
        let state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        let caps = TelegramChannel::new(Arc::new(Mutex::new(state)))
            .caps()
            .clone();
        assert!(!caps.emits_deletion_events);
        assert!(caps.threads);
        assert!(!caps.buttons);
        assert!(caps.attachments);
        assert_eq!(caps.markdown, MarkdownDialect::MarkdownV2);
        assert_eq!(caps.max_msg_bytes, 4096);
        assert!(caps.react);
        assert!(caps.edit);
        assert!(caps.typing_indicator);
        assert!(!caps.receives_edit_events);
        assert_eq!(caps.mention_parsing_hint, MentionStyle::AtUsername);
        assert!(!caps.bot_sees_read_receipts);
        assert_eq!(
            caps.has_native_multi_thread_view.as_ref().unwrap().label,
            "View as Messages"
        );
        assert!(!caps.ephemeral);
    }
}
