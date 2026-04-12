//! Telegram adapter — runs in dedicated thread with tokio runtime.
//!
//! Inbound: Telegram message → inbox + PTY notification
//! Outbound: reply(text) → Telegram send_message to topic

use crate::fleet::ChannelConfig;
use crate::inbox::{self, InboxMessage};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use teloxide::prelude::*;
use teloxide::types::{MessageId, ThreadId};

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
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(instance_name) = s.topic_to_instance.remove(&tid) {
                s.instance_to_topic.remove(&instance_name);
                let home = s.home.clone();
                drop(s);
                eprintln!("[telegram] topic {tid} closed, deleting instance '{instance_name}'");
                // Kill + remove via API
                let _ = crate::api::call(
                    &home,
                    &serde_json::json!({"method": "delete", "params": {"name": instance_name}}),
                );
                // Remove from fleet.yaml
                if let Err(e) = crate::fleet::remove_instance_from_yaml(&home, &instance_name) {
                    eprintln!("[telegram] failed to remove '{instance_name}' from fleet.yaml: {e}");
                }
                return;
            }
        }
        eprintln!("[telegram] topic closed (no matching instance)");
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
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        let name = resolve_topic(&mut s, thread_id);
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
            {
                match telegram_runtime()
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
        let s = self.lock().unwrap_or_else(|e| e.into_inner());
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
        );
        assert!(state.topic_to_instance.is_empty());
        assert!(state.instance_to_topic.is_empty());
    }

    #[test]
    fn telegram_state_submit_keys_preserved() {
        let mut keys = HashMap::new();
        keys.insert("agent1".to_string(), "\n".to_string());
        let state = TelegramState::new("tok", -1, HashMap::new(), PathBuf::from("/tmp"), keys);
        assert_eq!(state.submit_keys.get("agent1"), Some(&"\n".to_string()));
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
        let mut state =
            TelegramState::new("tok", -1, topic_map, PathBuf::from("/tmp"), HashMap::new());
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
        let mut state = TelegramState::new("tok", -1, HashMap::new(), home.clone(), HashMap::new());

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
        let mut state = TelegramState::new("tok", -1, HashMap::new(), home.clone(), HashMap::new());

        // First call: reloads from fleet.yaml
        assert_eq!(resolve_topic(&mut state, Some(500)), "bob");
        // Delete fleet.yaml — second call should use cached map
        std::fs::remove_file(home.join("fleet.yaml")).ok();
        assert_eq!(resolve_topic(&mut state, Some(500)), "bob");

        std::fs::remove_dir_all(&home).ok();
    }
}
