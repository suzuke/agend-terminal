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
    std::thread::Builder::new()
        .name("tg_notify".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            if let Err(_e) = rt.block_on(async {
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
                tracing::warn!(error = %_e, "telegram notify failed");
            }
        })
        .ok();
}
