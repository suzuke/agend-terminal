//! Telegram adapter — runs in dedicated thread with tokio runtime.
//!
//! Inbound: Telegram message → inbox + PTY notification
//! Outbound: reply(text) → Telegram send_message to topic

use crate::fleet::ChannelConfig;
use crate::inbox::{self, InboxMessage};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use teloxide::prelude::*;
use teloxide::types::{MessageId, ThreadId};

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
}

impl TelegramState {
    pub fn new(
        token: &str,
        group_id: i64,
        topic_map: HashMap<String, i32>,
        home: PathBuf,
        submit_keys: HashMap<String, String>,
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
                eprintln!("[telegram] failed to build tokio runtime");
                return;
            };
            rt.block_on(async {
                let bot = state.lock().unwrap_or_else(|e| e.into_inner()).bot.clone();
                let state2 = Arc::clone(&state);
                let handler = Update::filter_message().endpoint(move |_bot: Bot, msg: Message| {
                    let state = Arc::clone(&state2);
                    async move {
                        handle_message(&state, &msg);
                        respond(())
                    }
                });
                eprintln!("[telegram] polling started");
                Dispatcher::builder(bot, handler).build().dispatch().await;
            });
        })
    {
        eprintln!("[telegram] failed to spawn polling thread: {e}");
    }
}

fn handle_message(state: &Arc<Mutex<TelegramState>>, msg: &Message) {
    // Detect topic closure
    if msg.forum_topic_closed().is_some() {
        eprintln!("[telegram] topic closed");
        return;
    }

    let text = match msg.text() {
        Some(t) => t,
        None => return,
    };

    let username = msg
        .from
        .as_ref()
        .and_then(|u| u.username.as_deref())
        .unwrap_or("unknown");

    let thread_id = msg.thread_id.map(|ThreadId(MessageId(id))| id);

    let (instance_name, home, submit_key) = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        let name = thread_id
            .and_then(|tid| s.topic_to_instance.get(&tid).cloned())
            .unwrap_or_else(|| "general".to_string());
        let sk = s
            .submit_keys
            .get(&name)
            .cloned()
            .unwrap_or_else(|| "\r".to_string());
        (name, s.home.clone(), sk)
    };

    eprintln!("[telegram] {username} → {instance_name}: {text}");

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
        &format!("user:{username} via telegram"),
        text,
        &submit_key,
    );
}

/// Send a reply from an agent to Telegram (called from MCP reply tool).
#[allow(dead_code)]
pub fn send_reply(
    state: &Arc<Mutex<TelegramState>>,
    instance_name: &str,
    text: &str,
) -> anyhow::Result<()> {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let (bot, group_id, topic_id) = (
        s.bot.clone(),
        s.group_id,
        s.instance_to_topic.get(instance_name).copied(),
    );
    drop(s);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(send_with_topic(&bot, group_id, topic_id, text))
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
        ..
    } = config.channel.as_ref()?;
    let token = match std::env::var(bot_token_env) {
        Ok(t) => t,
        Err(_) => {
            eprintln!("[telegram] bot token env '{bot_token_env}' not set, skipping");
            return None;
        }
    };

    let mut topic_map: HashMap<String, i32> = config
        .instances
        .iter()
        .filter_map(|(name, inst)| inst.topic_id.map(|tid| (name.clone(), tid)))
        .collect();

    // Auto-create topics for instances without topic_id
    let bot = teloxide::Bot::new(&token);
    let chat_id = teloxide::types::ChatId(*group_id);
    for (name, inst) in &config.instances {
        if name == "general" && inst.topic_id.is_none() {
            topic_map.insert("general".to_string(), 1);
        } else if inst.topic_id.is_none() {
            eprintln!("[telegram] creating topic for '{name}'...");
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                match rt
                    .block_on(async { bot.create_forum_topic(chat_id, name, 0x6FB9F0, "").await })
                {
                    Ok(topic) => {
                        let tid = topic.thread_id.0 .0;
                        eprintln!("[telegram] created topic '{name}' → {tid}");
                        topic_map.insert(name.clone(), tid);
                    }
                    Err(e) => eprintln!("[telegram] failed to create topic for '{name}': {e}"),
                }
            }
        }
    }

    // Write back topic_ids
    if home.join("fleet.yaml").exists() && !topic_map.is_empty() {
        for (name, tid) in &topic_map {
            let _ = crate::fleet::update_instance_field(
                home,
                name,
                "topic_id",
                serde_yaml::Value::Number(serde_yaml::Number::from(*tid)),
            );
        }
        eprintln!("[telegram] updated fleet.yaml with topic_ids");
    }

    let state = Arc::new(Mutex::new(TelegramState::new(
        &token,
        *group_id,
        topic_map,
        home.to_path_buf(),
        submit_keys,
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
        let s = self.lock().unwrap_or_else(|e| e.into_inner());
        let (bot, group_id, topic_id) = (
            s.bot.clone(),
            s.group_id,
            s.instance_to_topic.get(instance_name).copied(),
        );
        drop(s);
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return crate::channel::SendResult::Failed("tokio build failed".into());
        };
        match rt.block_on(send_with_topic(&bot, group_id, topic_id, text)) {
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
        let s = self.lock().unwrap_or_else(|e| e.into_inner());
        let (bot, group_id) = (s.bot.clone(), s.group_id);
        drop(s);
        let Ok(mid) = message_id.parse::<i32>() else {
            return crate::channel::SendResult::Failed(format!("invalid message_id: {message_id}"));
        };
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return crate::channel::SendResult::Failed("tokio build failed".into());
        };
        match rt.block_on(async {
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
