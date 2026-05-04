//! Telegram notification helpers — daemon/supervisor notify to instance topics.

use crate::channel::telegram::error::*;
use crate::channel::telegram::send::*;
use crate::channel::telegram::state::*;

/// Send a notification to Telegram (instance topic or general).
pub fn notify_telegram(home: &std::path::Path, instance_name: &str, text: &str) {
    notify_telegram_inner(home, instance_name, text, false);
}

/// Send a notification with Telegram's `disable_notification` flag set — the
/// message still appears in the topic but does not push/vibrate the operator.
pub fn notify_telegram_silent(home: &std::path::Path, instance_name: &str, text: &str) {
    notify_telegram_inner(home, instance_name, text, true);
}

fn notify_telegram_inner(
    home: &std::path::Path,
    instance_name: &str,
    text: &str,
    disable_notification: bool,
) {
    let config = match crate::fleet::FleetConfig::load(&home.join("fleet.yaml")) {
        Ok(c) => c,
        Err(_) => return,
    };
    let (token, group_id, topic_id) = match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram {
            bot_token_env,
            group_id,
            ..
        }) => match std::env::var(bot_token_env) {
            Ok(t) => (
                t,
                *group_id,
                config.instances.get(instance_name).and_then(|i| i.topic_id),
            ),
            Err(_) => return,
        },
        Some(crate::fleet::ChannelConfig::Discord { .. }) => return,
        None => return,
    };

    let text = text.to_string();
    let home_owned = home.to_path_buf();
    let instance_owned = instance_name.to_string();
    // fire-and-forget: losing one notification on shutdown is acceptable.
    telegram_runtime().spawn(async move {
        use teloxide::payloads::SendMessageSetters;
        use teloxide::prelude::Requester;
        let bot = teloxide::Bot::new(&token);
        let chat_id = teloxide::types::ChatId(group_id);
        let result = match topic_id {
            Some(tid) if tid != 1 => {
                let mut req = bot
                    .send_message(chat_id, &text)
                    .message_thread_id(teloxide::types::ThreadId(teloxide::types::MessageId(tid)));
                if disable_notification {
                    req = req.disable_notification(true);
                }
                req.await.map(|_| ())
            }
            _ => {
                let mut req = bot.send_message(chat_id, &text);
                if disable_notification {
                    req = req.disable_notification(true);
                }
                req.await.map(|_| ())
            }
        };
        if let Err(e) = result {
            let e: anyhow::Error = e.into();
            if let Some(stale_tid) = topic_id {
                if is_topic_deleted_error(&e) {
                    if let Some(new_tid) =
                        invalidate_and_recreate_topic(&home_owned, &instance_owned, stale_tid)
                    {
                        tracing::info!(
                            instance = %instance_owned,
                            old_topic = stale_tid,
                            new_topic = new_tid,
                            "notify: retrying with recreated topic"
                        );
                        let mut req = bot.send_message(chat_id, &text).message_thread_id(
                            teloxide::types::ThreadId(teloxide::types::MessageId(new_tid)),
                        );
                        if disable_notification {
                            req = req.disable_notification(true);
                        }
                        let _ = req.await;
                        return;
                    }
                }
            }
            tracing::warn!(error = %e, "telegram notify failed");
        }
    });
}
