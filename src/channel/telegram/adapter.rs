//! Telegram Channel trait adapter (T1d) + UxEventSink impl.

use crate::agent::AgentRegistry;
use crate::channel::telegram::bot_api::*;
use crate::channel::telegram::error::*;
use crate::channel::telegram::notify::*;
use crate::channel::telegram::reply::*;
use crate::channel::telegram::send::*;
use crate::channel::telegram::state::*;
use crate::channel::telegram::topic_registry::*;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::MessageId;

/// Platform payload carried inside a [`BindingRef`] for Telegram.
#[derive(Debug, Clone)]
pub(crate) struct TelegramBindingPayload {
    pub topic_id: i32,
}

impl TelegramBindingPayload {
    pub(super) fn into_binding(self) -> crate::channel::BindingRef {
        let tag = format!("TG#{}", self.topic_id);
        crate::channel::BindingRef::new("telegram", Some(tag), self)
    }
}

/// Construct a `BindingRef` for a `MsgRef` returned from `Channel::send`.
pub(super) fn build_telegram_msg_binding(topic_id: Option<i32>) -> crate::channel::BindingRef {
    match topic_id {
        Some(tid) => TelegramBindingPayload { topic_id: tid }.into_binding(),
        None => crate::channel::BindingRef::new("telegram", None, ()),
    }
}

/// Placeholder `MsgRef` for ops that don't surface a platform message id.
pub(super) fn empty_msg_ref() -> crate::channel::MsgRef {
    crate::channel::MsgRef {
        binding: crate::channel::BindingRef::new("telegram", None, ()),
        id: "0".to_string(),
    }
}

/// Telegram adapter implementing the platform-neutral `Channel` trait.
pub struct TelegramChannel {
    pub(super) state: Arc<Mutex<TelegramState>>,
    pub(super) caps: crate::channel::ChannelCapabilities,
}

impl TelegramChannel {
    pub fn new(state: Arc<Mutex<TelegramState>>) -> Self {
        use crate::channel::{
            ChannelCapabilities, MarkdownDialect, MentionStyle, NativeSeeAllHint,
        };
        let caps = ChannelCapabilities {
            emits_deletion_events: false,
            threads: true,
            buttons: false,
            attachments: true,
            markdown: MarkdownDialect::MarkdownV2,
            max_msg_bytes: 4096,
            rate_budget: crate::channel::RateBudget::default(),
            react: true,
            edit: true,
            typing_indicator: true,
            receives_edit_events: false,
            mention_parsing_hint: MentionStyle::AtUsername,
            bot_sees_read_receipts: false,
            has_native_multi_thread_view: Some(NativeSeeAllHint {
                label: "View as Messages".to_string(),
            }),
            ephemeral: false,
        };
        Self { state, caps }
    }

    pub(crate) fn state(&self) -> &Arc<Mutex<TelegramState>> {
        &self.state
    }

    #[cfg(test)]
    pub(crate) fn with_caps(
        state: Arc<Mutex<TelegramState>>,
        caps: crate::channel::ChannelCapabilities,
    ) -> Self {
        Self { state, caps }
    }

    pub(crate) fn fleet_send_target(&self) -> Option<(ChatId, i32)> {
        let s = lock_state(&self.state);
        s.fleet_binding_topic_id.map(|tid| (s.group_id, tid))
    }

    pub(crate) fn apply_fleet_action(&self, fe: &crate::channel::ux_event::FleetEvent) {
        let Some((chat_id, topic_id)) = self.fleet_send_target() else {
            tracing::debug!(?fe, "fleet renderer: no fleet_binding configured (drop)");
            return;
        };
        let (bot, home) = {
            let s = lock_state(&self.state);
            match s.bot.clone() {
                Some(b) => (b, s.home.clone()),
                None => {
                    tracing::debug!(?fe, "fleet renderer: no bot (contract-test state, drop)");
                    return;
                }
            }
        };
        let text = crate::channel::ux_event::format_fleet_oneliner(fe, self.caps.max_msg_bytes);
        if let Err(e) = telegram_runtime()
            .block_on(async { send_with_topic(&bot, chat_id, Some(topic_id), &text, None).await })
        {
            let handled = handle_fleet_send_failure(&e, &home, &self.state, topic_id);
            if !handled {
                tracing::warn!(%e, topic_id, "fleet renderer: send failed");
            }
        }
    }
}

impl crate::channel::Channel for TelegramChannel {
    fn kind(&self) -> &'static str {
        "telegram"
    }

    fn caps(&self) -> &crate::channel::ChannelCapabilities {
        &self.caps
    }

    fn poll_event(&self) -> Option<crate::channel::ChannelEvent> {
        None
    }

    fn send(
        &self,
        binding: &crate::channel::BindingRef,
        msg: crate::channel::OutMsg,
    ) -> anyhow::Result<crate::channel::MsgRef> {
        let (bot, group_id) = {
            let s = lock_state(&self.state);
            match s.bot.clone() {
                Some(b) => (b, s.group_id),
                None => anyhow::bail!("telegram bot not initialized"),
            }
        };
        let topic_id: Option<i32> = binding
            .downcast::<TelegramBindingPayload>()
            .map(|p| p.topic_id);

        match msg.attachment {
            Some(ref att) => {
                let caption = resolve_caption(&msg.text, att);
                let msg_id = telegram_runtime().block_on(send_media(
                    &bot,
                    group_id,
                    topic_id,
                    att,
                    caption.as_deref(),
                ))?;
                if needs_separate_text(&msg.text, att) {
                    let _ = telegram_runtime()
                        .block_on(send_with_topic(&bot, group_id, topic_id, &msg.text, None));
                }
                Ok(crate::channel::MsgRef {
                    binding: build_telegram_msg_binding(topic_id),
                    id: msg_id.to_string(),
                })
            }
            None => {
                if msg.text.is_empty() {
                    anyhow::bail!("OutMsg has no text and no attachment");
                }
                let msg_id = telegram_runtime().block_on(send_with_topic_capturing_id(
                    &bot, group_id, topic_id, &msg.text, None,
                ))?;
                Ok(crate::channel::MsgRef {
                    binding: build_telegram_msg_binding(topic_id),
                    id: msg_id.to_string(),
                })
            }
        }
    }

    fn edit(
        &self,
        msg: &crate::channel::MsgRef,
        payload: crate::channel::OutMsg,
    ) -> anyhow::Result<()> {
        let (bot, group_id) = {
            let s = lock_state(&self.state);
            match s.bot.clone() {
                Some(b) => (b, s.group_id),
                None => anyhow::bail!("telegram bot not initialized"),
            }
        };
        let mid: i32 = msg
            .id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid telegram message_id: {}", msg.id))?;
        let text = payload.text;
        if text.is_empty() {
            anyhow::bail!("OutMsg.text empty — Telegram editMessageText requires non-empty text");
        }
        telegram_runtime().block_on(async move {
            use teloxide::prelude::Requester;
            bot.edit_message_text(group_id, MessageId(mid), &text)
                .send()
                .await?;
            Ok::<(), anyhow::Error>(())
        })
    }

    fn delete(&self, msg: &crate::channel::MsgRef) -> anyhow::Result<()> {
        let (bot, group_id) = {
            let s = lock_state(&self.state);
            match s.bot.clone() {
                Some(b) => (b, s.group_id),
                None => anyhow::bail!("telegram bot not initialized"),
            }
        };
        let mid: i32 = msg
            .id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid telegram message_id: {}", msg.id))?;
        telegram_runtime().block_on(async move {
            use teloxide::prelude::Requester;
            match bot.delete_message(group_id, MessageId(mid)).send().await {
                Ok(_) => Ok(()),
                Err(e) => {
                    let msg = format!("{e}");
                    if msg.contains("message to delete not found")
                        || msg.contains("message can't be deleted")
                    {
                        tracing::debug!(mid, "delete_message: already deleted — Ok");
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!("{e}"))
                    }
                }
            }
        })
    }

    fn create_binding(
        &self,
        name: &str,
        _opts: crate::channel::BindingOpts,
    ) -> anyhow::Result<crate::channel::BindingRef> {
        let home = lock_state(&self.state).home.clone();
        match create_topic_for_instance(&home, name) {
            Some(tid) => Ok(TelegramBindingPayload { topic_id: tid }.into_binding()),
            None => anyhow::bail!("create_topic_for_instance returned None for {name}"),
        }
    }

    fn remove_binding(&self, binding: &crate::channel::BindingRef) -> anyhow::Result<()> {
        let payload = binding
            .downcast::<TelegramBindingPayload>()
            .ok_or_else(|| anyhow::anyhow!("non-telegram binding passed to remove_binding"))?;
        let home = lock_state(&self.state).home.clone();
        delete_topic(&home, payload.topic_id);
        Ok(())
    }

    fn has_binding(&self, instance: &str) -> bool {
        lock_state(&self.state)
            .instance_to_topic
            .contains_key(instance)
    }

    fn record_binding(
        &self,
        instance: &str,
        binding: crate::channel::BindingRef,
        submit_key: String,
    ) {
        let Some(payload) = binding.downcast::<TelegramBindingPayload>() else {
            tracing::warn!(
                kind = binding.kind(),
                instance,
                "record_binding received non-telegram binding — dropping"
            );
            return;
        };
        let tid = payload.topic_id;
        let mut s = lock_state(&self.state);
        s.instance_to_topic.insert(instance.to_string(), tid);
        s.topic_to_instance.insert(tid, instance.to_string());
        s.submit_keys.insert(instance.to_string(), submit_key);
    }

    fn take_binding(&self, instance: &str) -> Option<crate::channel::BindingRef> {
        let mut s = lock_state(&self.state);
        let tid = s.instance_to_topic.remove(instance)?;
        s.topic_to_instance.remove(&tid);
        s.submit_keys.remove(instance);
        drop(s);
        Some(TelegramBindingPayload { topic_id: tid }.into_binding())
    }

    fn attach_registry(&self, registry: AgentRegistry) {
        let mut s = lock_state(&self.state);
        s.registry = Some(registry);
    }

    fn create_topic(
        &self,
        name: &str,
    ) -> std::result::Result<crate::channel::TopicRef, crate::channel::ChannelError> {
        let home = lock_state(&self.state).home.clone();
        match create_topic_for_instance(&home, name) {
            Some(tid) => Ok(crate::channel::TopicRef {
                id: tid.to_string(),
                channel_kind: crate::channel::ChannelKind::Telegram,
            }),
            None => Err(crate::channel::ChannelError::Other(anyhow::anyhow!(
                "failed to create topic for {name}"
            ))),
        }
    }

    fn notify(
        &self,
        instance: &str,
        _severity: crate::channel::NotifySeverity,
        message: &str,
        silent: bool,
    ) -> std::result::Result<(), crate::channel::ChannelError> {
        let home = lock_state(&self.state).home.clone();
        if silent {
            notify_telegram_silent(&home, instance, message);
        } else {
            notify_telegram(&home, instance, message);
        }
        Ok(())
    }

    fn outbound_authorized(&self) -> bool {
        crate::channel::auth::is_outbound_authorized(&lock_state(&self.state).user_allowlist)
    }

    fn send_from_agent(
        &self,
        agent: &str,
        op: crate::channel::AgentOutboundOp,
    ) -> std::result::Result<crate::channel::MsgRef, crate::channel::ChannelError> {
        use crate::channel::ChannelError;

        if !self.outbound_authorized() {
            return Err(ChannelError::Other(anyhow::anyhow!(
                "outbound disabled — channel.user_allowlist not configured \
                 (see docs/USAGE.md \"Channel: Telegram\" migration section)"
            )));
        }

        let home = lock_state(&self.state).home.clone();
        match op {
            crate::channel::AgentOutboundOp::Reply { text } => {
                let (msg_id, _chat_id) =
                    try_telegram_reply_from(&home, agent, &text).map_err(ChannelError::Other)?;
                Ok(crate::channel::MsgRef {
                    binding: crate::channel::BindingRef::new(
                        "telegram",
                        Some(format!("TG#{agent}")),
                        TelegramBindingPayload { topic_id: msg_id },
                    ),
                    id: msg_id.to_string(),
                })
            }
            crate::channel::AgentOutboundOp::React { emoji, message_id } => {
                try_telegram_react(&home, agent, &emoji, message_id.as_deref())
                    .map_err(ChannelError::Other)?;
                Ok(empty_msg_ref())
            }
            crate::channel::AgentOutboundOp::Edit {
                message_id,
                new_text,
            } => {
                try_telegram_edit(&home, agent, &message_id, &new_text)
                    .map_err(ChannelError::Other)?;
                Ok(crate::channel::MsgRef {
                    binding: crate::channel::BindingRef::new("telegram", None, ()),
                    id: message_id,
                })
            }
            crate::channel::AgentOutboundOp::InjectProvenance { from, task } => {
                inject_provenance(agent, &from, &task).map_err(ChannelError::Other)?;
                Ok(empty_msg_ref())
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::channel::telegram::topic_registry::*;

    fn tmp_home(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static CTR: AtomicU32 = AtomicU32::new(0);
        let id = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "agend-tg-adapter-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&d).ok();
        d
    }

    fn contract_state(home: PathBuf) -> Arc<Mutex<TelegramState>> {
        Arc::new(Mutex::new(TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            home,
            HashMap::new(),
            None,
        )))
    }

    #[test]
    fn telegram_channel_create_topic_returns_error_without_config() {
        use crate::channel::Channel;
        let home = tmp_home("create_topic_no_config");
        let channel = TelegramChannel::new(contract_state(home.clone()));
        assert!(channel.create_topic("test-agent").is_err());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn telegram_channel_notify_succeeds_without_config() {
        use crate::channel::{Channel, NotifySeverity};
        let home = tmp_home("notify_no_config");
        let channel = TelegramChannel::new(contract_state(home.clone()));
        assert!(channel
            .notify("test-agent", NotifySeverity::Warn, "stall", false)
            .is_ok());
        assert!(channel
            .notify("test-agent", NotifySeverity::Info, "recovered", true)
            .is_ok());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn telegram_channel_trait_methods_are_object_safe() {
        let channel = TelegramChannel::new(contract_state(PathBuf::from("/tmp")));
        let dyn_channel: &dyn crate::channel::Channel = &channel;
        let _ = dyn_channel.create_topic("test");
        let _ = dyn_channel.notify("test", crate::channel::NotifySeverity::Warn, "msg", false);
    }

    #[test]
    fn send_returns_telegram_binding_with_topic_when_caller_supplied_topic() {
        let supplied = TelegramBindingPayload { topic_id: 42 }.into_binding();
        let topic = supplied
            .downcast::<TelegramBindingPayload>()
            .map(|p| p.topic_id);
        assert_eq!(topic, Some(42));
        let returned = build_telegram_msg_binding(topic);
        assert_eq!(returned.kind(), "telegram");
        assert_eq!(
            returned
                .downcast::<TelegramBindingPayload>()
                .map(|p| p.topic_id),
            Some(42),
        );
    }

    #[test]
    fn send_returns_telegram_binding_without_topic_for_foreign_binding() {
        let returned = build_telegram_msg_binding(None);
        assert_eq!(returned.kind(), "telegram");
        assert!(returned.downcast::<TelegramBindingPayload>().is_none());
    }

    #[test]
    fn channel_send_returns_err_when_bot_uninitialised() {
        use crate::channel::Channel;
        let channel = TelegramChannel::new(contract_state(PathBuf::from("/tmp/agend-phase3-test")));
        let binding = TelegramBindingPayload { topic_id: 7 }.into_binding();
        let err = channel
            .send(&binding, crate::channel::OutMsg::text("hello"))
            .expect_err("must Err");
        assert!(err.to_string().contains("bot not initialized"));
    }

    #[test]
    fn channel_edit_returns_err_when_bot_uninitialised() {
        use crate::channel::Channel;
        let channel = TelegramChannel::new(contract_state(PathBuf::from("/tmp/agend-phase3-test")));
        let msg_ref = crate::channel::MsgRef {
            binding: TelegramBindingPayload { topic_id: 7 }.into_binding(),
            id: "123".to_string(),
        };
        let err = channel
            .edit(&msg_ref, crate::channel::OutMsg::text("new"))
            .expect_err("must Err");
        assert!(err.to_string().contains("bot not initialized"));
    }

    #[test]
    fn channel_edit_rejects_invalid_message_id() {
        use crate::channel::Channel;
        let channel = TelegramChannel::new(contract_state(PathBuf::from("/tmp/agend-phase3-test")));
        let msg_ref = crate::channel::MsgRef {
            binding: TelegramBindingPayload { topic_id: 7 }.into_binding(),
            id: "not-a-number".to_string(),
        };
        let err = channel
            .edit(&msg_ref, crate::channel::OutMsg::text("x"))
            .expect_err("must Err");
        assert!(
            err.to_string().contains("bot not initialized")
                || err.to_string().contains("invalid telegram message_id")
        );
    }

    #[test]
    fn channel_delete_returns_err_when_bot_uninitialised() {
        use crate::channel::Channel;
        let channel = TelegramChannel::new(contract_state(PathBuf::from("/tmp/agend-phase3-test")));
        let msg_ref = crate::channel::MsgRef {
            binding: TelegramBindingPayload { topic_id: 7 }.into_binding(),
            id: "456".to_string(),
        };
        let err = channel.delete(&msg_ref).expect_err("must Err");
        assert!(err.to_string().contains("bot not initialized"));
    }

    #[test]
    fn send_from_agent_rejects_when_user_allowlist_unconfigured() {
        use crate::channel::Channel;
        let channel =
            TelegramChannel::new(contract_state(PathBuf::from("/tmp/agend-phase5b-test")));
        let ops: Vec<(&str, crate::channel::AgentOutboundOp)> = vec![
            (
                "reply",
                crate::channel::AgentOutboundOp::Reply {
                    text: "leak".to_string(),
                },
            ),
            (
                "react",
                crate::channel::AgentOutboundOp::React {
                    emoji: "👀".to_string(),
                    message_id: None,
                },
            ),
            (
                "edit",
                crate::channel::AgentOutboundOp::Edit {
                    message_id: "1".to_string(),
                    new_text: "x".to_string(),
                },
            ),
            (
                "inject_provenance",
                crate::channel::AgentOutboundOp::InjectProvenance {
                    from: "a".to_string(),
                    task: "t".to_string(),
                },
            ),
        ];
        for (label, op) in ops {
            let result = channel.send_from_agent("agent1", op);
            assert!(result.is_err(), "outbound gate must reject {label}");
            let err_str = format!("{}", result.expect_err("must reject"));
            assert!(
                err_str.contains("user_allowlist not configured"),
                "{label}: {err_str}"
            );
        }
    }
}
