use std::path::Path;
use teloxide::payloads::SendMessageSetters;
use teloxide::prelude::Requester;

/// Send a notification to Telegram (instance topic or general).
pub fn notify_telegram(home: &Path, instance_name: &str, text: &str) {
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
        None => return,
    };

    let text = text.to_string();
    let home_owned = home.to_path_buf();
    let instance_owned = instance_name.to_string();
    std::thread::Builder::new()
        .name("tg_notify".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            if let Err(e) = rt.block_on(async {
                let bot = teloxide::Bot::new(&token);
                let chat_id = teloxide::types::ChatId(group_id);
                match topic_id {
                    Some(tid) if tid != 1 => {
                        bot.send_message(chat_id, &text)
                            .message_thread_id(teloxide::types::ThreadId(
                                teloxide::types::MessageId(tid),
                            ))
                            .await?;
                    }
                    _ => {
                        bot.send_message(chat_id, &text).await?;
                    }
                }
                Ok::<(), anyhow::Error>(())
            }) {
                if crate::telegram::is_topic_deleted_error(&e) {
                    if let Some(tid) = topic_id {
                        tracing::info!(
                            instance = %instance_owned,
                            topic_id = tid,
                            "notify_telegram hit topic_deleted — cleaning up"
                        );
                        crate::telegram::cleanup_deleted_topic(
                            &home_owned,
                            &instance_owned,
                            tid,
                            None,
                        );
                    }
                }
                tracing::warn!(error = %e, "telegram notify failed");
            }
        })
        .ok();
}
