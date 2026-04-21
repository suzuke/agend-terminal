use std::path::Path;
use teloxide::payloads::SendMessageSetters;
use teloxide::prelude::Requester;

/// Send a notification to Telegram (instance topic or general).
pub fn notify_telegram(home: &Path, instance_name: &str, text: &str) {
    notify_telegram_inner(home, instance_name, text, false);
}

/// Send a notification with Telegram's `disable_notification` flag set — the
/// message still appears in the topic but does not push/vibrate the operator.
/// Use for state-recovery pings that should not compete with real alerts.
pub fn notify_telegram_silent(home: &Path, instance_name: &str, text: &str) {
    notify_telegram_inner(home, instance_name, text, true);
}

fn notify_telegram_inner(home: &Path, instance_name: &str, text: &str, disable_notification: bool) {
    let config = match crate::fleet::FleetConfig::load(&home.join("fleet.yaml")) {
        Ok(c) => c,
        Err(_) => return,
    };

    // Dispatch to Discord if configured
    #[cfg(feature = "discord")]
    if matches!(config.channel, Some(crate::fleet::ChannelConfig::Discord { .. })) {
        notify_discord(home, instance_name, text, &config);
        return;
    }

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
        _ => return,
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
                        let mut req = bot.send_message(chat_id, &text).message_thread_id(
                            teloxide::types::ThreadId(teloxide::types::MessageId(tid)),
                        );
                        if disable_notification {
                            req = req.disable_notification(true);
                        }
                        req.await?;
                    }
                    _ => {
                        let mut req = bot.send_message(chat_id, &text);
                        if disable_notification {
                            req = req.disable_notification(true);
                        }
                        req.await?;
                    }
                }
                Ok::<(), anyhow::Error>(())
            }) {
                let handled = crate::telegram::handle_send_failure(
                    &e,
                    &home_owned,
                    &instance_owned,
                    topic_id,
                    None,
                );
                if !handled {
                    tracing::warn!(error = %e, "telegram notify failed");
                }
            }
        })
        .ok();
}


/// Send a notification to Discord (instance channel).
#[cfg(feature = "discord")]
fn notify_discord(
    _home: &Path,
    instance_name: &str,
    text: &str,
    config: &crate::fleet::FleetConfig,
) {
    let channel_id = config
        .instances
        .get(instance_name)
        .and_then(|i| i.channel_id.as_ref())
        .and_then(|s| s.parse::<u64>().ok());
    let Some(channel_id) = channel_id else { return };

    let bot_token_env = match &config.channel {
        Some(crate::fleet::ChannelConfig::Discord { bot_token_env, .. }) => bot_token_env.clone(),
        _ => return,
    };
    let token = match std::env::var(&bot_token_env) {
        Ok(t) => t,
        Err(_) => return,
    };

    let text = text.to_string();
    std::thread::Builder::new()
        .name("discord_notify".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            if let Err(e) = rt.block_on(async {
                let http = serenity::all::Http::new(&token);
                let cid = serenity::all::ChannelId::new(channel_id);
                for chunk in crate::discord::split_message(&text, 2000) {
                    cid.send_message(&http, serenity::all::CreateMessage::new().content(chunk))
                        .await?;
                }
                Ok::<(), anyhow::Error>(())
            }) {
                tracing::warn!(error = %e, "discord notify failed");
            }
        })
        .ok();
}
