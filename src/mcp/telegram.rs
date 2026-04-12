//! Telegram integration — reply, react, edit, download via Bot API.

use serde_json::Value;
use std::sync::OnceLock;
use teloxide::net::Download;
use teloxide::prelude::*;

/// Shared tokio runtime for Telegram API calls (built once, reused).
/// Panics if tokio runtime cannot be built (system-level failure).
#[allow(clippy::unwrap_used)]
pub fn mcp_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("mcp tokio runtime")
    })
}

/// Returns (message_id, chat_id) on success.
pub fn try_telegram_reply(instance_name: &str, text: &str) -> anyhow::Result<(i32, i64)> {
    let home = crate::home_dir();
    let fleet_path = home.join("fleet.yaml");
    if !fleet_path.exists() {
        anyhow::bail!("No fleet.yaml");
    }
    let config = crate::fleet::FleetConfig::load(&fleet_path)?;

    match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram {
            bot_token_env,
            group_id,
            ..
        }) => {
            let token = std::env::var(bot_token_env)?;
            let topic_id = config
                .instances
                .get(instance_name)
                .and_then(|inst| inst.topic_id);
            let gid = *group_id;

            let msg_id = mcp_runtime().block_on(async {
                let bot = teloxide::Bot::new(&token);
                let chat_id = teloxide::types::ChatId(gid);
                let sent = if let Some(tid) = topic_id {
                    if tid == 1 {
                        bot.send_message(chat_id, text).await?
                    } else {
                        bot.send_message(chat_id, text)
                            .message_thread_id(teloxide::types::ThreadId(
                                teloxide::types::MessageId(tid),
                            ))
                            .await?
                    }
                } else {
                    anyhow::bail!("No topic_id for {instance_name}");
                };
                Ok::<i32, anyhow::Error>(sent.id.0)
            })?;
            Ok((msg_id, gid))
        }
        None => anyhow::bail!("No Telegram channel configured"),
    }
}

pub fn try_telegram_react(
    instance_name: &str,
    emoji: &str,
    message_id: Option<&str>,
) -> anyhow::Result<()> {
    let home = crate::home_dir();
    let fleet_path = home.join("fleet.yaml");
    let config = crate::fleet::FleetConfig::load(&fleet_path)?;
    match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram {
            bot_token_env,
            group_id,
            ..
        }) => {
            let token = std::env::var(bot_token_env)?;
            let mid: i32 = message_id.and_then(|m| m.parse().ok()).unwrap_or_else(|| {
                // Try to read last received message ID from metadata
                let meta_path = home.join("metadata").join(format!("{instance_name}.json"));
                std::fs::read_to_string(&meta_path)
                    .ok()
                    .and_then(|c| serde_json::from_str::<Value>(&c).ok())
                    .and_then(|m| m["last_message_id"].as_i64())
                    .unwrap_or(0) as i32
            });
            if mid == 0 {
                anyhow::bail!("No message_id to react to");
            }
            // Map emoji name to actual emoji
            let emoji_char = match emoji {
                "thumbsup" | "thumbs_up" => "👍",
                "thumbsdown" | "thumbs_down" => "👎",
                "heart" | "red_heart" => "❤",
                "fire" => "🔥",
                "clap" => "👏",
                "thinking" => "🤔",
                "pray" | "folded_hands" => "🙏",
                "party" | "tada" => "🎉",
                "eyes" => "👀",
                "100" => "💯",
                "ok" | "ok_hand" => "👌",
                "rocket" => "🚀",
                "check" | "white_check_mark" => "✅",
                other => other, // Pass through actual emoji chars
            };

            mcp_runtime().block_on(async {
                let bot = teloxide::Bot::new(&token);
                let chat_id = teloxide::types::ChatId(*group_id);
                let msg_id = teloxide::types::MessageId(mid);
                let reaction = teloxide::types::ReactionType::Emoji {
                    emoji: emoji_char.to_string(),
                };
                bot.set_message_reaction(chat_id, msg_id)
                    .reaction(vec![reaction])
                    .await?;
                Ok::<(), anyhow::Error>(())
            })?;
            Ok(())
        }
        None => anyhow::bail!("No Telegram channel configured"),
    }
}

pub fn try_telegram_edit(instance_name: &str, message_id: &str, text: &str) -> anyhow::Result<()> {
    let home = crate::home_dir();
    let fleet_path = home.join("fleet.yaml");
    let config = crate::fleet::FleetConfig::load(&fleet_path)?;
    match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram {
            bot_token_env,
            group_id,
            ..
        }) => {
            let token = std::env::var(bot_token_env)?;
            let mid: i32 = message_id
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid message_id: {message_id}"))?;

            let _ = instance_name; // suppress unused warning
            mcp_runtime().block_on(async {
                let bot = teloxide::Bot::new(&token);
                let chat_id = teloxide::types::ChatId(*group_id);
                let msg_id = teloxide::types::MessageId(mid);
                bot.edit_message_text(chat_id, msg_id, text).await?;
                Ok::<(), anyhow::Error>(())
            })?;
            Ok(())
        }
        None => anyhow::bail!("No Telegram channel configured"),
    }
}

/// Create a Telegram forum topic for a new instance.
/// Reads channel config from fleet.yaml. Returns the topic_id on success.
pub fn create_topic_for_instance(home: &std::path::Path, instance_name: &str) -> Option<i32> {
    let fleet_path = home.join("fleet.yaml");
    if !fleet_path.exists() {
        return None;
    }
    let config = crate::fleet::FleetConfig::load(&fleet_path).ok()?;
    match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram {
            bot_token_env,
            group_id,
            ..
        }) => {
            let token = std::env::var(bot_token_env).ok()?;
            let gid = *group_id;
            let topic_id = mcp_runtime().block_on(async {
                let bot = teloxide::Bot::new(&token);
                let chat_id = teloxide::types::ChatId(gid);
                let topic = bot
                    .create_forum_topic(chat_id, instance_name, 0x6FB9F0, "")
                    .await?;
                Ok::<i32, anyhow::Error>(topic.thread_id.0 .0)
            });
            match topic_id {
                Ok(tid) => {
                    eprintln!("[telegram] created topic for '{instance_name}' -> {tid}");
                    // Save topic_id back to fleet.yaml
                    let _ = crate::fleet::update_instance_field(
                        home,
                        instance_name,
                        "topic_id",
                        serde_yaml::Value::Number(serde_yaml::Number::from(tid)),
                    );
                    Some(tid)
                }
                Err(e) => {
                    eprintln!("[telegram] failed to create topic for '{instance_name}': {e}");
                    None
                }
            }
        }
        None => None,
    }
}

pub fn try_download_attachment(instance_name: &str, file_id: &str) -> anyhow::Result<String> {
    let home = crate::home_dir();
    let fleet_path = home.join("fleet.yaml");
    if !fleet_path.exists() {
        anyhow::bail!("No fleet.yaml");
    }
    let config = crate::fleet::FleetConfig::load(&fleet_path)?;
    match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram { bot_token_env, .. }) => {
            let token = std::env::var(bot_token_env)?;
            mcp_runtime().block_on(async {
                let bot = teloxide::Bot::new(&token);
                let file = bot.get_file(file_id).await?;
                let download_dir = home.join("downloads").join(instance_name);
                std::fs::create_dir_all(&download_dir)?;
                let filename = file.path.rsplit('/').next().unwrap_or("attachment");
                let dest = download_dir.join(filename);
                let mut dst = tokio::fs::File::create(&dest).await?;
                bot.download_file(&file.path, &mut dst).await?;
                Ok(dest.display().to_string())
            })
        }
        None => anyhow::bail!("No Telegram channel configured"),
    }
}
