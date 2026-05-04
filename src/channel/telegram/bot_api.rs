//! Telegram Bot API helpers — react, edit, download attachment.

use crate::channel::telegram::creds::*;
use crate::channel::telegram::state::*;
use teloxide::payloads::SetMessageReactionSetters;
use teloxide::prelude::Requester;

/// Map emoji name to Unicode character.
#[allow(dead_code)]
pub(super) fn map_emoji_name(name: &str) -> &str {
    match name {
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
        other => other,
    }
}

/// React to a message with an emoji.
pub(crate) fn try_telegram_react(
    home: &std::path::Path,
    instance_name: &str,
    emoji: &str,
    message_id: Option<&str>,
) -> anyhow::Result<()> {
    let ch = resolve_channel_only_from(home)?;
    let mid: i32 = message_id.and_then(|m| m.parse().ok()).unwrap_or_else(|| {
        let meta_path = home.join("metadata").join(format!("{instance_name}.json"));
        std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|m| m["last_message_id"].as_i64())
            .unwrap_or(0) as i32
    });
    if mid == 0 {
        anyhow::bail!("No message_id to react to");
    }
    let emoji_char = map_emoji_name(emoji).to_string();
    spawn_or_block_on(async move {
        let bot = teloxide::Bot::new(&ch.token);
        let chat_id = teloxide::types::ChatId(ch.group_id);
        let msg_id = teloxide::types::MessageId(mid);
        let reaction = teloxide::types::ReactionType::Emoji { emoji: emoji_char };
        bot.set_message_reaction(chat_id, msg_id)
            .reaction(vec![reaction])
            .await?;
        Ok::<(), anyhow::Error>(())
    })
}

/// Edit a previously sent message.
pub(crate) fn try_telegram_edit(
    home: &std::path::Path,
    _instance_name: &str,
    message_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    let ch = resolve_channel_only_from(home)?;
    let mid: i32 = message_id
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid message_id: {message_id}"))?;
    let text = text.to_string();
    spawn_or_block_on(async move {
        let bot = teloxide::Bot::new(&ch.token);
        bot.edit_message_text(
            teloxide::types::ChatId(ch.group_id),
            teloxide::types::MessageId(mid),
            &text,
        )
        .await?;
        Ok::<(), anyhow::Error>(())
    })
}

/// Download an attachment by file_id.
pub fn try_download_attachment(
    home: &std::path::Path,
    instance_name: &str,
    file_id: &str,
) -> anyhow::Result<String> {
    let ch = resolve_channel_only_from(home)?;
    telegram_runtime().block_on(async {
        let bot = teloxide::Bot::new(&ch.token);
        download_file_async(&bot, home, instance_name, file_id).await
    })
}

/// Async inner: download a telegram file to `$AGEND_HOME/downloads/{instance}/`.
pub(super) async fn download_file_async(
    bot: &teloxide::Bot,
    home: &std::path::Path,
    instance_name: &str,
    file_id: &str,
) -> anyhow::Result<String> {
    use teloxide::net::Download;
    use teloxide::prelude::Requester;
    let file = bot.get_file(file_id).await?;
    let download_dir = home.join("downloads").join(instance_name);
    std::fs::create_dir_all(&download_dir)?;
    let filename = std::path::Path::new(&file.path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("attachment");
    let dest = download_dir.join(filename);
    let mut dst = tokio::fs::File::create(&dest).await?;
    bot.download_file(&file.path, &mut dst).await?;
    Ok(dest.display().to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn map_emoji_name_known() {
        assert_eq!(map_emoji_name("thumbsup"), "👍");
        assert_eq!(map_emoji_name("thumbs_up"), "👍");
        assert_eq!(map_emoji_name("fire"), "🔥");
        assert_eq!(map_emoji_name("heart"), "❤");
        assert_eq!(map_emoji_name("rocket"), "🚀");
        assert_eq!(map_emoji_name("check"), "✅");
    }

    #[test]
    fn map_emoji_name_unknown_passthrough() {
        assert_eq!(map_emoji_name("🎵"), "🎵");
        assert_eq!(map_emoji_name("custom_emoji"), "custom_emoji");
    }

    #[test]
    fn map_emoji_name_aliases() {
        assert_eq!(map_emoji_name("pray"), "🙏");
        assert_eq!(map_emoji_name("folded_hands"), "🙏");
        assert_eq!(map_emoji_name("thumbsdown"), "👎");
        assert_eq!(map_emoji_name("thumbs_down"), "👎");
        assert_eq!(map_emoji_name("tada"), "🎉");
        assert_eq!(map_emoji_name("party"), "🎉");
    }
}
