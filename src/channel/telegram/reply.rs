//! Telegram reply / provenance — outbound message send + S2d provenance injection.

use crate::channel::telegram::creds::*;
use crate::channel::telegram::error::*;
use crate::channel::telegram::send::*;
use crate::channel::telegram::state::*;
use parking_lot::Mutex;
use std::sync::Arc;
use teloxide::payloads::SendMessageSetters;
use teloxide::prelude::Requester;
use teloxide::types::MessageId;

/// Send a reply from an agent to Telegram (called from MCP reply tool).
#[allow(dead_code)]
pub fn send_reply(
    state: &Arc<Mutex<TelegramState>>,
    instance_name: &str,
    text: &str,
) -> anyhow::Result<()> {
    let s = lock_state(state);
    let (bot, group_id, topic_id, home) = (
        s.bot
            .clone()
            .expect("telegram bot not initialized (send_reply)"),
        s.group_id,
        s.instance_to_topic.get(instance_name).copied(),
        s.home.clone(),
    );
    drop(s);
    let res = telegram_runtime().block_on(send_with_topic(&bot, group_id, topic_id, text, None));
    if let Err(e) = &res {
        handle_send_failure(e, &home, instance_name, topic_id, Some(state));
    }
    res
}

/// Core bot-send primitive shared by [`try_telegram_reply`] and
/// [`try_telegram_reply_no_cleanup`]. Performs the actual teloxide
/// call and returns the message id; does NOT classify errors, run
/// cleanup, or touch fleet state. Both public wrappers own the
/// error-branch policy (cleanup or not) so the shared core stays
/// non-authoritative.
///
/// `#[cfg(test)]` gate: pinning side-channel isolation (the PR #57
/// round-2 finding) requires forcing the post-send branch to hit a
/// topic-deleted error without a live Bot. Prod builds skip the gate
/// entirely.
pub(super) fn telegram_reply_send_inner(
    ch: &TelegramCreds,
    instance_name: &str,
    topic_id: Option<i32>,
    text: &str,
) -> anyhow::Result<i32> {
    #[cfg(test)]
    if let Some(err) = tests::take_forced_send_error() {
        return Err(err);
    }
    // If already inside an async runtime, block_on would panic. Spawn
    // fire-and-forget instead and return a sentinel msg_id. Callers from
    // the emit path log-and-discard errors, so this is safe.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let token = ch.token.clone();
        let group_id = ch.group_id;
        let text = text.to_string();
        let instance_name = instance_name.to_string();
        handle.spawn(async move {
            let bot = teloxide::Bot::new(&token);
            let chat_id = teloxide::types::ChatId(group_id);
            let res = match topic_id {
                Some(1) | None => bot.send_message(chat_id, &text).await,
                Some(tid) => {
                    bot.send_message(chat_id, &text)
                        .message_thread_id(teloxide::types::ThreadId(teloxide::types::MessageId(
                            tid,
                        )))
                        .await
                }
            };
            if let Err(e) = res {
                tracing::warn!(%e, %instance_name, "reply spawn failed");
            }
        });
        return Ok(0);
    }
    telegram_runtime().block_on(async {
        let bot = teloxide::Bot::new(&ch.token);
        let chat_id = teloxide::types::ChatId(ch.group_id);
        let sent = match topic_id {
            Some(1) | None => {
                if topic_id.is_none() {
                    anyhow::bail!("No topic_id for {instance_name}");
                }
                bot.send_message(chat_id, text).await?
            }
            Some(tid) => {
                bot.send_message(chat_id, text)
                    .message_thread_id(teloxide::types::ThreadId(MessageId(tid)))
                    .await?
            }
        };
        Ok::<i32, anyhow::Error>(sent.id.0)
    })
}

/// Send a reply from an instance to its Telegram topic. Returns (message_id, chat_id).
///
/// On topic-deleted errors, runs the cleanup path
/// ([`handle_send_failure`] → [`cleanup_deleted_topic`]) — appropriate
/// for the main send pathway where a deleted topic means the instance
/// is gone from the operator's side. Side-channels that MUST NOT have
/// this authority (e.g. S2d provenance per DESIGN §6) use
/// [`try_telegram_reply_no_cleanup`] instead.
pub(crate) fn try_telegram_reply(instance_name: &str, text: &str) -> anyhow::Result<(i32, i64)> {
    try_telegram_reply_from(&crate::home_dir(), instance_name, text)
}

pub(super) fn try_telegram_reply_from(
    home: &std::path::Path,
    instance_name: &str,
    text: &str,
) -> anyhow::Result<(i32, i64)> {
    let (ch, config) = resolve_channel_from(home)?;
    let topic_id = config
        .instances
        .get(instance_name)
        .and_then(|inst| inst.topic_id);
    match telegram_reply_send_inner(&ch, instance_name, topic_id, text) {
        Ok(msg_id) => Ok((msg_id, ch.group_id)),
        Err(e) => {
            if let Some(stale_tid) = topic_id {
                if is_topic_deleted_error(&e) {
                    if let Some(new_tid) =
                        invalidate_and_recreate_topic(home, instance_name, stale_tid)
                    {
                        tracing::info!(
                            instance = %instance_name,
                            old_topic = stale_tid,
                            new_topic = new_tid,
                            "retrying send with recreated topic"
                        );
                        return telegram_reply_send_inner(&ch, instance_name, Some(new_tid), text)
                            .map(|msg_id| (msg_id, ch.group_id));
                    }
                }
            }
            Err(e)
        }
    }
}

/// Like [`try_telegram_reply`] but the error branch does NOT run
/// [`handle_send_failure`] / [`cleanup_deleted_topic`] — reserved for
/// orthogonal side-channels that must not be authoritative over fleet
/// membership.
pub(crate) fn try_telegram_reply_no_cleanup(
    instance_name: &str,
    text: &str,
) -> anyhow::Result<(i32, i64)> {
    try_telegram_reply_no_cleanup_from(&crate::home_dir(), instance_name, text)
}

pub(super) fn try_telegram_reply_no_cleanup_from(
    home: &std::path::Path,
    instance_name: &str,
    text: &str,
) -> anyhow::Result<(i32, i64)> {
    let (ch, config) = resolve_channel_from(home)?;
    let topic_id = config
        .instances
        .get(instance_name)
        .and_then(|inst| inst.topic_id);
    telegram_reply_send_inner(&ch, instance_name, topic_id, text)
        .map(|msg_id| (msg_id, ch.group_id))
}

/// Format the S2d provenance tag body per DESIGN-stage-b-ux.md §6.
///
/// Shape: `⬅️ from {from} — DELEGATE\n   (brief: "{brief}")`.
pub(crate) fn format_provenance(from: &str, brief: &str) -> String {
    format!("⬅️ from {from} — DELEGATE\n   (brief: \"{brief}\")")
}

/// S2d provenance injection (Stage B-UX PR-C, DESIGN §6).
///
/// Routes through [`try_telegram_reply_no_cleanup`] so a failed send
/// never mutates fleet membership.
pub fn inject_provenance(target_instance: &str, from: &str, brief: &str) -> anyhow::Result<()> {
    let text = format_provenance(from, brief);
    try_telegram_reply_no_cleanup(target_instance, &text).map(|_| ())
}

#[cfg(test)]
pub(super) fn inject_provenance_from(
    home: &std::path::Path,
    target_instance: &str,
    from: &str,
    brief: &str,
) -> anyhow::Result<()> {
    let text = format_provenance(from, brief);
    try_telegram_reply_no_cleanup_from(home, target_instance, &text).map(|_| ())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::channel::telegram::topic_registry::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    // -----------------------------------------------------------------
    // Test-only error injector for `telegram_reply_send_inner`
    // -----------------------------------------------------------------
    static FORCED_SEND_ERROR: parking_lot::Mutex<Option<anyhow::Error>> =
        parking_lot::Mutex::new(None);

    pub(super) fn take_forced_send_error() -> Option<anyhow::Error> {
        FORCED_SEND_ERROR.lock().take()
    }

    fn set_forced_send_error(err: anyhow::Error) {
        *FORCED_SEND_ERROR.lock() = Some(err);
    }

    fn tmp_home(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-telegram-reply-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn channel_env_test_guard() -> parking_lot::MutexGuard<'static, ()> {
        static GUARD: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
        GUARD.lock()
    }

    #[test]
    fn format_provenance_matches_design_s6_shape() {
        let rendered = format_provenance("at-dev-1", "refactor auth middleware");
        assert_eq!(
            rendered,
            "⬅️ from at-dev-1 — DELEGATE\n   (brief: \"refactor auth middleware\")"
        );
    }

    #[test]
    fn format_provenance_distinguishes_from_and_brief_slots() {
        let normal = format_provenance("a", "b");
        let swapped = format_provenance("b", "a");
        assert_ne!(normal, swapped, "from/brief slots must not be symmetric");
        assert!(normal.contains("from a"));
        assert!(normal.contains("(brief: \"b\")"));
    }

    #[test]
    fn inject_provenance_failure_does_not_mutate_fleet_or_topic_registry() {
        let _g = channel_env_test_guard();
        let home = tmp_home("inject_prov_no_cleanup");

        let yaml = "\
channel:
  type: telegram
  bot_token_env: PR57_ROUND2_FAKE_TOKEN
  group_id: -100999999
  mode: topic
instances:
  B:
    command: /bin/true
    topic_id: 42
";
        std::fs::write(home.join("fleet.yaml"), yaml).expect("write fleet.yaml");
        std::fs::create_dir_all(home.join("channel")).ok();
        std::fs::write(home.join("channel").join("topics.json"), "{\"B\":42}")
            .expect("write topics.json");

        std::env::set_var("PR57_ROUND2_FAKE_TOKEN", "fake");
        set_forced_send_error(anyhow::anyhow!("Bad Request: message thread not found"));

        let res = inject_provenance_from(&home, "B", "sender", "do the thing");
        assert!(
            res.is_err(),
            "inject_provenance should bubble the forced error"
        );

        let fleet_yaml = std::fs::read_to_string(home.join("fleet.yaml")).expect("read fleet.yaml");
        assert!(
            fleet_yaml.contains("B:"),
            "provenance failure mutated fleet.yaml (removed B): {fleet_yaml}"
        );

        let topics_json =
            std::fs::read_to_string(home.join("channel").join("topics.json")).unwrap_or_default();
        assert!(
            topics_json.contains("\"B\""),
            "provenance failure unregistered target's topic: {topics_json}"
        );

        std::env::remove_var("PR57_ROUND2_FAKE_TOKEN");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn try_telegram_reply_cleanup_variant_mutates_fleet_on_topic_deleted() {
        let _g = channel_env_test_guard();
        let home = tmp_home("cleanup_variant_baseline");

        let yaml = "\
channel:
  type: telegram
  bot_token_env: PR57_ROUND2_FAKE_TOKEN
  group_id: -100999999
  mode: topic
instances:
  B:
    command: /bin/true
    topic_id: 42
";
        std::fs::write(home.join("fleet.yaml"), yaml).expect("write fleet.yaml");
        std::fs::create_dir_all(home.join("channel")).ok();
        std::fs::write(home.join("channel").join("topics.json"), "{\"B\":42}")
            .expect("write topics.json");

        std::env::set_var("PR57_ROUND2_FAKE_TOKEN", "fake");
        set_forced_send_error(anyhow::anyhow!("Bad Request: message thread not found"));

        let res = try_telegram_reply_from(&home, "B", "main-path send");
        assert!(res.is_err());

        let fleet_yaml = std::fs::read_to_string(home.join("fleet.yaml")).expect("read fleet.yaml");
        assert!(
            fleet_yaml.contains("B:"),
            "Sprint 23 P1: instance must survive topic invalidation; yaml was:\n{fleet_yaml}"
        );
        let config =
            crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).expect("load fleet.yaml");
        let inst_b = config.instances.get("B").expect("B exists");
        assert_eq!(
            inst_b.topic_id, None,
            "topic_id must be cleared after invalidation"
        );

        let reg = load_topic_registry(&home);
        assert!(
            !reg.contains_key(&42),
            "stale topic 42 must be unregistered"
        );

        std::env::remove_var("PR57_ROUND2_FAKE_TOKEN");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn try_telegram_reply_from_invalidates_on_topic_deleted() {
        let _g = channel_env_test_guard();
        let home = tmp_home("reply-retry-invalidate");
        let yaml = "\
channel:
  type: telegram
  bot_token_env: SPRINT23_P1_FAKE_TOKEN
  group_id: -100999999
  mode: topic
instances:
  agent-x:
    command: /bin/true
    topic_id: 42
";
        std::fs::write(home.join("fleet.yaml"), yaml).expect("write fleet.yaml");
        register_topic(&home, 42, "agent-x");
        std::env::set_var("SPRINT23_P1_FAKE_TOKEN", "fake");

        set_forced_send_error(anyhow::anyhow!("Bad Request: message thread not found"));

        let res = try_telegram_reply_from(&home, "agent-x", "hello");
        assert!(res.is_err());

        let reg = load_topic_registry(&home);
        assert!(
            !reg.contains_key(&42),
            "stale topic 42 must be unregistered after retry; reg={reg:?}"
        );

        let fleet_yaml = std::fs::read_to_string(home.join("fleet.yaml")).expect("read fleet.yaml");
        assert!(
            fleet_yaml.contains("agent-x"),
            "instance must survive topic invalidation; yaml:\n{fleet_yaml}"
        );

        std::env::remove_var("SPRINT23_P1_FAKE_TOKEN");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn try_telegram_reply_from_does_not_invalidate_on_unrelated_error() {
        let _g = channel_env_test_guard();
        let home = tmp_home("reply-no-invalidate");
        let yaml = "\
channel:
  type: telegram
  bot_token_env: SPRINT23_P1_FAKE_TOKEN2
  group_id: -100999999
  mode: topic
instances:
  agent-x:
    command: /bin/true
    topic_id: 42
";
        std::fs::write(home.join("fleet.yaml"), yaml).expect("write fleet.yaml");
        register_topic(&home, 42, "agent-x");
        std::env::set_var("SPRINT23_P1_FAKE_TOKEN2", "fake");

        set_forced_send_error(anyhow::anyhow!("Too Many Requests: retry after 5"));

        let res = try_telegram_reply_from(&home, "agent-x", "hello");
        assert!(res.is_err());

        let reg = load_topic_registry(&home);
        assert_eq!(
            reg.get(&42),
            Some(&"agent-x".to_string()),
            "unrelated error must not invalidate topic"
        );

        std::env::remove_var("SPRINT23_P1_FAKE_TOKEN2");
        std::fs::remove_dir_all(&home).ok();
    }
}
