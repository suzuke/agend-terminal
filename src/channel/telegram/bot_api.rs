//! Telegram Bot API helpers — react, edit, download attachment.

use crate::channel::telegram::creds::*;
use crate::channel::telegram::state::*;
use teloxide::payloads::SetMessageReactionSetters;
use teloxide::prelude::Requester;

/// Map emoji name to Unicode character.
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

/// React to a message with an emoji, using the `Bot` the state already owns.
///
/// #2975: this used to `resolve_channel_only_from` (a fleet.yaml read) and
/// `Bot::new` on every call — a fresh reqwest client and connection pool per
/// 👀/✅. The transport now comes from the state owner, which is also where
/// config reload republishes it. The state lock is held only long enough to
/// clone the handle and is released before any async I/O.
pub(crate) fn try_telegram_react(
    state: &std::sync::Arc<parking_lot::Mutex<TelegramState>>,
    instance_name: &str,
    emoji: &str,
    message_id: Option<&str>,
) -> anyhow::Result<()> {
    let (bot, chat_id, home) = {
        let s = lock_state(state);
        (s.bot.clone(), s.group_id, s.home.clone())
    };
    let bot = bot.ok_or_else(|| anyhow::anyhow!("telegram bot not initialized (react)"))?;
    let mid: i32 = message_id.and_then(|m| m.parse().ok()).unwrap_or_else(|| {
        let meta_path = crate::agent_ops::metadata_path_resolved(&home, instance_name);
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
    block_on_value(async {
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
    let file = bot
        .get_file(teloxide::types::FileId(file_id.to_string()))
        .await?;
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

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static CTR: AtomicU32 = AtomicU32::new(0);
        let id = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "agend-tg-react-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// #2975: every reaction re-read the channel config from disk and built a
    /// fresh `teloxide::Bot` — a new reqwest client and connection pool per ✅ —
    /// even though `TelegramState` already owns a live `Bot`.
    ///
    /// Drives the two REAL ownership entries: `UxEventSink::emit` for both
    /// lifecycle reactions (👀 on `UserMsgReceived`, ✅ on `AgentPickedUp`) and
    /// `AgentOutboundOp::React` through `send_from_agent`. The witness is the
    /// per-call `resolve_channel_only_from`, which in the pre-fix helper sits
    /// immediately before `Bot::new` in the same function — so zero resolutions
    /// across repeated reactions means zero per-call bot construction. (The
    /// transport call itself cannot be exercised here: there is no mock-bot
    /// harness, so the reactions are driven on a bot-less contract state that
    /// fails before any network I/O.)
    #[test]
    fn repeated_reactions_reuse_state_owned_bot_2975() {
        use crate::channel::telegram::adapter::TelegramChannel;
        use crate::channel::telegram::state::TelegramState;
        use crate::channel::ux_event::{UxEvent, UxEventSink};
        use crate::channel::{AgentOutboundOp, BindingRef, Channel, MsgRef};
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::sync::Arc;

        let home = tmp_home("reuse-state-bot");
        // Bot-less contract state: the reaction must fail at the state-ownership
        // gate, never by falling back to an on-disk config (none is written).
        let state = Arc::new(Mutex::new(TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            home.clone(),
            HashMap::new(),
            Some(vec![1]),
        )));
        let channel = TelegramChannel::new(state);
        let origin = MsgRef {
            binding: BindingRef::new("telegram", Some("agent1".into()), ()),
            id: "7".into(),
        };

        crate::channel::telegram::creds::reset_channel_resolve_count();
        (&channel as &dyn UxEventSink).emit(&UxEvent::UserMsgReceived {
            origin_msg: origin.clone(),
            agent: "agent1".into(),
        });
        (&channel as &dyn UxEventSink).emit(&UxEvent::AgentPickedUp {
            origin_msg: origin.clone(),
            agent: "agent1".into(),
        });
        let err = channel
            .send_from_agent(
                "agent1",
                AgentOutboundOp::React {
                    emoji: "fire".into(),
                    message_id: Some("7".into()),
                },
            )
            .expect_err("bot-less state must Err");

        assert_eq!(
            crate::channel::telegram::creds::take_channel_resolve_count(),
            0,
            "#2975: repeated reactions must reuse the state-owned Bot, not \
             re-resolve the channel config and construct a Bot per call"
        );
        assert!(
            err.to_string().contains("bot not initialized"),
            "#2975: the reaction transport must come from the state owner \
             (got: {err})"
        );

        // A state that DOES own a Bot passes the ownership gate and only then
        // reaches the message-id gate — proving the state-owned Bot is what the
        // reaction uses as its transport handle.
        let with_bot = Arc::new(Mutex::new(TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            home.clone(),
            HashMap::new(),
            Some(vec![1]),
        )));
        let channel = TelegramChannel::new(with_bot);
        crate::channel::telegram::creds::reset_channel_resolve_count();
        let err = channel
            .send_from_agent(
                "agent1",
                AgentOutboundOp::React {
                    emoji: "fire".into(),
                    message_id: None,
                },
            )
            .expect_err("no resolvable message id must Err");
        assert!(
            err.to_string().contains("No message_id"),
            "#2975: with a state-owned Bot the reaction must pass the ownership \
             gate and stop at the message-id gate (got: {err})"
        );
        assert_eq!(
            crate::channel::telegram::creds::take_channel_resolve_count(),
            0,
            "#2975: the metadata message-id fallback must not re-resolve config"
        );
        std::fs::remove_dir_all(&home).ok();
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
