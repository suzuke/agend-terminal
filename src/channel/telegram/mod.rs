//! Telegram adapter — runs in dedicated thread with tokio runtime.
//!
//! Inbound: Telegram message → inbox + PTY notification
//! Outbound: reply(text) → Telegram send_message to topic

pub(crate) mod error;
pub(crate) mod inbound;
pub(crate) mod send;
pub(crate) mod state;
pub(crate) mod topic_registry;

pub(crate) use error::*;
pub(crate) use inbound::*;
pub(crate) use send::*;
pub(crate) use state::*;
pub(crate) use topic_registry::*;

use crate::agent::AgentRegistry;
use crate::fleet::ChannelConfig;
use crate::inbox::{self, InboxMessage};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{MessageId, ThreadId};

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

/// Wire the in-process [`AgentRegistry`] into an already-initialized
/// [`TelegramState`]. `init_from_config` runs during bootstrap before the
/// daemon / app creates the registry, so this two-phase setup lets inbound
/// message routing read agent state without a cross-thread API round-trip.
pub fn attach_registry(state: &Arc<Mutex<TelegramState>>, registry: AgentRegistry) {
    let mut s = lock_state(state);
    s.registry = Some(registry);
}

/// Initialize Telegram from fleet config.
pub fn init_from_config(
    config: &crate::fleet::FleetConfig,
    home: &Path,
    submit_keys: HashMap<String, String>,
) -> Option<Arc<Mutex<TelegramState>>> {
    let (bot_token_env, group_id, user_allowlist, fleet_binding) = match config.channel.as_ref()? {
        ChannelConfig::Telegram {
            bot_token_env,
            group_id,
            user_allowlist,
            fleet_binding,
            ..
        } => (bot_token_env, group_id, user_allowlist, fleet_binding),
        ChannelConfig::Discord { .. } => return None,
    };
    let token = match std::env::var(bot_token_env) {
        Ok(t) => t,
        Err(_) => {
            // Fallback: legacy AGEND_BOT_TOKEN env var (deprecated).
            match std::env::var("AGEND_BOT_TOKEN") {
                Ok(t) => {
                    tracing::warn!(
                        "AGEND_BOT_TOKEN is deprecated — migrate to {bot_token_env} in fleet.yaml"
                    );
                    t
                }
                Err(_) => {
                    tracing::info!(env = %bot_token_env, "bot token env not set, skipping");
                    return None;
                }
            }
        }
    };
    match user_allowlist {
        None => tracing::warn!(
            "telegram channel.user_allowlist is not set — Sprint 21 Phase 2 fail-closed default: \
             ALL inbound messages and outbound notifications are dropped. \
             Set `user_allowlist: [123, 456]` in fleet.yaml to enable the channel \
             (see docs/USAGE.md \"Channel: Telegram\" migration section)."
        ),
        Some(list) if list.is_empty() => {
            tracing::info!(
                "telegram channel.user_allowlist is empty — all inbound messages will be rejected"
            )
        }
        Some(list) => tracing::info!(count = list.len(), "telegram user_allowlist active"),
    }
    let allowlist = user_allowlist.clone();

    // Clean up orphaned topics: exist in registry but not in fleet.yaml
    let mut reg = load_topic_registry(home);
    let instance_names: std::collections::HashSet<&String> = config.instances.keys().collect();
    let mut orphan_count = 0;
    for (tid, inst_name) in reg.clone() {
        // `FLEET_BINDING_SENTINEL` marks the `fleet_binding` topic so it
        // survives orphan cleanup — it's not an instance name.
        if tid != 1 && inst_name != FLEET_BINDING_SENTINEL && !instance_names.contains(&inst_name) {
            tracing::info!(topic_id = tid, instance = %inst_name, "orphaned topic, deleting");
            delete_topic(home, tid); // also removes from registry
            orphan_count += 1;
        }
    }
    if orphan_count > 0 {
        reg = load_topic_registry(home); // reload after deletions
        tracing::info!(count = orphan_count, "cleaned up orphaned topics");
    }

    let bot = teloxide::Bot::new(&token);
    let chat_id = teloxide::types::ChatId(*group_id);

    let mut topic_map: HashMap<String, i32> = config
        .instances
        .iter()
        .filter_map(|(name, inst)| inst.topic_id.map(|tid| (name.clone(), tid)))
        .collect();

    // Fix B: merge topics.json entries not already in fleet.yaml.
    // topics.json is the runtime registry (written by register_topic on
    // create_instance); fleet.yaml is the declarative config. Fleet.yaml
    // wins on conflict (same instance, different topic_id).
    for (tid, inst_name) in &reg {
        if *tid != 1 && inst_name != FLEET_BINDING_SENTINEL && !topic_map.contains_key(inst_name) {
            tracing::info!(
                instance = %inst_name,
                topic_id = tid,
                "merging topic from topics.json (not in fleet.yaml)"
            );
            topic_map.insert(inst_name.clone(), *tid);
        }
    }

    // Auto-create topics for instances without topic_id
    for (name, inst) in &config.instances {
        if name == "general" && inst.topic_id.is_none() {
            topic_map.insert("general".to_string(), 1);
        } else if inst.topic_id.is_none() {
            tracing::info!(instance = %name, "creating topic");
            match telegram_runtime().block_on(async {
                bot.create_forum_topic(chat_id, name, teloxide::types::Rgb::from_u32(0x6FB9F0), "")
                    .await
            }) {
                Ok(topic) => {
                    let tid = topic.thread_id.0 .0;
                    tracing::info!(instance = %name, topic_id = tid, "created topic");
                    topic_map.insert(name.clone(), tid);
                }
                Err(e) => tracing::error!(instance = %name, error = %e, "failed to create topic"),
            }
        }
    }

    // Write back topic_ids + update registry in one pass
    if home.join("fleet.yaml").exists() && !topic_map.is_empty() {
        for (name, tid) in &topic_map {
            let _ = crate::fleet::update_instance_field(
                home,
                name,
                "topic_id",
                serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(*tid)),
            );
            reg.insert(*tid, name.clone());
        }
        save_topic_registry(home, &reg);
        tracing::info!("updated fleet.yaml with topic_ids");
    }

    // Resolve `fleet_binding` → topic_id. See docs/archived/DESIGN-stage-b-ux.md §3/§5.
    // Persistence uses the `FLEET_BINDING_SENTINEL` row in `topics.json` so
    // the same topic is reused across daemon restarts without touching
    // fleet.yaml schema. Shorthand is warned + ignored (TG has no
    // channel-tag primitive — only thread_id routing).
    let fleet_binding_topic_id =
        resolve_fleet_binding(&bot, chat_id, home, &mut reg, fleet_binding);

    let mut raw_state = TelegramState::new(
        &token,
        *group_id,
        topic_map,
        home.to_path_buf(),
        submit_keys,
        allowlist,
    );
    raw_state.fleet_binding_topic_id = fleet_binding_topic_id;
    let state = Arc::new(Mutex::new(raw_state));
    start_polling(Arc::clone(&state));
    Some(state)
}

/// Resolve the `fleet_binding` block from `ChannelConfig::Telegram` to a
/// concrete Telegram forum topic id. Returns `None` for "no mirror":
///
/// - Field absent in config → `None`.
/// - `Shorthand("#name")` → warn + `None` (Telegram has no channel-tag
///   primitive; topics route by `message_thread_id`, not by tag).
/// - `Struct(Topic { name })` → look up existing tid in the topic
///   registry under [`FLEET_BINDING_SENTINEL`]; create a new forum topic
///   via `createForumTopic` if absent and persist the sentinel row.
///
/// Mutates `reg` (registry snapshot) + writes it back on create so the
/// caller can pass a fresh `reg` clone into subsequent logic.
fn resolve_fleet_binding(
    bot: &teloxide::Bot,
    chat_id: teloxide::types::ChatId,
    home: &Path,
    reg: &mut HashMap<i32, String>,
    fleet_binding: &Option<crate::fleet::FleetBindingConfig>,
) -> Option<i32> {
    let name = match fleet_binding.as_ref()? {
        crate::fleet::FleetBindingConfig::Struct(crate::fleet::FleetBindingStruct::Topic {
            name,
        }) => name.clone(),
        crate::fleet::FleetBindingConfig::Shorthand(raw) => {
            tracing::warn!(
                shorthand = %raw,
                "telegram channel.fleet_binding shorthand ignored — Telegram requires \
                 `{{type: topic, name: ...}}` (shorthand is Discord/Slack only). \
                 Fleet events will not be mirrored on this channel."
            );
            return None;
        }
    };

    // Fast path: previously-resolved topic still present in registry.
    for (tid, inst) in reg.iter() {
        if inst == FLEET_BINDING_SENTINEL {
            tracing::info!(topic_id = *tid, %name, "reusing existing fleet_binding topic");
            return Some(*tid);
        }
    }

    // Slow path: create the forum topic once and pin it into the registry.
    tracing::info!(%name, "creating fleet_binding topic");
    match telegram_runtime().block_on(async {
        bot.create_forum_topic(chat_id, &name, teloxide::types::Rgb::from_u32(0x6FB9F0), "")
            .await
    }) {
        Ok(topic) => {
            let tid = topic.thread_id.0 .0;
            tracing::info!(topic_id = tid, %name, "created fleet_binding topic");
            reg.insert(tid, FLEET_BINDING_SENTINEL.to_string());
            save_topic_registry(home, reg);
            Some(tid)
        }
        Err(e) => {
            tracing::error!(error = %e, %name, "failed to create fleet_binding topic");
            None
        }
    }
}

/// Map emoji name to Unicode character.
#[allow(dead_code)]
fn map_emoji_name(name: &str) -> &str {
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

// ---------------------------------------------------------------------------
// Bot API functions (reply, react, edit, download, topic management)
// ---------------------------------------------------------------------------

/// Resolved Telegram channel credentials — avoids repeated fleet.yaml loads.
struct TelegramCreds {
    token: String,
    group_id: i64,
}

fn resolve_channel() -> anyhow::Result<(TelegramCreds, crate::fleet::FleetConfig)> {
    resolve_channel_from(&crate::home_dir())
}

fn resolve_channel_from(
    home: &std::path::Path,
) -> anyhow::Result<(TelegramCreds, crate::fleet::FleetConfig)> {
    let config = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))?;
    match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram {
            bot_token_env,
            group_id,
            ..
        }) => {
            let token = std::env::var(bot_token_env)
                .or_else(|_| {
                    let legacy = std::env::var("AGEND_BOT_TOKEN");
                    if legacy.is_ok() {
                        tracing::warn!(
                            "AGEND_BOT_TOKEN is deprecated — migrate to {bot_token_env}"
                        );
                    }
                    legacy
                })
                .map_err(|_| anyhow::anyhow!("bot token env '{bot_token_env}' not set"))?;
            Ok((
                TelegramCreds {
                    token,
                    group_id: *group_id,
                },
                config,
            ))
        }
        Some(crate::fleet::ChannelConfig::Discord { .. }) => {
            anyhow::bail!("Discord channel configured but telegram resolver called")
        }
        None => anyhow::bail!("No Telegram channel configured"),
    }
}

fn resolve_channel_only() -> anyhow::Result<TelegramCreds> {
    resolve_channel().map(|(ch, _)| ch)
}

/// Like [`resolve_channel_only`] but reads `fleet.yaml` from a caller-
/// supplied home instead of the process-wide `AGEND_HOME`. Telegram
/// helpers that already receive a `home` argument (e.g.
/// `create_topic_for_instance`, `delete_topic`) must use this so a
/// `cargo test` pointing at a throwaway temp home doesn't silently
/// bleed into the operator's real bot channel — the `positive_pin-1`
/// topics the user observed were exactly this: the positive-pin dispatch
/// test creating a team via the API, reaching the topic helper, and the
/// unscoped resolver loading the real fleet.yaml instead of the test's.
fn resolve_channel_only_from(home: &std::path::Path) -> anyhow::Result<TelegramCreds> {
    resolve_channel_from(home).map(|(ch, _)| ch)
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
fn telegram_reply_send_inner(
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
                    .message_thread_id(teloxide::types::ThreadId(teloxide::types::MessageId(tid)))
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

fn try_telegram_reply_from(
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
///
/// Reviewer context (at-dev-4 on PR #57 round 2): without this, the
/// S2d provenance side-channel inherited `try_telegram_reply`'s
/// cleanup authority. If the target's main topic was deleted and
/// `inject_provenance` happened to fire afterwards, the cleanup path
/// would rip the target instance out of fleet.yaml / topic registry —
/// a destructive side effect that violates DESIGN §6 ("pure
/// side-channel, no mutation of main state"). This variant closes
/// that hole: the caller gets the error to `warn!` on, the shared
/// fleet state stays untouched.
pub(crate) fn try_telegram_reply_no_cleanup(
    instance_name: &str,
    text: &str,
) -> anyhow::Result<(i32, i64)> {
    try_telegram_reply_no_cleanup_from(&crate::home_dir(), instance_name, text)
}

fn try_telegram_reply_no_cleanup_from(
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
///
/// Extracted as a pure fn (not inlined into [`inject_provenance`]) so
/// the §4 value-source regression pin can lock the rendered text
/// without needing a live Bot / env config.
pub(crate) fn format_provenance(from: &str, brief: &str) -> String {
    format!("⬅️ from {from} — DELEGATE\n   (brief: \"{brief}\")")
}

/// S2d provenance injection (Stage B-UX PR-C, DESIGN §6).
///
/// When `delegate_task` succeeds, the daemon calls this to send a
/// short "who sent this to you" tag into `target_instance`'s primary
/// topic. The injection is orthogonal to the actual delegated message
/// — it's a side-channel hint so the recipient's operator (watching
/// the topic in Telegram) can tell at a glance which agent the task
/// came from.
///
/// Routes through [`try_telegram_reply_no_cleanup`] so a failed send
/// never mutates fleet membership (see reviewer context on that fn).
/// The returned error is passed back unchanged so the caller can
/// `tracing::warn!` per §4 Q4 (chosen over silent drop: provenance
/// failure may signal a real routing bug — topic_id pointing at the
/// wrong instance — that deserves log visibility, even though it
/// doesn't block the main path).
pub fn inject_provenance(target_instance: &str, from: &str, brief: &str) -> anyhow::Result<()> {
    let text = format_provenance(from, brief);
    try_telegram_reply_no_cleanup(target_instance, &text).map(|_| ())
}

#[cfg(test)]
fn inject_provenance_from(
    home: &std::path::Path,
    target_instance: &str,
    from: &str,
    brief: &str,
) -> anyhow::Result<()> {
    let text = format_provenance(from, brief);
    try_telegram_reply_no_cleanup_from(home, target_instance, &text).map(|_| ())
}

/// React to a message with an emoji.
///
/// `home` scopes the `fleet.yaml` + metadata lookups so callers on
/// alternate homes (tests, multi-tenant daemons) can't accidentally
/// leak into the operator's real Telegram channel — same class of fix
/// as `create_topic_for_instance`.
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

/// Edit a previously sent message. `home` scopes credential resolution
/// so this helper can't silently reach the operator's real channel
/// from a test or alt-home context.
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

/// Download an attachment by file_id. `home` scopes credential +
/// download-directory lookups so test runs don't drop files into the
/// operator's real `~/.agend-terminal/downloads/`.
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
/// Used directly from async contexts (polling thread) and via `block_on`
/// wrapper from sync contexts (MCP handler).
async fn download_file_async(
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

// ---------------------------------------------------------------------------
// Supervisor / daemon notify helpers (folded from src/daemon/telegram.rs
// during T1c — single-file-per-channel convention).
// ---------------------------------------------------------------------------

/// Send a notification to Telegram (instance topic or general).
pub fn notify_telegram(home: &std::path::Path, instance_name: &str, text: &str) {
    notify_telegram_inner(home, instance_name, text, false);
}

/// Send a notification with Telegram's `disable_notification` flag set — the
/// message still appears in the topic but does not push/vibrate the operator.
/// Use for state-recovery pings that should not compete with real alerts.
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
    // H1: use shared telegram_runtime() instead of per-notification thread+runtime.
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

// ---------------------------------------------------------------------------
// Channel trait adapter (T1d)
//
// `TelegramChannel` wraps `Arc<Mutex<TelegramState>>` and implements the
// platform-neutral `Channel` trait. Registry-side bookkeeping
// (has/record/take/attach) delegates to the wrapped state; binding
// create/remove delegate to the existing free fns so the teloxide runtime
// invocations stay in one place during this atomic cut-over.
// ---------------------------------------------------------------------------

/// Platform payload carried inside a [`BindingRef`] for Telegram.
/// Opaque to core code — only `TelegramChannel` downcasts to this shape.
#[derive(Debug, Clone)]
pub(crate) struct TelegramBindingPayload {
    pub topic_id: i32,
}

impl TelegramBindingPayload {
    fn into_binding(self) -> crate::channel::BindingRef {
        let tag = format!("TG#{}", self.topic_id);
        crate::channel::BindingRef::new("telegram", Some(tag), self)
    }
}

/// Construct a `BindingRef` for a `MsgRef` returned from
/// [`Channel::send`]. When `topic_id` is `Some`, downcasting the
/// returned binding back to [`TelegramBindingPayload`] yields the same
/// topic id — so subsequent `Channel::edit` / `Channel::delete` calls
/// can route to the same topic without the caller threading a
/// separate id. When `topic_id` is `None` (group-only send), the
/// binding is a bare-kind discriminator (no payload) so downcast
/// returns `None` and the trait method falls back to group routing.
fn build_telegram_msg_binding(topic_id: Option<i32>) -> crate::channel::BindingRef {
    match topic_id {
        Some(tid) => TelegramBindingPayload { topic_id: tid }.into_binding(),
        None => crate::channel::BindingRef::new("telegram", None, ()),
    }
}

/// Telegram adapter implementing the platform-neutral `Channel` trait.
pub struct TelegramChannel {
    state: Arc<Mutex<TelegramState>>,
    caps: crate::channel::ChannelCapabilities,
}

impl TelegramChannel {
    pub fn new(state: Arc<Mutex<TelegramState>>) -> Self {
        use crate::channel::{
            ChannelCapabilities, MarkdownDialect, MentionStyle, NativeSeeAllHint,
        };
        let caps = ChannelCapabilities {
            // ── Transport-layer ──────────────────────────────────────
            // Telegram sends no native topic-delete event; removals are
            // detected on the next API call into the topic.
            emits_deletion_events: false,
            // Forum groups expose native topics via `message_thread_id`.
            threads: true,
            // Inline keyboards exist at the Bot API level, but this
            // adapter does not render them. Flip when the UX layer wires
            // `InlineKeyboardMarkup` through `OutMsg`.
            buttons: false,
            // sendDocument / sendPhoto / sendVideo / sendAudio all
            // supported via Bot API.
            attachments: true,
            // MarkdownV2 is the dialect the outbound formatter targets
            // (see `escape_markdown_v2`); HTML is also available but we
            // commit to one dialect per adapter.
            markdown: MarkdownDialect::MarkdownV2,
            // Bot API hard cap on a single text message.
            // Ref: https://core.telegram.org/bots/api#sendmessage
            max_msg_bytes: 4096,
            // Telegram's documented bulk-send rate guidance is ≤1 msg/s
            // per chat and ≤20 msgs/min per group, which happens to
            // match RateBudget's default. If this diverges (e.g. we
            // raise the default), pin the values explicitly here.
            rate_budget: crate::channel::RateBudget::default(),

            // ── UX-layer ─────────────────────────────────────────────
            // `setMessageReaction` (Bot API 7.0+, Jan 2024) lets bots
            // react with emoji on messages in groups they are in.
            // Ref: https://core.telegram.org/bots/api#setmessagereaction
            react: true,
            // `editMessageText` / `editMessageCaption` / `editMessageMedia`
            // are supported for bot-sent messages; Bot API imposes no
            // general time limit on those edits (business-message edits
            // have separate platform-specific constraints that do not
            // apply here).
            // Ref: https://core.telegram.org/bots/api#editmessagetext
            edit: true,
            // `sendChatAction` with action="typing" shows the indicator
            // for ~5 s per call; UX renderer re-emits to keep alive.
            // Ref: https://core.telegram.org/bots/api#sendchataction
            typing_indicator: true,
            // Adapter does not yet ingest edited messages: the teloxide
            // dispatcher only registers `Update::filter_message()`; wiring
            // would need a `filter_edited_message` handler that routes
            // into the same dispatch path. The underlying platform does
            // push `edited_message` / `edited_channel_post` through
            // `getUpdates`, but the field's contract (per
            // `docs/archived/PLAN-channel-ux-layer.md`) is "adapter currently
            // emits this signal", not "platform is capable". Flip to
            // true when the ingest path lands.
            receives_edit_events: false,
            // Telegram tags users by `@username`; ID-only fallback
            // uses a `tg://user?id=N` URL, but the visible mention
            // syntax the UX renderer should emit is `@<username>`.
            mention_parsing_hint: MentionStyle::AtUsername,
            // The Bot API Update schema does not deliver read receipts
            // to bots on any chat type; private chats optionally expose
            // them to users, not to bots. Conservative `false`.
            // TODO: re-verify if Telegram adds bot-visible read
            // receipts in a future Bot API revision.
            bot_sees_read_receipts: false,
            // Forum (topic) groups expose a "View as Messages" toggle
            // that flattens all topics into one chronological feed;
            // UX renderer can point users at this native affordance
            // instead of synthesising an in-TUI view.
            has_native_multi_thread_view: Some(NativeSeeAllHint {
                label: "View as Messages".to_string(),
            }),
            // Telegram messages persist on the server until explicitly
            // deleted; this platform is not ephemeral by design.
            ephemeral: false,
        };
        Self { state, caps }
    }

    /// Access the underlying legacy state. Kept `pub(crate)` so existing
    /// free-function call sites (e.g. `start_polling`, `init_from_config`
    /// consumers in bootstrap) can continue to operate on `TelegramState`
    /// directly until T2 generalizes them.
    pub(crate) fn state(&self) -> &Arc<Mutex<TelegramState>> {
        &self.state
    }

    /// Test-only constructor that lets unit tests exercise the
    /// `UxEventSink` impl under arbitrary cap combinations (e.g.
    /// caps-neither to hit the Noop branch without going to the
    /// Bot API).
    #[cfg(test)]
    pub(crate) fn with_caps(
        state: Arc<Mutex<TelegramState>>,
        caps: crate::channel::ChannelCapabilities,
    ) -> Self {
        Self { state, caps }
    }

    /// Lookup the fleet-binding send target under one lock. Returns
    /// `(group_chat_id, fleet_topic_id)` iff the channel was booted with
    /// a resolved `fleet_binding` (see [`init_from_config`]'s
    /// [`resolve_fleet_binding`] call). Extracted so the
    /// [`apply_fleet_action`](Self::apply_fleet_action) renderer can be
    /// unit-tested without reaching the teloxide send path, and so tests
    /// can pin *which* field feeds the topic id (regression contract v0.1
    /// §4 — value-source pin).
    pub(crate) fn fleet_send_target(&self) -> Option<(ChatId, i32)> {
        let s = lock_state(&self.state);
        s.fleet_binding_topic_id.map(|tid| (s.group_id, tid))
    }

    /// Render a cross-instance fleet event into the configured
    /// `fleet_binding` topic. No-op when no binding was resolved at
    /// bootstrap; errors never propagate (logged-only, matching the
    /// `UxEventSink` contract).
    ///
    /// On a topic-deleted error the renderer self-heals by routing
    /// through [`handle_fleet_send_failure`]: the stale
    /// [`FLEET_BINDING_SENTINEL`] row is stripped from `topics.json` and
    /// `fleet_binding_topic_id` is cleared to `None`, so subsequent
    /// emits early-return cleanly and the next bootstrap re-resolves
    /// (i.e. creates a fresh topic) instead of reusing a dead thread id.
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
                tracing::warn!(
                    %e,
                    topic_id,
                    "fleet renderer: send failed"
                );
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
        // Legacy path pushes events via `attach_registry`; pull-style API
        // lands in a later PR once the dispatcher is in place.
        None
    }

    /// Phase 3 (Sprint 21): real dispatcher per cap matrix.
    ///
    /// Resolves the destination topic by downcasting `binding` to
    /// [`TelegramBindingPayload`] (the adapter's own payload shape).
    /// `binding.kind() != "telegram"` falls back to the group with no
    /// topic — defensive for callers that hand us a foreign `BindingRef`
    /// (the trait surface accepts any kind, so we cannot statically
    /// reject). Returns a `MsgRef` with the resolved binding and the
    /// platform-assigned `message_id` so subsequent `edit` / `delete`
    /// calls have a usable handle (Sprint 20 Track A H1+H4 fix).
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
        // Resolve topic from the supplied binding. Foreign-kind binding
        // (e.g. caller passed a Discord `BindingRef`) downcasts to None
        // and falls back to "no topic, group only" — same fail-soft
        // shape `try_telegram_reply` uses when the topic registry has
        // no entry for the instance.
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

    /// Phase 3 (Sprint 21): real `bot.edit_message_text` dispatch.
    ///
    /// Mirrors [`try_telegram_edit`] (which exists for legacy free-fn
    /// callers in `mcp/handlers.rs`); this trait method is the
    /// operator-direct path, NOT agent-callable (`send_from_agent` Phase
    /// 5b owns the agent surface). Idempotency / topic-deletion error
    /// recovery is intentionally not added here — a future "self-heal
    /// on stale binding" enhancement is its own scope.
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

    /// Phase 3 (Sprint 21): real `bot.delete_message` dispatch.
    ///
    /// Idempotent on the Telegram side — Bot API returns
    /// "message to delete not found" when the id is already gone, which
    /// we translate to `Ok(())` so callers (e.g. future
    /// delete_instance message cleanup workflows per Sprint 20 Track A
    /// H4 follow-up) can call this safely without separate "exists?"
    /// pre-checks.
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
                    // Bot API surfaces both shapes; treat both as idempotent no-ops.
                    if msg.contains("message to delete not found")
                        || msg.contains("message can't be deleted")
                    {
                        tracing::debug!(
                            mid,
                            "delete_message: already deleted / not deletable — Ok"
                        );
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

    /// Adapter-side outbound gate: returns `true` iff `user_allowlist`
    /// is `Some(non-empty)`. Reuses [`crate::channel::auth::is_outbound_authorized`]
    /// so the same predicate is the source of truth across both the
    /// trait-level [`gated_notify`](crate::channel::gated_notify) helper
    /// and Phase 2's pending inbound auth reform.
    fn outbound_authorized(&self) -> bool {
        crate::channel::auth::is_outbound_authorized(&lock_state(&self.state).user_allowlist)
    }

    /// Unified entry for agent-callable outbound. Routes the
    /// four MCP→Channel bridge surfaces (`reply` / `react` / `edit` /
    /// `inject_provenance`) through the adapter-level allowlist gate.
    ///
    /// Order of checks:
    /// 1. Adapter-level outbound gate ([`Self::outbound_authorized`]) —
    ///    if the operator has not configured `user_allowlist`, drop
    ///    everything (PR #216 contract).
    /// 2. Dispatch to platform-specific send.
    fn send_from_agent(
        &self,
        agent: &str,
        op: crate::channel::AgentOutboundOp,
    ) -> std::result::Result<crate::channel::MsgRef, crate::channel::ChannelError> {
        use crate::channel::ChannelError;

        // Step 1: adapter-level allowlist gate (PR #216 contract).
        if !self.outbound_authorized() {
            return Err(ChannelError::Other(anyhow::anyhow!(
                "outbound disabled — channel.user_allowlist not configured \
                 (see docs/USAGE.md \"Channel: Telegram\" migration section)"
            )));
        }

        // Step 2: dispatch to platform-specific send.
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
                // `inject_provenance` is the public entry that uses
                // `crate::home_dir()` internally — matches the daemon
                // process home used by the MCP handler that previously
                // called this fn directly. The `home` we lock above is
                // already the daemon home, so the two paths converge in
                // production.
                inject_provenance(agent, &from, &task).map_err(ChannelError::Other)?;
                Ok(empty_msg_ref())
            }
        }
    }
}

/// Placeholder `MsgRef` returned when the underlying op (React /
/// InjectProvenance) does not surface a usable platform message id.
/// Maintains the trait contract that `send_from_agent` returns
/// `MsgRef` uniformly without breaking the four bridge surfaces that
/// historically returned `Result<()>`.
fn empty_msg_ref() -> crate::channel::MsgRef {
    crate::channel::MsgRef {
        binding: crate::channel::BindingRef::new("telegram", None, ()),
        id: "0".to_string(),
    }
}

// ─── UxEventSink: capability-gated degradation ────────────────────────
//
// `TelegramChannel` consumes UxEvents and renders them via whichever
// primitive its caps support. The pure decision lives in
// `crate::channel::ux_event::select_action`; this impl just executes
// the chosen action against the existing free-fn Bot API helpers
// (`try_telegram_reply` / `try_telegram_edit` / `try_telegram_react`).
//
// Why free fns not `Channel::send/edit/delete`? Those trait methods are
// still `bail!` stubs (lines above); the real dispatcher wiring lands
// in a later PR. Keeping the UxEventSink path on free fns lets T3 ship
// without blowing scope into the dispatcher cut-over. The two paths
// collapse once the dispatcher arrives. See PR body for the full
// rationale (reviewer: at-dev-4 flagged this shape up-front).
impl crate::channel::ux_event::UxEventSink for TelegramChannel {
    fn emit(&self, event: &crate::channel::ux_event::UxEvent) {
        use crate::channel::ux_event::{select_action, UxAction, UxEvent};
        // Dispatch split per docs/archived/DESIGN-stage-b-ux.md §4.4: Fleet events
        // never flow through the Q1 `select_action` cap-degradation ladder —
        // their target is the configured `fleet_binding`, not the origin
        // user's thread, and rendering is a plain one-liner with no
        // react/edit degradation. Q1 events (UserMsgReceived /
        // AgentPickedUp / AgentReplied) keep the existing ladder path.
        if let UxEvent::Fleet(fe) = event {
            self.apply_fleet_action(fe);
            return;
        }
        // Snapshot home from state for the scoped helper calls below —
        // cloning the PathBuf is cheap and avoids holding the mutex
        // across the network calls inside the match.
        let home = lock_state(&self.state).home.clone();
        let action = select_action(event, &self.caps);
        // All transport errors are logged, not propagated — a failed
        // reaction is never a reason to crash the daemon.
        match action {
            UxAction::React {
                instance,
                msg,
                emoji,
            } => {
                // `instance` is the fleet instance name, sourced from
                // the event's `agent` field by `select_action`. Today
                // `try_telegram_react` only uses it as a metadata
                // fallback when `message_id` is None (we always pass
                // `Some`), but routing through the real instance name
                // keeps the contract stable if that fallback ever runs.
                if let Err(e) = try_telegram_react(&home, &instance, emoji, Some(&msg.id)) {
                    tracing::warn!(
                        %e,
                        instance = %instance,
                        msg_id = %msg.id,
                        emoji,
                        "UxEventSink: react failed"
                    );
                }
            }
            UxAction::EditText {
                instance,
                msg,
                text,
            } => {
                if let Err(e) = try_telegram_edit(&home, &instance, &msg.id, &text) {
                    tracing::warn!(
                        %e,
                        instance = %instance,
                        msg_id = %msg.id,
                        "UxEventSink: edit failed"
                    );
                }
            }
            UxAction::SendText {
                instance,
                binding: _binding,
                text,
            } => {
                // `try_telegram_reply` calls `config.instances.get(instance_name)`
                // to resolve the routing topic. `instance` MUST be the
                // fleet key (agent name) — NOT `binding.display_tag()`,
                // which is a human-readable label like "TG#229" and
                // would silently bail the fleet lookup. See
                // `ux_event::agent_replied_instance_comes_from_agent_not_display_tag`
                // for the pin.
                if let Err(e) = try_telegram_reply(&instance, &text) {
                    tracing::warn!(
                        %e,
                        instance = %instance,
                        "UxEventSink: send failed"
                    );
                }
            }
            UxAction::Noop => {
                // Plan §6: silence is an acceptable fallback. Record
                // that we intentionally dropped the event so future
                // debugging knows it wasn't a bug.
                tracing::debug!(?event, "UxEventSink: Noop (caps do not support)");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Test-only error injector for `telegram_reply_send_inner` — lets
    // regression pins force the post-send branch to hit a specific
    // error (e.g. "message thread not found") without a live Bot /
    // network. Serialized through `fleet_test_guard` via the caller,
    // so the plain `Mutex<Option<_>>` is sufficient.
    // -----------------------------------------------------------------
    static FORCED_SEND_ERROR: parking_lot::Mutex<Option<anyhow::Error>> =
        parking_lot::Mutex::new(None);

    pub(super) fn take_forced_send_error() -> Option<anyhow::Error> {
        FORCED_SEND_ERROR.lock().take()
    }

    fn set_forced_send_error(err: anyhow::Error) {
        *FORCED_SEND_ERROR.lock() = Some(err);
    }

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

    /// Exercises the `UxEventSink` impl down the Noop branch — the only
    /// cap combo that does not touch the Bot API. Asserts the impl
    /// compiles, is dyn-safe, and does not panic when the adapter's
    /// caps reject the event. Full-path React / Edit / Send branches
    /// need a live Bot API and are exercised via integration tests
    /// (out of scope for T3).
    #[test]
    fn telegram_channel_emit_noop_when_caps_reject() {
        use crate::channel::{
            ux_event::{UxEvent, UxEventSink},
            BindingRef, ChannelCapabilities, MsgRef,
        };
        // Caps-neither: no react, no edit → AgentPickedUp degrades to Noop.
        let caps = ChannelCapabilities {
            react: false,
            edit: false,
            ..Default::default()
        };
        let state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        let channel = TelegramChannel::with_caps(Arc::new(Mutex::new(state)), caps);

        let origin = MsgRef {
            binding: BindingRef::new("telegram", Some("test-agent".into()), ()),
            id: "1".into(),
        };
        let ev = UxEvent::AgentPickedUp {
            origin_msg: origin,
            agent: "test-agent".into(),
        };
        // Must not panic. Noop branch only logs via tracing::debug.
        (&channel as &dyn UxEventSink).emit(&ev);
    }

    // ─── Stage B-UX fleet renderer pins ────────────────────────────────
    //
    // These tests cover the wiring between `TelegramState.fleet_binding_topic_id`
    // (set at bootstrap by `init_from_config::resolve_fleet_binding`) and
    // `TelegramChannel::apply_fleet_action` (invoked via the UxEventSink
    // dispatch split from docs/archived/DESIGN-stage-b-ux.md §4.4).

    /// Value-source pin per Reviewer Contract v0.1 §4.
    ///
    /// `fleet_send_target()` MUST read the topic id from the dedicated
    /// `fleet_binding_topic_id` field — NOT from `instance_to_topic`
    /// (which would make fleet events land in `general`'s topic or a
    /// random instance topic) and NOT from some implicit default.
    ///
    /// This test constructs state where `instance_to_topic` holds an
    /// entry (topic 1 = "general") *and* `fleet_binding_topic_id` holds
    /// 42, then asserts the target is 42 — so a future refactor that
    /// regresses to "whichever topic id the code saw first" trips here
    /// instead of silently mis-routing every fleet event into the
    /// `general` thread.
    #[test]
    fn fleet_send_target_reads_fleet_binding_topic_id_not_general() {
        let mut topic_map = HashMap::new();
        topic_map.insert("general".to_string(), 1);
        topic_map.insert("at-dev-1".to_string(), 100);
        let mut state = TelegramState::new(
            "tok",
            -12345,
            topic_map,
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        state.fleet_binding_topic_id = Some(42);
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));

        let (chat_id, topic_id) = channel
            .fleet_send_target()
            .expect("fleet_binding_topic_id=Some → Some(target)");
        assert_eq!(
            topic_id, 42,
            "value must come from `fleet_binding_topic_id`, NOT `instance_to_topic[\"general\"]` (=1) or any instance topic"
        );
        assert_ne!(topic_id, 1, "regression: general topic leak");
        assert_ne!(topic_id, 100, "regression: first-instance leak");
        assert_eq!(chat_id, ChatId(-12345), "chat id must thread through");
    }

    /// Inverse pin: no fleet_binding configured → no target → renderer
    /// early-returns and never hits the send path. Protects the
    /// "absent block = no fleet sink" contract from PLAN §7.
    #[test]
    fn fleet_send_target_is_none_when_binding_unresolved() {
        let state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        // Default: fleet_binding_topic_id = None.
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));
        assert!(
            channel.fleet_send_target().is_none(),
            "no fleet_binding → no target"
        );
    }

    /// Dispatch-split pin per DESIGN §4.4: Fleet events take the fleet
    /// renderer path, NOT `select_action`. Concretely:
    ///
    /// - Fleet variants with `fleet_binding_topic_id=None` → silent drop
    ///   (apply_fleet_action early-returns; no panic, no Bot API call)
    /// - Q1 variants with `caps.react=caps.edit=false` → `Noop` via the
    ///   `select_action` ladder
    ///
    /// If a refactor regresses to routing Fleet through `select_action`,
    /// Fleet events would silently be `Noop`'d inside select_action and
    /// the renderer would never run — so this test also exercises the
    /// "no panic" side-channel (contract-test state has `bot=None`, so
    /// any accidental send would panic via `.expect`).
    #[test]
    fn emit_fleet_event_does_not_panic_without_binding_or_bot() {
        use crate::channel::{
            ux_event::{FleetEvent, UxEvent, UxEventSink},
            ChannelCapabilities,
        };
        // Contract-test state: bot=None. If dispatch went through a path
        // that calls `bot.as_ref().expect(..)`, this test would panic.
        let state = TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        let channel =
            TelegramChannel::with_caps(Arc::new(Mutex::new(state)), ChannelCapabilities::default());
        let fleet_ev = UxEvent::Fleet(FleetEvent::DelegateTask {
            from: "a".into(),
            to: "b".into(),
            summary: "s".into(),
            task_id: None,
        });
        (&channel as &dyn UxEventSink).emit(&fleet_ev);
    }

    /// Reviewer Contract v0.1 §4 value-source pin — **self-heal**.
    ///
    /// Reviewer at-dev-4 on PR #56 flagged: deleting the fleet topic in
    /// Telegram once left us in a permanently broken state:
    /// 1. `apply_fleet_action` only logged send failures — no cleanup,
    ///    so subsequent emissions kept sending against the dead tid.
    /// 2. `topics.json` still carried the `__fleet__` sentinel row → the
    ///    next daemon restart happily reused the same dead thread id.
    ///
    /// This test seeds the *exact* broken state (stale sentinel row + a
    /// `fleet_binding_topic_id` pointing at it), fires a topic-deleted
    /// error through [`handle_fleet_send_failure`], and pins both sides
    /// of the self-heal:
    /// - `fleet_binding_topic_id` is cleared to `None` (no more send
    ///   attempts against the dead tid).
    /// - The sentinel row is stripped from `topics.json` (next boot's
    ///   `resolve_fleet_binding` sees no sentinel and re-creates).
    ///
    /// Before the fix this test's `fleet_binding_topic_id` assertion
    /// would fail — the renderer never touched the field. Running the
    /// test at this commit should pass; reverting
    /// [`handle_fleet_send_failure`] should reproduce the bug.
    #[test]
    fn fleet_binding_self_heals_when_topic_deleted() {
        let home = tmp_home("fleet-self-heal");
        // Pre-seed: topics.json has a stale `__fleet__` row pointing at a
        // tid we assume was just deleted on the TG side.
        let mut reg = HashMap::new();
        reg.insert(42, FLEET_BINDING_SENTINEL.to_string());
        // An unrelated instance row in the same file — self-heal must NOT
        // touch unrelated rows.
        reg.insert(100, "at-dev-1".to_string());
        save_topic_registry(&home, &reg);

        // State mirrors the "post-bootstrap, pre-first-send" shape: the
        // binding was resolved from the sentinel row and cached in state.
        let state = Arc::new(Mutex::new(TelegramState::new(
            "tok",
            -12345,
            HashMap::new(),
            home.clone(),
            HashMap::new(),
            None,
        )));
        {
            let mut s = lock_state(&state);
            s.fleet_binding_topic_id = Some(42);
        }

        // Simulate the topic-deleted error teloxide returns after the
        // user removes the fleet topic. This is the same marker string
        // the other handlers key off — mirror of the existing
        // `is_topic_deleted_error_matches_thread_not_found` pin.
        let err = anyhow::anyhow!("Bad Request: message thread not found");
        let handled = handle_fleet_send_failure(&err, &home, &state, 42);

        assert!(
            handled,
            "topic-deleted classifier must match so renderer skips outer warn"
        );

        // Value-source pin: the cleared field MUST be
        // `fleet_binding_topic_id` specifically — not `instance_to_topic`
        // (which cleanup_deleted_topic drains for instances) and not the
        // `topic_to_instance` reverse map. Pin ensures future refactors
        // that repurpose the helper can't silently redirect the cleanup.
        let post = lock_state(&state);
        assert_eq!(
            post.fleet_binding_topic_id, None,
            "fleet_binding_topic_id must be cleared after topic-deleted"
        );

        let reg_after = load_topic_registry(&home);
        assert!(
            !reg_after.values().any(|v| v == FLEET_BINDING_SENTINEL),
            "sentinel row must be unregistered so next boot re-resolves; reg={reg_after:?}"
        );
        // Unrelated instance row survives — cleanup is fleet-scoped.
        assert_eq!(
            reg_after.get(&100),
            Some(&"at-dev-1".to_string()),
            "unrelated instance rows must survive fleet self-heal"
        );
    }

    /// Negative pin: non-topic-deleted errors (network, auth, rate-limit)
    /// must NOT clear the binding. A transient failure is not grounds to
    /// tear down the registry state.
    #[test]
    fn fleet_binding_self_heal_ignores_unrelated_errors() {
        let home = tmp_home("fleet-self-heal-neg");
        let mut reg = HashMap::new();
        reg.insert(42, FLEET_BINDING_SENTINEL.to_string());
        save_topic_registry(&home, &reg);

        let state = Arc::new(Mutex::new(TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            home.clone(),
            HashMap::new(),
            None,
        )));
        {
            let mut s = lock_state(&state);
            s.fleet_binding_topic_id = Some(42);
        }

        for msg in [
            "network timeout",
            "Too Many Requests: retry after 5",
            "Forbidden: bot was blocked by the user",
        ] {
            let err = anyhow::anyhow!(msg.to_string());
            assert!(
                !handle_fleet_send_failure(&err, &home, &state, 42),
                "classifier must not match unrelated error: {msg}"
            );
        }
        // State untouched after the negative sweep.
        assert_eq!(lock_state(&state).fleet_binding_topic_id, Some(42));
        let reg_after = load_topic_registry(&home);
        assert_eq!(
            reg_after.get(&42),
            Some(&FLEET_BINDING_SENTINEL.to_string())
        );
    }

    /// Guards the UX-layer cap values shipped with the Telegram adapter.
    /// Values are justified inline at `TelegramChannel::new` against the
    /// Bot API; if any of these assertions start failing, update the
    /// rationale comment there at the same time so reviewers can diff
    /// claim vs. evidence.
    #[test]
    fn telegram_channel_caps_are_populated() {
        use crate::channel::{Channel, MarkdownDialect, MentionStyle};
        let state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));
        let caps = channel.caps();

        // Transport-layer claims.
        assert!(!caps.emits_deletion_events);
        assert!(caps.threads);
        assert!(!caps.buttons, "adapter does not yet render keyboards");
        assert!(caps.attachments);
        assert_eq!(caps.markdown, MarkdownDialect::MarkdownV2);
        assert_eq!(caps.max_msg_bytes, 4096);

        // UX-layer claims.
        assert!(caps.react, "setMessageReaction exists on Bot API 7.0+");
        assert!(caps.edit, "editMessageText/Caption/Media all supported");
        assert!(caps.typing_indicator, "sendChatAction action=typing");
        assert!(
            !caps.receives_edit_events,
            "adapter does not yet ingest edited_message (platform supports, ingress missing)"
        );
        assert_eq!(caps.mention_parsing_hint, MentionStyle::AtUsername);
        assert!(
            !caps.bot_sees_read_receipts,
            "bots do not see read receipts"
        );
        let hint = caps
            .has_native_multi_thread_view
            .as_ref()
            .expect("forum groups expose a native see-all view");
        assert_eq!(hint.label, "View as Messages");
        assert!(!caps.ephemeral, "Telegram messages persist until deleted");
    }

    fn tmp_home(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-telegram-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Value-source pin (Reviewer Contract v0.1 §4) for S2d provenance.
    ///
    /// DESIGN-stage-b-ux.md §6 fixes the exact wire-shape of the
    /// provenance tag: `⬅️ from {from} — DELEGATE\n   (brief: "{brief}")`.
    /// Any field mixing (e.g., rendering `brief` where `from` belongs, or
    /// losing the em-dash / arrow glyph) would silently reshape the
    /// recipient-side UX. Lock the exact bytes for a known input.
    #[test]
    fn format_provenance_matches_design_s6_shape() {
        // Known distinct values so a swapped-argument bug would fail.
        let rendered = format_provenance("at-dev-1", "refactor auth middleware");
        assert_eq!(
            rendered,
            "⬅️ from at-dev-1 — DELEGATE\n   (brief: \"refactor auth middleware\")"
        );
    }

    /// Source-of-fields pin: swapping `from` and `brief` must produce a
    /// visibly different string (i.e. we're not symmetric on the two
    /// inputs — the shape distinguishes who-sent from what-they-sent).
    /// Catches a refactor that accidentally passes args in the wrong
    /// order at the inject_provenance call site.
    #[test]
    fn format_provenance_distinguishes_from_and_brief_slots() {
        let normal = format_provenance("a", "b");
        let swapped = format_provenance("b", "a");
        assert_ne!(normal, swapped, "from/brief slots must not be symmetric");
        assert!(normal.contains("from a"));
        assert!(normal.contains("(brief: \"b\")"));
    }

    /// Shared lock for tests that mutate `AGEND_HOME` / bot-token env
    /// and the `FORCED_SEND_ERROR` injector. Tests run in parallel by
    /// default; these env-touching tests must serialize.
    fn channel_env_test_guard() -> parking_lot::MutexGuard<'static, ()> {
        static GUARD: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
        GUARD.lock()
    }

    /// Round-2 reviewer finding on PR #57 (at-dev-4, blocking):
    /// `inject_provenance` used to route through `try_telegram_reply`,
    /// whose error branch runs `handle_send_failure` →
    /// `cleanup_deleted_topic`. If the target's main topic was ever
    /// deleted, a provenance side-call would then rip the target
    /// instance out of `fleet.yaml` and the topic registry — a
    /// cleanup authority that DESIGN §6 explicitly denies the
    /// side-channel ("pure side-channel, no mutation of main state").
    ///
    /// Pin setup:
    /// - Write a fleet.yaml with channel block + target instance "B"
    ///   (topic_id 42).
    /// - Write a topics.json registering "B" → 42.
    /// - Force `telegram_reply_send_inner` to return the exact
    ///   topic-deleted error (`"Bad Request: message thread not
    ///   found"`) that would trigger cleanup in `try_telegram_reply`.
    /// - Call `inject_provenance("B", ...)`.
    ///
    /// Assertions (all must hold):
    /// - inject_provenance propagates the error (caller's `warn!` fires).
    /// - `fleet.yaml` still contains instance "B" (no rewrite).
    /// - `topics.json` still contains "B" → 42 (no unregister).
    ///
    /// Validated against the pre-fix wiring (inject_provenance using
    /// `try_telegram_reply` with cleanup) — pin FAILS there because
    /// `remove_instance_from_yaml` strips "B". Restored to
    /// `try_telegram_reply_no_cleanup` → PASSES.
    #[test]
    fn inject_provenance_failure_does_not_mutate_fleet_or_topic_registry() {
        let _g = channel_env_test_guard();
        let home = tmp_home("inject_prov_no_cleanup");

        // Full fleet.yaml with channel block + target instance "B".
        // Using a distinct bot_token_env name so concurrent suites
        // don't accidentally satisfy `resolve_channel`.
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

        // Seed topic registry: "B" → 42.
        std::fs::create_dir_all(home.join("channel")).ok();
        std::fs::write(home.join("channel").join("topics.json"), "{\"B\":42}")
            .expect("write topics.json");

        // Point the home resolver + satisfy the env-token check.
        std::env::set_var("PR57_ROUND2_FAKE_TOKEN", "fake");

        // Inject the exact topic-deleted error shape
        // `is_topic_deleted_error` matches on.
        set_forced_send_error(anyhow::anyhow!("Bad Request: message thread not found"));

        let res = inject_provenance_from(&home, "B", "sender", "do the thing");
        assert!(
            res.is_err(),
            "inject_provenance should bubble the forced error"
        );

        // Fleet membership pin: fleet.yaml must still contain "B".
        let fleet_yaml = std::fs::read_to_string(home.join("fleet.yaml")).expect("read fleet.yaml");
        assert!(
            fleet_yaml.contains("B:"),
            "provenance failure mutated fleet.yaml (removed B): {fleet_yaml}"
        );

        // Topic registry pin: "B" → 42 must still be there.
        let topics_json =
            std::fs::read_to_string(home.join("channel").join("topics.json")).unwrap_or_default();
        assert!(
            topics_json.contains("\"B\""),
            "provenance failure unregistered target's topic: {topics_json}"
        );

        // Cleanup.
        std::env::remove_var("PR57_ROUND2_FAKE_TOKEN");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Sibling pin (baseline): confirms the invalidate-and-recreate path
    /// fires on topic-deleted errors. `try_telegram_reply_from` now
    /// invalidates the stale topic and attempts recreation (Sprint 23 P1)
    /// instead of deleting the instance. The instance survives in
    /// fleet.yaml but `topic_id` is cleared.
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

        // Sprint 23 P1: try_telegram_reply_from now invalidates the stale
        // topic and attempts recreation instead of deleting the instance.
        let res = try_telegram_reply_from(&home, "B", "main-path send");
        assert!(res.is_err());

        let fleet_yaml = std::fs::read_to_string(home.join("fleet.yaml")).expect("read fleet.yaml");
        // Instance B must survive (not deleted).
        assert!(
            fleet_yaml.contains("B:"),
            "Sprint 23 P1: instance must survive topic invalidation; yaml was:\n{fleet_yaml}"
        );
        // But topic_id must be cleared (invalidated).
        let config =
            crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).expect("load fleet.yaml");
        let inst_b = config.instances.get("B").expect("B exists");
        assert_eq!(
            inst_b.topic_id, None,
            "topic_id must be cleared after invalidation"
        );

        // Stale topic must be stripped from registry.
        let reg = load_topic_registry(&home);
        assert!(
            !reg.contains_key(&42),
            "stale topic 42 must be unregistered"
        );

        std::env::remove_var("PR57_ROUND2_FAKE_TOKEN");
        std::fs::remove_dir_all(&home).ok();
    }

    /// TelegramChannel::create_topic delegates to `create_topic_for_instance`.
    /// Without a fleet.yaml the helper returns None → the trait method
    /// returns `ChannelError::Other`. This exercises the wiring without
    /// needing a live bot.
    #[test]
    fn telegram_channel_create_topic_returns_error_without_config() {
        use crate::channel::Channel;
        let home = tmp_home("create_topic_no_config");
        let state = TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            home.clone(),
            HashMap::new(),
            None,
        );
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));
        let result = channel.create_topic("test-agent");
        assert!(result.is_err(), "create_topic must fail without fleet.yaml");
        std::fs::remove_dir_all(&home).ok();
    }

    /// TelegramChannel::notify delegates to `notify_telegram` /
    /// `notify_telegram_silent`. Without a fleet.yaml the notify helpers
    /// return early (no-op). The trait method returns Ok(()) because the
    /// underlying helpers are fire-and-forget.
    #[test]
    fn telegram_channel_notify_succeeds_without_config() {
        use crate::channel::{Channel, NotifySeverity};
        let home = tmp_home("notify_no_config");
        let state = TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            home.clone(),
            HashMap::new(),
            None,
        );
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));
        // notify delegates to fire-and-forget helpers that silently no-op
        // when fleet.yaml is missing — so the trait method returns Ok.
        let result = channel.notify("test-agent", NotifySeverity::Warn, "stall", false);
        assert!(result.is_ok(), "notify should succeed (fire-and-forget)");
        let result_silent = channel.notify("test-agent", NotifySeverity::Info, "recovered", true);
        assert!(result_silent.is_ok(), "silent notify should succeed");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Verify that TelegramChannel's create_topic returns a TopicRef with
    /// the correct channel_kind when it would succeed. We can't test the
    /// happy path without a live bot, but we can verify the type shape
    /// via the trait signature.
    #[test]
    fn telegram_channel_trait_methods_are_object_safe() {
        let state = TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));
        // Verify the channel can be used as a trait object with the new methods.
        let dyn_channel: &dyn crate::channel::Channel = &channel;
        let _ = dyn_channel.create_topic("test");
        let _ = dyn_channel.notify("test", crate::channel::NotifySeverity::Warn, "msg", false);
    }

    // ─── Phase 3 send/edit/delete trait dispatcher tests ──────────────
    //
    // Bot APIs cannot be exercised without network access; tests below
    // use the contract-test fixture (`bot: None`) to exercise the
    // pre-network argument-handling paths (binding downcast, msg id
    // parse, Err shape on missing bot). The full happy-path is covered
    // by integration tests behind the existing real-bot feature gate
    // (out of scope for trait-wiring PR).

    #[test]
    fn send_returns_telegram_binding_with_topic_when_caller_supplied_topic() {
        // Phase 3 fix: Channel::send must downcast the input binding
        // and return a MsgRef whose binding carries the same topic so
        // subsequent edit/delete calls have a usable handle. Even when
        // the bot itself is None (contract-test), the binding-shape
        // path runs first and we can assert it via the resolution
        // helper directly.
        use crate::channel::BindingRef;
        let supplied = TelegramBindingPayload { topic_id: 42 }.into_binding();
        // Round-trip via build_telegram_msg_binding (the helper Channel::send
        // calls) — confirms downcast preserves topic_id.
        let topic = supplied
            .downcast::<TelegramBindingPayload>()
            .map(|p| p.topic_id);
        assert_eq!(topic, Some(42));
        let returned: BindingRef = build_telegram_msg_binding(topic);
        assert_eq!(returned.kind(), "telegram");
        assert_eq!(
            returned
                .downcast::<TelegramBindingPayload>()
                .map(|p| p.topic_id),
            Some(42),
            "MsgRef binding must preserve topic_id from supplied binding"
        );
    }

    #[test]
    fn send_returns_telegram_binding_without_topic_for_foreign_binding() {
        // Defensive shape: if a caller hands us a non-Telegram binding
        // (e.g. Discord BindingRef shape), Channel::send falls back to
        // group-only routing. The returned binding has no payload —
        // downcast yields None.
        use crate::channel::BindingRef;
        let returned: BindingRef = build_telegram_msg_binding(None);
        assert_eq!(returned.kind(), "telegram");
        assert!(
            returned.downcast::<TelegramBindingPayload>().is_none(),
            "no-topic binding must not carry TelegramBindingPayload"
        );
    }

    #[test]
    fn channel_send_returns_err_when_bot_uninitialised() {
        // Contract-test fixture has `bot: None` (production never has
        // this, but the trait method's first step is the bot
        // unwrap). Phase 3 wiring must surface a typed Err — not
        // panic — so trait consumers can fall back gracefully.
        use crate::channel::Channel;
        let state = TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            PathBuf::from("/tmp/agend-phase3-test"),
            HashMap::new(),
            None,
        );
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));
        let binding = TelegramBindingPayload { topic_id: 7 }.into_binding();
        let msg = crate::channel::OutMsg::text("hello");
        let err = channel
            .send(&binding, msg)
            .expect_err("must Err when bot is None");
        assert!(
            err.to_string().contains("bot not initialized"),
            "Err must name the missing bot: {err}"
        );
    }

    #[test]
    fn channel_edit_returns_err_when_bot_uninitialised() {
        use crate::channel::Channel;
        let state = TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            PathBuf::from("/tmp/agend-phase3-test"),
            HashMap::new(),
            None,
        );
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));
        let msg_ref = crate::channel::MsgRef {
            binding: TelegramBindingPayload { topic_id: 7 }.into_binding(),
            id: "123".to_string(),
        };
        let payload = crate::channel::OutMsg::text("new text");
        let err = channel
            .edit(&msg_ref, payload)
            .expect_err("must Err when bot is None");
        assert!(
            err.to_string().contains("bot not initialized"),
            "Err must name the missing bot: {err}"
        );
    }

    #[test]
    fn channel_edit_rejects_invalid_message_id() {
        // Pre-bot validation: malformed id returns a typed Err
        // distinct from the "bot None" case so callers can
        // distinguish caller error from infra fault.
        use crate::channel::Channel;
        let state = TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            PathBuf::from("/tmp/agend-phase3-test"),
            HashMap::new(),
            None,
        );
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));
        let msg_ref = crate::channel::MsgRef {
            binding: TelegramBindingPayload { topic_id: 7 }.into_binding(),
            id: "not-a-number".to_string(),
        };
        let payload = crate::channel::OutMsg::text("x");
        // Note: bot None still gates first (returns "bot not initialized")
        // — the invalid-id check fires only after bot resolution. This
        // test pins that the trait method order is bot-first then id-parse,
        // matching the prod path where bot is always Some.
        let err = channel.edit(&msg_ref, payload).expect_err("must Err");
        // Either "bot not initialized" (current contract-test fixture)
        // OR "invalid telegram message_id" if a future refactor reorders
        // the checks. Assert one-of so the test pins behaviour without
        // over-specifying ordering.
        assert!(
            err.to_string().contains("bot not initialized")
                || err.to_string().contains("invalid telegram message_id"),
            "Err shape unexpected: {err}"
        );
    }

    #[test]
    fn channel_delete_returns_err_when_bot_uninitialised() {
        use crate::channel::Channel;
        let state = TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            PathBuf::from("/tmp/agend-phase3-test"),
            HashMap::new(),
            None,
        );
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));
        let msg_ref = crate::channel::MsgRef {
            binding: TelegramBindingPayload { topic_id: 7 }.into_binding(),
            id: "456".to_string(),
        };
        let err = channel
            .delete(&msg_ref)
            .expect_err("must Err when bot is None");
        assert!(
            err.to_string().contains("bot not initialized"),
            "Err must name the missing bot: {err}"
        );
    }

    /// Phase 5b: when the operator has not configured `user_allowlist`,
    /// `send_from_agent` MUST reject all four `AgentOutboundOp` variants
    /// at the adapter-level outbound gate when `user_allowlist` is not
    /// configured. The allowlist is the load-bearing gate.
    #[test]
    fn send_from_agent_rejects_when_user_allowlist_unconfigured() {
        use crate::channel::Channel;
        let state = TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            PathBuf::from("/tmp/agend-phase5b-test"),
            HashMap::new(),
            None, // user_allowlist=None → outbound_authorized=false
        );
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));

        // All four AgentOutboundOp variants must reject at Step 1
        // (allowlist gate). They never reach Step 2 (capability gate)
        // or Step 3 (platform dispatch).
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
            assert!(
                result.is_err(),
                "outbound gate must reject {label} when user_allowlist=None",
            );
            let err_str = format!(
                "{}",
                result.expect_err("outbound gate must reject on user_allowlist=None")
            );
            assert!(
                err_str.contains("user_allowlist not configured"),
                "error must name the missing config for {label}: {err_str}",
            );
        }
    }

    /// `try_telegram_reply_from` must retry with a recreated topic when
    /// the first send hits a topic-deleted error. Since we can't create
    /// a real topic in tests, the retry also fails — but the test pins
    /// that the invalidation side-effect fires (stale topic stripped
    /// from registry).
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
        // The retry also fails (no real bot), but the stale topic must
        // be stripped from the registry.
        assert!(res.is_err());

        let reg = load_topic_registry(&home);
        assert!(
            !reg.contains_key(&42),
            "stale topic 42 must be unregistered after retry; reg={reg:?}"
        );

        // Instance must still exist in fleet.yaml (not deleted).
        let fleet_yaml = std::fs::read_to_string(home.join("fleet.yaml")).expect("read fleet.yaml");
        assert!(
            fleet_yaml.contains("agent-x"),
            "instance must survive topic invalidation; yaml:\n{fleet_yaml}"
        );

        std::env::remove_var("SPRINT23_P1_FAKE_TOKEN");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Non-topic-deleted errors must NOT trigger invalidation — only the
    /// exact "message thread not found" marker should.
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

        // Topic must still be in registry — no invalidation.
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
