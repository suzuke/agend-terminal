use teloxide::prelude::*;
use teloxide::types::{MessageId, ThreadId};

/// Telegram Bot API caption limit (characters).
pub(super) const CAPTION_MAX_CHARS: usize = 1024;

/// Send a message, optionally to a topic, optionally as a reply, returning
/// the platform-assigned `message_id` (i32 per Telegram Bot API).
///
/// Phase 3 (Sprint 21) extracts msg_id capture so `Channel::send` can
/// return a `MsgRef` with a real id instead of `"0"` placeholder. Other
/// callers that historically returned `Result<()>` continue to use
/// [`send_with_topic`] (thin wrapper) so the behaviour change is
/// localised to the trait-method dispatch path.
pub(super) async fn send_with_topic_capturing_id(
    bot: &Bot,
    chat_id: ChatId,
    topic_id: Option<i32>,
    text: &str,
    reply_to_msg_id: Option<i32>,
) -> anyhow::Result<i32> {
    use teloxide::payloads::SendMessageSetters;
    use teloxide::prelude::Requester;
    use teloxide::types::ReplyParameters;
    let mut req = bot.send_message(chat_id, text);
    if let Some(tid) = topic_id {
        if tid != 1 {
            req = req.message_thread_id(ThreadId(MessageId(tid)));
        }
    }
    if let Some(mid) = reply_to_msg_id {
        req = req.reply_parameters(ReplyParameters::new(MessageId(mid)));
    }
    let sent = req.send().await?;
    Ok(sent.id.0)
}

/// Backwards-compatible thin wrapper around
/// [`send_with_topic_capturing_id`] for callers that don't need the
/// returned `message_id`. Existing five call sites
/// (`send_to_topic` / `notify_telegram_inner` / `apply_fleet_action` /
/// the text-after-attachment follow-up in `Channel::send` /
/// `try_telegram_*` etc.) keep their `Result<()>` shape.
pub(super) async fn send_with_topic(
    bot: &Bot,
    chat_id: ChatId,
    topic_id: Option<i32>,
    text: &str,
    reply_to_msg_id: Option<i32>,
) -> anyhow::Result<()> {
    send_with_topic_capturing_id(bot, chat_id, topic_id, text, reply_to_msg_id)
        .await
        .map(|_| ())
}

/// Resolve the caption for a media send.
pub(super) fn resolve_caption(text: &str, att: &crate::channel::Attachment) -> Option<String> {
    use crate::channel::AttachmentKind;
    if att.kind == AttachmentKind::Sticker {
        return None;
    }
    if let Some(ref cap) = att.caption {
        return Some(cap.chars().take(CAPTION_MAX_CHARS).collect());
    }
    if !text.is_empty() && text.chars().count() <= CAPTION_MAX_CHARS {
        return Some(text.to_string());
    }
    None
}

/// Returns true when text should be sent as a separate follow-up message.
pub(super) fn needs_separate_text(text: &str, att: &crate::channel::Attachment) -> bool {
    use crate::channel::AttachmentKind;
    if text.is_empty() {
        return false;
    }
    if att.caption.is_some() {
        return true;
    }
    att.kind == AttachmentKind::Sticker || text.chars().count() > CAPTION_MAX_CHARS
}

/// Send a media attachment via the appropriate Telegram Bot API method.
pub(super) async fn send_media(
    bot: &Bot,
    chat_id: ChatId,
    topic_id: Option<i32>,
    att: &crate::channel::Attachment,
    caption: Option<&str>,
) -> anyhow::Result<i32> {
    use crate::channel::AttachmentKind;
    use teloxide::prelude::Requester;
    use teloxide::types::InputFile;

    let input = InputFile::file(&att.path);
    let input = if let Some(ref name) = att.original_filename {
        input.file_name(name.clone())
    } else {
        input
    };
    let thread = topic_id.filter(|&t| t != 1).map(|t| ThreadId(MessageId(t)));

    let msg_id = match att.kind {
        AttachmentKind::Photo => {
            let mut req = bot.send_photo(chat_id, input);
            if let Some(cap) = caption {
                req = req.caption(cap);
            }
            if let Some(tid) = thread {
                req = req.message_thread_id(tid);
            }
            req.await?.id.0
        }
        AttachmentKind::Voice => {
            let mut req = bot.send_voice(chat_id, input);
            if let Some(cap) = caption {
                req = req.caption(cap);
            }
            if let Some(tid) = thread {
                req = req.message_thread_id(tid);
            }
            req.await?.id.0
        }
        AttachmentKind::Document => {
            let mut req = bot.send_document(chat_id, input);
            if let Some(cap) = caption {
                req = req.caption(cap);
            }
            if let Some(tid) = thread {
                req = req.message_thread_id(tid);
            }
            req.await?.id.0
        }
        AttachmentKind::Video => {
            let mut req = bot.send_video(chat_id, input);
            if let Some(cap) = caption {
                req = req.caption(cap);
            }
            if let Some(tid) = thread {
                req = req.message_thread_id(tid);
            }
            req.await?.id.0
        }
        AttachmentKind::Sticker => {
            let mut req = bot.send_sticker(chat_id, input);
            if let Some(tid) = thread {
                req = req.message_thread_id(tid);
            }
            req.await?.id.0
        }
    };
    Ok(msg_id)
}
