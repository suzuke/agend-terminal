//! Telegram adapter — runs in dedicated thread with tokio runtime.
//!
//! Inbound: Telegram message → inbox + PTY notification
//! Outbound: reply(text) → Telegram send_message to topic

use crate::fleet::ChannelConfig;
use crate::inbox::{self, InboxMessage};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{MessageId, ThreadId};

/// Lock TelegramState, recovering from poison.
/// Mutex poisoning can occur if a background thread panics while holding the lock.
/// We recover rather than propagate the panic to keep the TUI responsive.
pub(crate) fn lock_state(
    tg: &Arc<Mutex<TelegramState>>,
) -> std::sync::MutexGuard<'_, TelegramState> {
    tg.lock().unwrap_or_else(|e| {
        tracing::warn!("TelegramState mutex poisoned, recovering");
        e.into_inner()
    })
}

// ---------------------------------------------------------------------------
// Topic registry — persists topic_id → instance_name in $AGEND_HOME/topics.json
// so we can detect orphaned topics on daemon restart.
// ---------------------------------------------------------------------------

fn topic_registry_path(home: &Path) -> PathBuf {
    home.join("topics.json")
}

fn load_topic_registry(home: &Path) -> HashMap<i32, String> {
    let path = topic_registry_path(home);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<HashMap<String, String>>(&s).ok())
        .map(|m| {
            m.into_iter()
                .filter_map(|(k, v)| k.parse::<i32>().ok().map(|id| (id, v)))
                .collect()
        })
        .unwrap_or_default()
}

fn save_topic_registry(home: &Path, registry: &HashMap<i32, String>) {
    let map: HashMap<String, &String> = registry.iter().map(|(k, v)| (k.to_string(), v)).collect();
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(topic_registry_path(home), json);
    }
}

fn register_topic(home: &Path, topic_id: i32, instance_name: &str) {
    let mut reg = load_topic_registry(home);
    reg.insert(topic_id, instance_name.to_string());
    save_topic_registry(home, &reg);
}

fn unregister_topic(home: &Path, topic_id: i32) {
    let mut reg = load_topic_registry(home);
    reg.remove(&topic_id);
    save_topic_registry(home, &reg);
}

/// Shared tokio runtime for all Telegram sync→async calls.
fn telegram_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("telegram tokio runtime")
    })
}

pub struct TelegramState {
    pub bot: Bot,
    #[allow(dead_code)]
    pub group_id: ChatId,
    pub topic_to_instance: HashMap<i32, String>,
    #[allow(dead_code)]
    pub instance_to_topic: HashMap<String, i32>,
    pub home: PathBuf,
    /// Submit key per instance (for PTY notification injection).
    pub submit_keys: HashMap<String, String>,
    /// Allowlist of Telegram user IDs permitted to command the fleet.
    /// See [`crate::fleet::ChannelConfig::Telegram::user_allowlist`] for
    /// semantics of `None` vs `Some(empty)` vs `Some([...])`.
    pub user_allowlist: Option<Vec<i64>>,
}

impl TelegramState {
    pub fn new(
        token: &str,
        group_id: i64,
        topic_map: HashMap<String, i32>,
        home: PathBuf,
        submit_keys: HashMap<String, String>,
        user_allowlist: Option<Vec<i64>>,
    ) -> Self {
        let topic_to_instance: HashMap<i32, String> = topic_map
            .iter()
            .map(|(name, &tid)| (tid, name.clone()))
            .collect();
        Self {
            bot: Bot::new(token),
            group_id: ChatId(group_id),
            topic_to_instance,
            instance_to_topic: topic_map,
            home,
            submit_keys,
            user_allowlist,
        }
    }

    /// Return true if a sender is permitted by the allowlist.
    /// `None` allowlist = accept (legacy). `Some` = must appear in the list.
    pub fn is_user_allowed(&self, user_id: i64) -> bool {
        match &self.user_allowlist {
            None => true,
            Some(list) => list.contains(&user_id),
        }
    }

    /// Send a message to an instance's Telegram topic.
    #[allow(dead_code)]
    pub async fn send_to_topic(&self, instance_name: &str, text: &str) -> anyhow::Result<()> {
        let topic_id = self
            .instance_to_topic
            .get(instance_name)
            .ok_or_else(|| anyhow::anyhow!("No topic for '{instance_name}'"))?;
        send_with_topic(&self.bot, self.group_id, Some(*topic_id), text).await
    }
}

/// Send a message, optionally to a topic.
async fn send_with_topic(
    bot: &Bot,
    chat_id: ChatId,
    topic_id: Option<i32>,
    text: &str,
) -> anyhow::Result<()> {
    use teloxide::payloads::SendMessageSetters;
    use teloxide::prelude::Requester;
    match topic_id {
        Some(tid) if tid != 1 => {
            bot.send_message(chat_id, text)
                .message_thread_id(ThreadId(MessageId(tid)))
                .await?;
        }
        _ => {
            bot.send_message(chat_id, text).await?;
        }
    }
    Ok(())
}

/// Start Telegram polling in a dedicated thread with its own tokio runtime.
pub fn start_polling(state: Arc<Mutex<TelegramState>>) {
    if let Err(e) = std::thread::Builder::new()
        .name("telegram".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                tracing::error!("failed to build tokio runtime");
                return;
            };
            rt.block_on(async {
                let bot = lock_state(&state).bot.clone();
                let state2 = Arc::clone(&state);
                let handler = Update::filter_message().endpoint(move |_bot: Bot, msg: Message| {
                    let state = Arc::clone(&state2);
                    async move {
                        handle_message(&state, &msg);
                        respond(())
                    }
                });
                tracing::info!("polling started");
                Dispatcher::builder(bot, handler).build().dispatch().await;
            });
        })
    {
        tracing::error!(error = %e, "failed to spawn polling thread");
    }
}

/// Resolve a topic_id to an instance name.
/// First checks the in-memory map, then reloads from fleet.yaml for
/// runtime-created topics (via create_instance).
fn resolve_topic(state: &mut TelegramState, topic_id: Option<i32>) -> String {
    if let Some(tid) = topic_id {
        if let Some(name) = state.topic_to_instance.get(&tid).cloned() {
            return name;
        }
        // Unknown topic_id — reload from fleet.yaml
        if let Ok(config) = crate::fleet::FleetConfig::load(&state.home.join("fleet.yaml")) {
            for (inst_name, inst) in &config.instances {
                if inst.topic_id == Some(tid) {
                    state.topic_to_instance.insert(tid, inst_name.clone());
                    state.instance_to_topic.insert(inst_name.clone(), tid);
                    return inst_name.clone();
                }
            }
        }
    }
    "general".to_string()
}

fn handle_message(state: &Arc<Mutex<TelegramState>>, msg: &Message) {
    // Detect topic closure/deletion — auto-delete the corresponding instance
    if msg.forum_topic_closed().is_some() {
        let thread_id = msg.thread_id.map(|ThreadId(MessageId(id))| id);
        if let Some(tid) = thread_id {
            let mut s = lock_state(state);
            if let Some(instance_name) = s.topic_to_instance.remove(&tid) {
                s.instance_to_topic.remove(&instance_name);
                let home = s.home.clone();
                drop(s);
                tracing::info!(topic_id = tid, instance = %instance_name, "topic closed, deleting instance");
                // Kill + remove via API
                let _ = crate::api::call(
                    &home,
                    &serde_json::json!({"method": crate::api::method::DELETE, "params": {"name": instance_name}}),
                );
                // Remove from fleet.yaml
                if let Err(e) = crate::fleet::remove_instance_from_yaml(&home, &instance_name) {
                    tracing::warn!(instance = %instance_name, error = %e, "failed to remove from fleet.yaml");
                }
                return;
            }
        }
        tracing::warn!("topic closed (no matching instance)");
        return;
    }

    let text = match msg.text() {
        Some(t) => t,
        None => return,
    };

    let sender_id: Option<i64> = msg.from.as_ref().map(|u| u.id.0 as i64);
    let username = msg
        .from
        .as_ref()
        .and_then(|u| u.username.as_deref())
        .unwrap_or("unknown");

    // Authz: drop messages from senders not on the allowlist. Legacy
    // deployments (user_allowlist = None) accept all; `Some([])` rejects
    // all; `Some([...])` restricts to the listed IDs.
    {
        let s = lock_state(state);
        let allowed = match sender_id {
            Some(id) => s.is_user_allowed(id),
            None => s.user_allowlist.is_none(),
        };
        if !allowed {
            tracing::warn!(
                from = username,
                user_id = ?sender_id,
                "telegram message rejected by user_allowlist"
            );
            return;
        }
    }

    let thread_id = msg.thread_id.map(|ThreadId(MessageId(id))| id);

    let (instance_name, home, submit_key) = {
        let mut s = lock_state(state);
        let name = resolve_topic(&mut s, thread_id);
        let sk = s
            .submit_keys
            .get(&name)
            .cloned()
            .unwrap_or_else(|| "\r".to_string());
        (name, s.home.clone(), sk)
    };

    tracing::info!(from = username, to = %instance_name, %text, "inbound message");

    // Route based on agent state: when blocked on an unexpected startup
    // prompt (AwaitingOperator), the operator's reply must reach the PTY
    // as raw keystrokes — any inbox prefix ("[telegram:@user] …") would
    // confuse the CLI's prompt parser. In every other state, preserve the
    // existing inbox semantics so agent-authored message handling keeps
    // working.
    if agent_is_awaiting_operator(&home, &instance_name) {
        let payload = format!("{text}\n");
        match crate::api::call(
            &home,
            &serde_json::json!({
                "method": crate::api::method::INJECT,
                "params": {"name": instance_name, "data": payload, "raw": true}
            }),
        ) {
            Ok(_) => tracing::info!(
                to = %instance_name,
                bytes = payload.len(),
                "routed raw keystrokes (awaiting_operator)"
            ),
            Err(e) => tracing::warn!(
                to = %instance_name,
                error = %e,
                "raw injection failed"
            ),
        }
        return;
    }

    // Enqueue in inbox
    let msg_obj = InboxMessage {
        from: format!("user:{username}"),
        text: text.to_string(),
        kind: Some("telegram".to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    let _ = inbox::enqueue(&home, &instance_name, msg_obj);

    // Notify agent PTY
    inbox::notify_agent(
        &home,
        &instance_name,
        &inbox::NotifySource::Telegram(username),
        text,
        &submit_key,
    );
}

/// Query the daemon for the current `agent_state` of one instance.
/// Returns true only when the daemon reports `"awaiting_operator"`.
/// Any error (daemon down, agent missing, parse failure) returns false so
/// we fall through to normal inbox routing rather than silently dropping
/// messages.
fn agent_is_awaiting_operator(home: &Path, instance_name: &str) -> bool {
    let resp = match crate::api::call(
        home,
        &serde_json::json!({"method": crate::api::method::LIST}),
    ) {
        Ok(v) => v,
        Err(_) => return false,
    };
    list_response_is_awaiting(&resp, instance_name)
}

/// Pure JSON-inspection half of [`agent_is_awaiting_operator`]. Separated
/// out so the routing logic can be unit-tested without a running daemon.
fn list_response_is_awaiting(resp: &serde_json::Value, instance_name: &str) -> bool {
    resp["result"]["agents"]
        .as_array()
        .and_then(|arr| {
            arr.iter().find(|a| a["name"].as_str() == Some(instance_name))
        })
        .and_then(|a| a["agent_state"].as_str())
        .map(|s| s == crate::state::AgentState::AwaitingOperator.display_name())
        .unwrap_or(false)
}

/// Send a reply from an agent to Telegram (called from MCP reply tool).
#[allow(dead_code)]
pub fn send_reply(
    state: &Arc<Mutex<TelegramState>>,
    instance_name: &str,
    text: &str,
) -> anyhow::Result<()> {
    let s = lock_state(state);
    let (bot, group_id, topic_id) = (
        s.bot.clone(),
        s.group_id,
        s.instance_to_topic.get(instance_name).copied(),
    );
    drop(s);
    telegram_runtime().block_on(send_with_topic(&bot, group_id, topic_id, text))
}

/// Initialize Telegram from fleet config.
pub fn init_from_config(
    config: &crate::fleet::FleetConfig,
    home: &Path,
    submit_keys: HashMap<String, String>,
) -> Option<Arc<Mutex<TelegramState>>> {
    let ChannelConfig::Telegram {
        bot_token_env,
        group_id,
        user_allowlist,
        ..
    } = config.channel.as_ref()?;
    let token = match std::env::var(bot_token_env) {
        Ok(t) => t,
        Err(_) => {
            tracing::info!(env = %bot_token_env, "bot token env not set, skipping");
            return None;
        }
    };
    match user_allowlist {
        None => tracing::warn!(
            "telegram channel.user_allowlist is not set — any group member can command the fleet. \
             Set `user_allowlist: [123, 456]` in fleet.yaml to lock this down."
        ),
        Some(list) if list.is_empty() => {
            tracing::info!("telegram channel.user_allowlist is empty — all inbound messages will be rejected")
        }
        Some(list) => tracing::info!(count = list.len(), "telegram user_allowlist active"),
    }
    let allowlist = user_allowlist.clone();

    // Clean up orphaned topics: exist in registry but not in fleet.yaml
    let mut reg = load_topic_registry(home);
    let instance_names: std::collections::HashSet<&String> = config.instances.keys().collect();
    let mut orphan_count = 0;
    for (tid, inst_name) in reg.clone() {
        if tid != 1 && !instance_names.contains(&inst_name) {
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

    // Auto-create topics for instances without topic_id
    for (name, inst) in &config.instances {
        if name == "general" && inst.topic_id.is_none() {
            topic_map.insert("general".to_string(), 1);
        } else if inst.topic_id.is_none() {
            tracing::info!(instance = %name, "creating topic");
            match telegram_runtime()
                .block_on(async { bot.create_forum_topic(chat_id, name, 0x6FB9F0, "").await })
            {
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
                serde_yaml::Value::Number(serde_yaml::Number::from(*tid)),
            );
            reg.insert(*tid, name.clone());
        }
        save_topic_registry(home, &reg);
        tracing::info!("updated fleet.yaml with topic_ids");
    }

    let state = Arc::new(Mutex::new(TelegramState::new(
        &token,
        *group_id,
        topic_map,
        home.to_path_buf(),
        submit_keys,
        allowlist,
    )));
    start_polling(Arc::clone(&state));
    Some(state)
}

/// ChannelAdapter implementation for Telegram.
impl crate::channel::ChannelAdapter for Arc<Mutex<TelegramState>> {
    fn name(&self) -> &str {
        "telegram"
    }

    fn send_reply(&self, instance_name: &str, text: &str) -> crate::channel::SendResult {
        let s = lock_state(self);
        let (bot, group_id, topic_id) = (
            s.bot.clone(),
            s.group_id,
            s.instance_to_topic.get(instance_name).copied(),
        );
        drop(s);
        match telegram_runtime().block_on(send_with_topic(&bot, group_id, topic_id, text)) {
            Ok(()) => crate::channel::SendResult::Sent,
            Err(e) => crate::channel::SendResult::Failed(format!("{e}")),
        }
    }

    fn react(&self, _instance_name: &str, _emoji: &str) -> crate::channel::SendResult {
        // React requires message_id which we don't have in this context
        crate::channel::SendResult::Failed("react via adapter needs message_id context".into())
    }

    fn edit_message(
        &self,
        _instance_name: &str,
        message_id: &str,
        text: &str,
    ) -> crate::channel::SendResult {
        let s = lock_state(self);
        let (bot, group_id) = (s.bot.clone(), s.group_id);
        drop(s);
        let Ok(mid) = message_id.parse::<i32>() else {
            return crate::channel::SendResult::Failed(format!("invalid message_id: {message_id}"));
        };
        match telegram_runtime().block_on(async {
            bot.edit_message_text(group_id, MessageId(mid), text)
                .await
                .map(|_| ())
        }) {
            Ok(()) => crate::channel::SendResult::Sent,
            Err(e) => crate::channel::SendResult::Failed(format!("{e}")),
        }
    }

    fn start_polling(&self, _home: &std::path::Path) {
        // Already started via init_from_config
    }

    fn stop(&self) {
        // Polling thread exits when daemon exits
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
struct TelegramChannel {
    token: String,
    group_id: i64,
}

fn resolve_channel() -> anyhow::Result<(TelegramChannel, crate::fleet::FleetConfig)> {
    let home = crate::home_dir();
    let config = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))?;
    match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram {
            bot_token_env,
            group_id,
            ..
        }) => {
            let token = std::env::var(bot_token_env)
                .map_err(|_| anyhow::anyhow!("bot token env '{bot_token_env}' not set"))?;
            Ok((
                TelegramChannel {
                    token,
                    group_id: *group_id,
                },
                config,
            ))
        }
        None => anyhow::bail!("No Telegram channel configured"),
    }
}

fn resolve_channel_only() -> anyhow::Result<TelegramChannel> {
    resolve_channel().map(|(ch, _)| ch)
}

/// Send a reply from an instance to its Telegram topic. Returns (message_id, chat_id).
pub fn try_telegram_reply(instance_name: &str, text: &str) -> anyhow::Result<(i32, i64)> {
    let (ch, config) = resolve_channel()?;
    let topic_id = config
        .instances
        .get(instance_name)
        .and_then(|inst| inst.topic_id);
    let msg_id = telegram_runtime().block_on(async {
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
    })?;
    Ok((msg_id, ch.group_id))
}

/// React to a message with an emoji.
pub fn try_telegram_react(
    instance_name: &str,
    emoji: &str,
    message_id: Option<&str>,
) -> anyhow::Result<()> {
    let ch = resolve_channel_only()?;
    let home = crate::home_dir();
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
    let emoji_char = map_emoji_name(emoji);
    telegram_runtime().block_on(async {
        let bot = teloxide::Bot::new(&ch.token);
        let chat_id = teloxide::types::ChatId(ch.group_id);
        let msg_id = teloxide::types::MessageId(mid);
        let reaction = teloxide::types::ReactionType::Emoji {
            emoji: emoji_char.to_string(),
        };
        bot.set_message_reaction(chat_id, msg_id)
            .reaction(vec![reaction])
            .await?;
        Ok::<(), anyhow::Error>(())
    })
}

/// Edit a previously sent message.
pub fn try_telegram_edit(_instance_name: &str, message_id: &str, text: &str) -> anyhow::Result<()> {
    let ch = resolve_channel_only()?;
    let mid: i32 = message_id
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid message_id: {message_id}"))?;
    telegram_runtime().block_on(async {
        let bot = teloxide::Bot::new(&ch.token);
        bot.edit_message_text(
            teloxide::types::ChatId(ch.group_id),
            teloxide::types::MessageId(mid),
            text,
        )
        .await?;
        Ok::<(), anyhow::Error>(())
    })
}

/// Create a forum topic for a new instance.
pub fn create_topic_for_instance(home: &std::path::Path, instance_name: &str) -> Option<i32> {
    let ch = resolve_channel_only().ok()?;
    match telegram_runtime().block_on(async {
        let bot = teloxide::Bot::new(&ch.token);
        let topic = bot
            .create_forum_topic(
                teloxide::types::ChatId(ch.group_id),
                instance_name,
                0x6FB9F0,
                "",
            )
            .await?;
        Ok::<i32, anyhow::Error>(topic.thread_id.0 .0)
    }) {
        Ok(tid) => {
            tracing::info!(instance = %instance_name, topic_id = tid, "created topic");
            let _ = crate::fleet::update_instance_field(
                home,
                instance_name,
                "topic_id",
                serde_yaml::Value::Number(serde_yaml::Number::from(tid)),
            );
            register_topic(home, tid, instance_name);
            Some(tid)
        }
        Err(e) => {
            tracing::error!(instance = %instance_name, error = %e, "failed to create topic");
            None
        }
    }
}

/// Delete a forum topic.
pub fn delete_topic(home: &std::path::Path, topic_id: i32) {
    let ch = match resolve_channel_only() {
        Ok(c) => c,
        Err(_) => return,
    };
    let tid = teloxide::types::ThreadId(teloxide::types::MessageId(topic_id));
    let _ = telegram_runtime().block_on(async {
        let bot = teloxide::Bot::new(&ch.token);
        let chat_id = teloxide::types::ChatId(ch.group_id);
        let _ = bot.close_forum_topic(chat_id, tid).await;
        bot.delete_forum_topic(chat_id, tid).await
    });
    unregister_topic(home, topic_id);
    tracing::info!(topic_id, "deleted topic");
}

/// Download an attachment by file_id.
pub fn try_download_attachment(instance_name: &str, file_id: &str) -> anyhow::Result<String> {
    let ch = resolve_channel_only()?;
    let home = crate::home_dir();
    telegram_runtime().block_on(async {
        let bot = teloxide::Bot::new(&ch.token);
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_state_new_builds_reverse_map() {
        let mut topic_map = HashMap::new();
        topic_map.insert("agent1".to_string(), 100);
        topic_map.insert("agent2".to_string(), 200);
        let state = TelegramState::new(
            "fake-token",
            -12345,
            topic_map,
            PathBuf::from("/tmp/test"),
            HashMap::new(),
            None,
        );
        assert_eq!(
            state.topic_to_instance.get(&100),
            Some(&"agent1".to_string())
        );
        assert_eq!(
            state.topic_to_instance.get(&200),
            Some(&"agent2".to_string())
        );
        assert_eq!(state.instance_to_topic.get("agent1"), Some(&100));
        assert_eq!(state.instance_to_topic.get("agent2"), Some(&200));
    }

    #[test]
    fn telegram_state_empty_topic_map() {
        let state = TelegramState::new(
            "fake-token",
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        assert!(state.topic_to_instance.is_empty());
        assert!(state.instance_to_topic.is_empty());
    }

    #[test]
    fn telegram_state_submit_keys_preserved() {
        let mut keys = HashMap::new();
        keys.insert("agent1".to_string(), "\n".to_string());
        let state =
            TelegramState::new("tok", -1, HashMap::new(), PathBuf::from("/tmp"), keys, None);
        assert_eq!(state.submit_keys.get("agent1"), Some(&"\n".to_string()));
    }

    #[test]
    fn is_user_allowed_none_means_open() {
        let state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        // Legacy open mode: any id accepted.
        assert!(state.is_user_allowed(1));
        assert!(state.is_user_allowed(i64::MAX));
    }

    #[test]
    fn is_user_allowed_empty_rejects_all() {
        let state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            Some(vec![]),
        );
        assert!(!state.is_user_allowed(1));
        assert!(!state.is_user_allowed(0));
    }

    #[test]
    fn is_user_allowed_restricts_to_list() {
        let state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            Some(vec![42, 100]),
        );
        assert!(state.is_user_allowed(42));
        assert!(state.is_user_allowed(100));
        assert!(!state.is_user_allowed(41));
        assert!(!state.is_user_allowed(0));
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

    #[test]
    fn resolve_topic_known_topic() {
        let mut topic_map = HashMap::new();
        topic_map.insert("alice".to_string(), 100);
        let mut state = TelegramState::new(
            "tok",
            -1,
            topic_map,
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        assert_eq!(resolve_topic(&mut state, Some(100)), "alice");
    }

    #[test]
    fn resolve_topic_none_returns_general() {
        let mut state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        assert_eq!(resolve_topic(&mut state, None), "general");
    }

    #[test]
    fn resolve_topic_unknown_falls_back_to_general() {
        let mut state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            PathBuf::from("/tmp/nonexistent"),
            HashMap::new(),
            None,
        );
        // No fleet.yaml → falls back to general
        assert_eq!(resolve_topic(&mut state, Some(999)), "general");
    }

    #[test]
    fn resolve_topic_reloads_from_fleet_yaml() {
        let home = tmp_home("resolve_reload");
        let yaml = r#"defaults:
  backend: claude
instances:
  alice:
    role: "Test"
    topic_id: 229
  general:
    role: "General"
    topic_id: 1
"#;
        std::fs::write(home.join("fleet.yaml"), yaml).ok();

        // State has NO topic mappings — simulates runtime-created topic
        let mut state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            home.clone(),
            HashMap::new(),
            None,
        );

        // Should reload from fleet.yaml and find alice
        assert_eq!(resolve_topic(&mut state, Some(229)), "alice");
        // Should be cached now
        assert_eq!(
            state.topic_to_instance.get(&229),
            Some(&"alice".to_string())
        );
        assert_eq!(state.instance_to_topic.get("alice"), Some(&229));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_topic_reload_caches_for_next_call() {
        let home = tmp_home("resolve_cache");
        let yaml = r#"instances:
  bob:
    topic_id: 500
"#;
        std::fs::write(home.join("fleet.yaml"), yaml).ok();
        let mut state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            home.clone(),
            HashMap::new(),
            None,
        );

        // First call: reloads from fleet.yaml
        assert_eq!(resolve_topic(&mut state, Some(500)), "bob");
        // Delete fleet.yaml — second call should use cached map
        std::fs::remove_file(home.join("fleet.yaml")).ok();
        assert_eq!(resolve_topic(&mut state, Some(500)), "bob");

        std::fs::remove_dir_all(&home).ok();
    }

    // --- Topic registry tests ---

    #[test]
    fn topic_registry_roundtrip() {
        let home = tmp_home("registry_roundtrip");
        assert!(load_topic_registry(&home).is_empty());

        register_topic(&home, 100, "alice");
        register_topic(&home, 200, "bob");

        let reg = load_topic_registry(&home);
        assert_eq!(reg.get(&100), Some(&"alice".to_string()));
        assert_eq!(reg.get(&200), Some(&"bob".to_string()));

        unregister_topic(&home, 100);
        let reg = load_topic_registry(&home);
        assert!(!reg.contains_key(&100));
        assert_eq!(reg.get(&200), Some(&"bob".to_string()));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn topic_registry_overwrite() {
        let home = tmp_home("registry_overwrite");
        register_topic(&home, 100, "alice");
        register_topic(&home, 100, "bob");

        let reg = load_topic_registry(&home);
        assert_eq!(reg.get(&100), Some(&"bob".to_string()));
        assert_eq!(reg.len(), 1);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn topic_registry_missing_file() {
        let home = PathBuf::from("/tmp/agend-test-nonexistent-dir-12345");
        let reg = load_topic_registry(&home);
        assert!(reg.is_empty());
    }

    #[test]
    fn list_response_is_awaiting_detects_target_state() {
        let resp = serde_json::json!({
            "ok": true,
            "result": {
                "agents": [
                    {"name": "alice", "agent_state": "ready"},
                    {"name": "bob",   "agent_state": "awaiting_operator"},
                    {"name": "carol", "agent_state": "starting"},
                ]
            }
        });
        assert!(!list_response_is_awaiting(&resp, "alice"));
        assert!(list_response_is_awaiting(&resp, "bob"));
        assert!(!list_response_is_awaiting(&resp, "carol"));
        // Missing agent returns false (fall through to inbox path)
        assert!(!list_response_is_awaiting(&resp, "eve"));
    }

    #[test]
    fn list_response_is_awaiting_tolerates_malformed() {
        // Missing result.agents → false
        let r1 = serde_json::json!({"ok": false, "error": "daemon down"});
        assert!(!list_response_is_awaiting(&r1, "any"));
        // agents not an array → false
        let r2 = serde_json::json!({"result": {"agents": "nope"}});
        assert!(!list_response_is_awaiting(&r2, "any"));
        // agent without agent_state field → false
        let r3 = serde_json::json!({"result": {"agents": [{"name": "x"}]}});
        assert!(!list_response_is_awaiting(&r3, "x"));
    }
}
