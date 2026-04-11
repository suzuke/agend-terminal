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

        if *topic_id == 1 {
            // General topic — no message_thread_id
            self.bot.send_message(self.group_id, text).await?;
        } else {
            self.bot
                .send_message(self.group_id, text)
                .message_thread_id(ThreadId(MessageId(*topic_id)))
                .await?;
        }
        Ok(())
    }
}

/// Start Telegram polling in a dedicated thread with its own tokio runtime.
pub fn start_polling(
    state: Arc<Mutex<TelegramState>>,
) {
    if let Err(e) = std::thread::Builder::new()
        .name("telegram".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("[telegram] failed to build tokio runtime: {e}");
                    return;
                }
            };
            rt.block_on(async {
                let bot = {
                    let s = state.lock().unwrap_or_else(|e| e.into_inner());
                    s.bot.clone()
                };

                let state2 = Arc::clone(&state);
                let handler = Update::filter_message().endpoint(
                    move |_bot: Bot, msg: Message| {
                        let state = Arc::clone(&state2);
                        async move {
                            handle_message(&state, &msg);
                            respond(())
                        }
                    },
                );

                eprintln!("[telegram] polling started");
                Dispatcher::builder(bot, handler)
                    .build()
                    .dispatch()
                    .await;
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
        let sk = s.submit_keys.get(&name).cloned().unwrap_or_else(|| "\r".to_string());
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
pub fn send_reply(state: &Arc<Mutex<TelegramState>>, instance_name: &str, text: &str) -> anyhow::Result<()> {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let bot = s.bot.clone();
    let group_id = s.group_id;
    let topic_id = s.instance_to_topic.get(instance_name).copied();
    drop(s);

    // Use a short-lived tokio runtime for the send
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        if let Some(tid) = topic_id {
            if tid == 1 {
                bot.send_message(group_id, text).await?;
            } else {
                bot.send_message(group_id, text)
                    .message_thread_id(ThreadId(MessageId(tid)))
                    .await?;
            }
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// Initialize Telegram from fleet config.
pub fn init_from_config(
    config: &crate::fleet::FleetConfig,
    home: &Path,
    submit_keys: HashMap<String, String>,
) -> Option<Arc<Mutex<TelegramState>>> {
    let channel = config.channel.as_ref()?;
    match channel {
        ChannelConfig::Telegram {
            bot_token_env,
            group_id,
            ..
        } => {
            let token = match std::env::var(bot_token_env) {
                Ok(t) => t,
                Err(_) => {
                    eprintln!("[telegram] bot token env '{bot_token_env}' not set, skipping");
                    return None;
                }
            };
            let topic_map: HashMap<String, i32> = config
                .instances
                .iter()
                .filter_map(|(name, inst)| inst.topic_id.map(|tid| (name.clone(), tid)))
                .collect();

            // Auto-create topics for instances without topic_id
            let bot = teloxide::Bot::new(&token);
            let chat_id = teloxide::types::ChatId(*group_id);
            let mut topic_map = topic_map;
            for (name, inst) in &config.instances {
                if inst.topic_id.is_none() && name != "general" {
                    eprintln!("[telegram] creating topic for '{name}'...");
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .ok();
                    if let Some(rt) = rt {
                        let result = rt.block_on(async {
                            bot.create_forum_topic(chat_id, name, 0x6FB9F0, "").await
                        });
                        match result {
                            Ok(topic) => {
                                let tid = topic.thread_id.0 .0;
                                eprintln!("[telegram] created topic '{name}' → {tid}");
                                topic_map.insert(name.clone(), tid);
                            }
                            Err(e) => {
                                eprintln!("[telegram] failed to create topic for '{name}': {e}");
                            }
                        }
                    }
                }
                // General without topic_id → use General topic (1)
                if name == "general" && inst.topic_id.is_none() {
                    topic_map.insert("general".to_string(), 1);
                }
            }

            // Write back newly created topic_ids to fleet.yaml (atomic via temp file)
            let fleet_path = home.join("fleet.yaml");
            if fleet_path.exists() {
                let lock_path = home.join(".fleet.yaml.lock");
                // Simple file lock: create lock file, write, remove lock
                if std::fs::OpenOptions::new().write(true).create_new(true).open(&lock_path).is_ok() {
                    if let Ok(content) = std::fs::read_to_string(&fleet_path) {
                        if let Ok(mut doc) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
                            let mut updated = false;
                            if let Some(instances) = doc.get_mut("instances").and_then(|v| v.as_mapping_mut()) {
                                for (name, tid) in &topic_map {
                                    let key = serde_yaml::Value::String(name.clone());
                                    if let Some(inst) = instances.get_mut(&key).and_then(|v| v.as_mapping_mut()) {
                                        let tid_key = serde_yaml::Value::String("topic_id".to_string());
                                        if !inst.contains_key(&tid_key) {
                                            inst.insert(tid_key, serde_yaml::Value::Number(serde_yaml::Number::from(*tid)));
                                            updated = true;
                                        }
                                    }
                                }
                            }
                            if updated {
                                if let Ok(yaml) = serde_yaml::to_string(&doc) {
                                    let yaml = format!("# Auto-updated by agend-terminal (topic_ids added)\n{yaml}");
                                    // Write to temp file first, then rename (atomic on same filesystem)
                                    let tmp_path = home.join(".fleet.yaml.tmp");
                                    if std::fs::write(&tmp_path, &yaml).is_ok() {
                                        if std::fs::rename(&tmp_path, &fleet_path).is_ok() {
                                            eprintln!("[telegram] updated fleet.yaml with topic_ids");
                                        } else {
                                            let _ = std::fs::remove_file(&tmp_path);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let _ = std::fs::remove_file(&lock_path);
                } else {
                    eprintln!("[telegram] fleet.yaml locked by another process, skipping topic_id write-back");
                }
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
    }
}

/// ChannelAdapter implementation for Telegram.
impl crate::channel::ChannelAdapter for Arc<Mutex<TelegramState>> {
    fn name(&self) -> &str {
        "telegram"
    }

    fn send_reply(&self, instance_name: &str, text: &str) -> crate::channel::SendResult {
        let s = self.lock().unwrap_or_else(|e| e.into_inner());
        let bot = s.bot.clone();
        let group_id = s.group_id;
        let topic_id = s.instance_to_topic.get(instance_name).copied();
        drop(s);

        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => return crate::channel::SendResult::Failed(format!("{e}")),
        };

        match rt.block_on(async {
            if let Some(tid) = topic_id {
                if tid == 1 {
                    bot.send_message(group_id, text).await?;
                } else {
                    bot.send_message(group_id, text)
                        .message_thread_id(ThreadId(MessageId(tid)))
                        .await?;
                }
            }
            Ok::<(), anyhow::Error>(())
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
