use crate::daemon::DaemonState;
use crate::protocol::InboxMessage;
use crate::pty_session::PtySession;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{MessageId, ThreadId};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// Telegram channel state held by daemon.
pub struct TelegramChannel {
    bot: Bot,
    group_id: ChatId,
    /// topic_id → instance name.
    topic_to_instance: HashMap<i32, String>,
    /// instance name → topic_id.
    instance_to_topic: HashMap<String, i32>,
    /// Shutdown token for stopping the polling dispatcher.
    shutdown_token: Option<teloxide::dispatching::ShutdownToken>,
}

impl TelegramChannel {
    pub fn new(
        bot_token: &str,
        group_id: i64,
        topic_map: HashMap<String, i32>,
    ) -> Self {
        let bot = Bot::new(bot_token);
        let topic_to_instance: HashMap<i32, String> = topic_map
            .iter()
            .map(|(name, &tid)| (tid, name.clone()))
            .collect();
        Self {
            bot,
            group_id: ChatId(group_id),
            topic_to_instance,
            instance_to_topic: topic_map,
            shutdown_token: None,
        }
    }

    /// Stop the polling dispatcher.
    pub async fn shutdown(&self) {
        if let Some(ref token) = self.shutdown_token {
            match token.shutdown() {
                Ok(f) => {
                    f.await;
                    info!("Telegram polling shutdown complete");
                }
                Err(_) => {
                    warn!("Telegram shutdown token already used");
                }
            }
        }
    }

    /// Send a message to a specific instance's Telegram topic.
    pub async fn send_to_topic(&self, instance_name: &str, text: &str) -> Result<()> {
        let topic_id = self
            .instance_to_topic
            .get(instance_name)
            .with_context(|| format!("No topic mapped for instance '{instance_name}'"))?;

        // General topic (topic_id=1) doesn't accept message_thread_id
        if *topic_id == 1 {
            self.bot
                .send_message(self.group_id, text)
                .await
                .context("Failed to send Telegram message")?;
        } else {
            self.bot
                .send_message(self.group_id, text)
                .message_thread_id(ThreadId(MessageId(*topic_id)))
                .await
                .context("Failed to send Telegram message")?;
        }

        Ok(())
    }

    /// Start polling for incoming Telegram messages.
    /// Routes messages to the appropriate session's inbox + PTY.
    pub fn start_polling(
        channel: Arc<Mutex<TelegramChannel>>,
        state: Arc<Mutex<DaemonState>>,
    ) {
        tokio::spawn(async move {
            let bot = {
                let ch = channel.lock().await;
                ch.bot.clone()
            };

            let channel_for_token = channel.clone();
            let handler = Update::filter_message().endpoint(
                move |_bot: Bot, msg: Message| {
                    let channel = channel.clone();
                    let state = state.clone();
                    async move {
                        if let Err(e) = handle_telegram_message(&channel, &state, &msg).await {
                            error!("Telegram handler error: {e:#}");
                        }
                        respond(())
                    }
                },
            );

            info!("Telegram polling started");
            let mut dispatcher = Dispatcher::builder(bot, handler).build();
            // Store shutdown token so fleet stop can terminate polling
            {
                let mut ch = channel_for_token.lock().await;
                ch.shutdown_token = Some(dispatcher.shutdown_token());
            }
            dispatcher.dispatch().await;
            info!("Telegram polling stopped");
        });
    }
}

async fn handle_telegram_message(
    channel: &Arc<Mutex<TelegramChannel>>,
    state: &Arc<Mutex<DaemonState>>,
    msg: &Message,
) -> Result<()> {
    let text = match msg.text() {
        Some(t) => t,
        None => return Ok(()), // Ignore non-text messages
    };

    let username = msg
        .from
        .as_ref()
        .and_then(|u| u.username.as_deref())
        .unwrap_or("unknown");

    // Determine which instance this message is for based on thread_id
    let thread_id = msg.thread_id.map(|ThreadId(MessageId(id))| id);

    let instance_name = {
        let ch = channel.lock().await;
        match thread_id {
            Some(tid) => ch.topic_to_instance.get(&tid).cloned(),
            None => {
                // Messages without thread_id go to general topic (thread_id = None)
                // or first instance if no topic mapping
                None
            }
        }
    };

    let instance_name = match instance_name {
        Some(name) => name,
        None => {
            // No topic mapping found — try "general" as fallback
            let st = state.lock().await;
            if st.name_to_id.contains_key("general") {
                "general".to_string()
            } else {
                warn!(
                    "No instance mapped for topic {:?}, ignoring message from {username}",
                    thread_id
                );
                return Ok(());
            }
        }
    };

    // Find the session
    let (session_id, session): (u32, Arc<PtySession>) = {
        let st = state.lock().await;
        match st.find_session_by_name(&instance_name) {
            Some((id, s)) => (id, s),
            None => {
                warn!("Instance '{instance_name}' not found as active session");
                return Ok(());
            }
        }
    };

    info!("[telegram] {username} → {instance_name}: {text}");

    // Enqueue in inbox
    let inbox_msg = InboxMessage {
        from: format!("user:{username}"),
        text: text.to_string(),
        kind: Some("telegram".to_string()),
        correlation_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    {
        let mut st = state.lock().await;
        st.enqueue_message(session_id, inbox_msg);
    }

    // Inject notification into PTY
    // Determine submit key based on command (gemini needs \n\r, others \r)
    let submit_key = {
        let st = state.lock().await;
        if let Some((_, s)) = st.find_session_by_name(&instance_name) {
            if s.command.contains("gemini") { "\n\r" } else { "\r" }
        } else {
            "\r"
        }
    };
    let notification = format!(
        "\n[user:{username} via telegram] {}{submit_key}",
        if text.chars().count() > 200 {
            let truncated: String = text.chars().take(200).collect();
            format!("{truncated}... (Run: agend-terminal inbox)")
        } else {
            text.to_string()
        }
    );
    let _ = session.write_input(notification.as_bytes()).await;

    Ok(())
}
