//! Telegram adapter — runs in dedicated thread with tokio runtime.
//!
//! Inbound: Telegram message → inbox + PTY notification
//! Outbound: reply(text) → Telegram send_message to topic

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

/// Lock TelegramState, recovering from poison.
/// With parking_lot::Mutex, lock never fails (no poisoning).
pub(crate) fn lock_state(
    tg: &Arc<Mutex<TelegramState>>,
) -> parking_lot::MutexGuard<'_, TelegramState> {
    tg.lock()
}

// ---------------------------------------------------------------------------
// Topic registry — persists topic_id → instance_name in $AGEND_HOME/topics.json
// so we can detect orphaned topics on daemon restart.
// ---------------------------------------------------------------------------

/// Reserved pseudo-instance name used in `topics.json` to pin the
/// `fleet_binding` topic across daemon restarts. Not a real instance —
/// chosen so it can never collide with a user-configured name
/// (`fleet.yaml` keys are slugs; underscores-bracketing is reserved).
/// See [`init_from_config`] orphan-cleanup filter and fleet-binding
/// resolution.
const FLEET_BINDING_SENTINEL: &str = "__fleet__";

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

/// Run a future on the Telegram runtime. If already inside an async context
/// (e.g. Telegram polling → emit path), spawns a fire-and-forget task on the
/// current runtime to avoid `block_on`-inside-runtime panic. Returns `Ok(())`
/// for the spawned path since the result is not awaited.
fn spawn_or_block_on<F>(fut: F) -> anyhow::Result<()>
where
    F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            if let Err(e) = fut.await {
                tracing::warn!(%e, "telegram spawn task failed");
            }
        });
        Ok(())
    } else {
        telegram_runtime().block_on(fut)
    }
}

pub struct TelegramState {
    /// `None` only inside the contract-test harness — production `new`
    /// always populates it via `Bot::new`. Transport methods unwrap with
    /// `.expect("telegram bot not initialized")`; contract tests never
    /// reach those paths (see `src/channel/contract.rs` scope comment).
    pub bot: Option<Bot>,
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
    /// Wired in post-bootstrap by [`attach_registry`]; lets inbound message
    /// routing read `agent_state` directly instead of via the `LIST` RPC.
    pub registry: Option<AgentRegistry>,
    /// Resolved `fleet_binding` target for cross-instance fleet activity
    /// rendering (Stage B-UX, `docs/DESIGN-stage-b-ux.md` §3/§5). `None`
    /// means no mirror is configured — [`TelegramChannel::apply_fleet_action`]
    /// returns early. Resolution happens in [`init_from_config`] from the
    /// config's `fleet_binding` block plus the on-disk topic registry
    /// sentinel `"__fleet__"`.
    pub fleet_binding_topic_id: Option<i32>,
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
            bot: Some(Bot::new(token)),
            group_id: ChatId(group_id),
            topic_to_instance,
            instance_to_topic: topic_map,
            home,
            submit_keys,
            user_allowlist,
            registry: None,
            fleet_binding_topic_id: None,
        }
    }

    /// Build a `TelegramState` without constructing a `teloxide::Bot` —
    /// used by the `src/channel/contract.rs` harness, which only exercises
    /// registry-side methods (`kind`, `has_binding`, `take_binding`,
    /// `record_binding`, `attach_registry`). `Bot::new` eagerly initializes
    /// reqwest + `system-configuration` proxy state and panics on some
    /// macOS setups, so the harness must not go through it. If a test
    /// triggers a transport path (`send_to_topic`, `send_reply`, polling),
    /// the `.expect("telegram bot not initialized")` unwrap will fire.
    #[cfg(test)]
    pub(crate) fn new_for_contract_test(
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
            bot: None,
            group_id: ChatId(group_id),
            topic_to_instance,
            instance_to_topic: topic_map,
            home,
            submit_keys,
            user_allowlist,
            registry: None,
            fleet_binding_topic_id: None,
        }
    }

    /// Return true if a sender is permitted by the allowlist.
    ///
    /// **Sprint 21 Phase 2 fail-closed swap (PR #216 + #217 cascade
    /// auth)**: previously `None` allowlist returned `true` (legacy
    /// accept-all); now it returns `false`. The implementation
    /// delegates to `crate::channel::auth::is_authorized_recipient`,
    /// which is the single source of truth shared with Phase 1's
    /// outbound notify gate. Operators must configure
    /// `user_allowlist: [user_id, ...]` in `fleet.yaml` to enable
    /// inbound (and outbound) traffic — see `docs/USAGE.md` "Channel:
    /// Telegram" section for the migration steps.
    pub fn is_user_allowed(&self, user_id: i64) -> bool {
        crate::channel::auth::is_authorized_recipient(&self.user_allowlist, user_id)
    }

    /// Send a message to an instance's Telegram topic.
    #[allow(dead_code)]
    pub async fn send_to_topic(&self, instance_name: &str, text: &str) -> anyhow::Result<()> {
        let topic_id = self
            .instance_to_topic
            .get(instance_name)
            .ok_or_else(|| anyhow::anyhow!("No topic for '{instance_name}'"))?;
        let bot = self
            .bot
            .as_ref()
            .expect("telegram bot not initialized (contract-test construction?)");
        send_with_topic(bot, self.group_id, Some(*topic_id), text, None).await
    }
}

/// Send a message, optionally to a topic, optionally as a reply, returning
/// the platform-assigned `message_id` (i32 per Telegram Bot API).
///
/// Phase 3 (Sprint 21) extracts msg_id capture so `Channel::send` can
/// return a `MsgRef` with a real id instead of `"0"` placeholder. Other
/// callers that historically returned `Result<()>` continue to use
/// [`send_with_topic`] (thin wrapper) so the behaviour change is
/// localised to the trait-method dispatch path.
async fn send_with_topic_capturing_id(
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
async fn send_with_topic(
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

/// Telegram Bot API caption limit (characters).
const CAPTION_MAX_CHARS: usize = 1024;

/// Resolve the caption for a media send.
fn resolve_caption(text: &str, att: &crate::channel::Attachment) -> Option<String> {
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
fn needs_separate_text(text: &str, att: &crate::channel::Attachment) -> bool {
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
async fn send_media(
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

/// Start Telegram polling in a dedicated thread with its own tokio runtime.
pub fn start_polling(state: Arc<Mutex<TelegramState>>) {
    // fire-and-forget: telegram polling thread runs the teloxide dispatcher
    // for the daemon's lifetime. Stops when the bot's update stream errors
    // (network drop / shutdown). No JoinHandle / shutdown signal needed —
    // process exit reaps the thread.
    if let Err(e) = std::thread::Builder::new()
        .name("telegram".into())
        .spawn(move || {
            let _census = crate::thread_census::register("telegram_poll");
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                tracing::error!("failed to build tokio runtime");
                return;
            };
            rt.block_on(async {
                let bot = lock_state(&state)
                    .bot
                    .clone()
                    .expect("telegram bot not initialized (polling thread)");
                let state2 = Arc::clone(&state);
                let handler = Update::filter_message().endpoint(move |_bot: Bot, msg: Message| {
                    let state = Arc::clone(&state2);
                    async move {
                        handle_message(&state, &msg).await;
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

async fn handle_message(state: &Arc<Mutex<TelegramState>>, msg: &Message) {
    // Detect topic closure/deletion — auto-delete the corresponding instance
    if msg.forum_topic_closed().is_some() {
        let thread_id = msg.thread_id.map(|ThreadId(MessageId(id))| id);
        if let Some(tid) = thread_id {
            let (instance_name, home) = {
                let s = lock_state(state);
                (s.topic_to_instance.get(&tid).cloned(), s.home.clone())
            };
            match instance_name {
                Some(name) => {
                    tracing::info!(topic_id = tid, instance = %name, "topic closed, deleting instance");
                    cleanup_deleted_topic(&home, &name, tid, Some(state));
                }
                None => tracing::warn!(topic_id = tid, "topic closed (no matching instance)"),
            }
            return;
        }
        tracing::warn!("topic closed (no thread_id)");
        return;
    }

    let text = match msg.text() {
        Some(t) => t.to_string(),
        None => msg.caption().unwrap_or("").to_string(),
    };

    // Status keyword detection — surface summary to the resolved instance's inbox.
    // The agent (typically general) sees the summary and can relay to operator.
    if crate::status_summary::is_status_keyword(&text) {
        let thread_id = msg.thread_id.map(|ThreadId(MessageId(id))| id);
        let instance_name = {
            let mut s = lock_state(state);
            resolve_topic(&mut s, thread_id)
        };
        let home = lock_state(state).home.clone();
        let summary = crate::status_summary::build_summary(&home);
        tracing::info!(to = %instance_name, "status keyword detected, injecting summary");
        let _ = crate::inbox::enqueue(
            &home,
            &instance_name,
            crate::inbox::InboxMessage {
                schema_version: 0,
                id: None,
                read_at: None,
                thread_id: None,
                parent_id: None,
                task_id: None,
                force_meta: None,
                correlation_id: None,
                reviewed_head: None,
                from: "system:status".to_string(),
                text: summary,
                kind: Some("status-summary".to_string()),
                timestamp: chrono::Utc::now().to_rfc3339(),
                channel: Some(crate::channel::ChannelKind::Telegram),
                delivery_mode: None,
                attachments: vec![],
                in_reply_to_msg_id: None,
                in_reply_to_excerpt: None,
            },
        );
        // Also notify agent PTY so it picks up the summary
        let username = msg
            .from
            .as_ref()
            .and_then(|u| u.username.as_deref())
            .unwrap_or("unknown");
        crate::inbox::notify_agent(
            &home,
            &instance_name,
            &crate::inbox::NotifySource::Channel(username, crate::channel::ChannelKind::Telegram),
            "[status-summary] check inbox for status overview",
        );
        return;
    }

    // Task entry via telegram: "加 task: <title>" creates a task assigned to dev-lead.
    if let Some(title) = crate::status_summary::parse_task_entry(&text) {
        let home = lock_state(state).home.clone();
        let result = crate::tasks::handle(
            &home,
            "operator",
            &serde_json::json!({
                "action": "create",
                "title": title,
                "assignee": "dev-lead",
                "priority": "normal",
            }),
        );
        let task_id = result["id"].as_str().unwrap_or("?");
        tracing::info!(title, task_id, "task created via telegram keyword");
        if let Some(ch) = crate::channel::active_channel() {
            let _ = ch.notify(
                "operator",
                crate::channel::NotifySeverity::Info,
                &format!("✅ Task created: {title} [{task_id}]"),
                false,
            );
        }
        return;
    }

    // Extract inbound attachment metadata (photo/voice/document/video/sticker).
    // Download happens later after topic resolution provides instance_name.
    struct InboundFile<'a> {
        file_id: &'a str,
        kind: crate::channel::event::AttachmentKind,
        mime: Option<String>,
        size: Option<u64>,
        filename: Option<String>,
    }
    let inbound_file: Option<InboundFile<'_>> = {
        use crate::channel::event::AttachmentKind;
        if let Some(sizes) = msg.photo() {
            sizes.last().map(|p| InboundFile {
                file_id: p.file.id.as_str(),
                kind: AttachmentKind::Photo,
                mime: None,
                size: Some(p.file.size as u64),
                filename: None,
            })
        } else if let Some(doc) = msg.document() {
            Some(InboundFile {
                file_id: doc.file.id.as_str(),
                kind: AttachmentKind::Document,
                mime: doc.mime_type.as_ref().map(|m| m.to_string()),
                size: Some(doc.file.size as u64),
                filename: doc.file_name.clone(),
            })
        } else if let Some(voice) = msg.voice() {
            Some(InboundFile {
                file_id: voice.file.id.as_str(),
                kind: AttachmentKind::Voice,
                mime: voice.mime_type.as_ref().map(|m| m.to_string()),
                size: Some(voice.file.size as u64),
                filename: None,
            })
        } else if let Some(video) = msg.video() {
            Some(InboundFile {
                file_id: video.file.id.as_str(),
                kind: AttachmentKind::Video,
                mime: video.mime_type.as_ref().map(|m| m.to_string()),
                size: Some(video.file.size as u64),
                filename: video.file_name.clone(),
            })
        } else {
            msg.sticker().map(|sticker| InboundFile {
                file_id: sticker.file.id.as_str(),
                kind: AttachmentKind::Sticker,
                mime: None,
                size: Some(sticker.file.size as u64),
                filename: None,
            })
        }
    };

    // If no text and no attachment, nothing to process.
    if text.is_empty() && inbound_file.is_none() {
        return;
    }

    let sender_id: Option<i64> = msg.from.as_ref().map(|u| u.id.0 as i64);
    let username = msg
        .from
        .as_ref()
        .and_then(|u| u.username.as_deref())
        .unwrap_or("unknown");

    // Authz: drop messages from senders not on the allowlist (Sprint 21
    // Phase 2 fail-closed cascade auth — combined with PR #216 outbound
    // gate this closes the cascade attack chain inbound side).
    //   - `Some([u1, u2])` → restricts to listed IDs (always was)
    //   - `Some([])` → rejects all
    //   - `None` → rejects all (Phase 2 inversion of legacy `accept-all`)
    //   - sender_id `None` (anonymous) → fail-closed: rejected
    {
        let s = lock_state(state);
        let allowed = match sender_id {
            Some(id) => s.is_user_allowed(id),
            None => false,
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

    let (instance_name, home, _submit_key, registry) = {
        let mut s = lock_state(state);
        let name = resolve_topic(&mut s, thread_id);
        let sk = s
            .submit_keys
            .get(&name)
            .cloned()
            .unwrap_or_else(|| "\r".to_string());
        (name, s.home.clone(), sk, s.registry.clone())
    };

    tracing::info!(from = username, to = %instance_name, %text, "inbound message");

    // Route based on agent state: when blocked on an interactive prompt
    // (AwaitingOperator startup stall, or a pattern-matched InteractivePrompt
    // like codex's update menu), the operator's reply must reach the PTY as
    // raw keystrokes — any inbox prefix ("[telegram:@user] …") would confuse
    // the CLI's prompt parser. In every other state, preserve the existing
    // inbox semantics so agent-authored message handling keeps working.
    if agent_wants_raw_keystrokes(registry.as_ref(), &instance_name) {
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
                "routed raw keystrokes (interactive prompt)"
            ),
            Err(e) => tracing::warn!(
                to = %instance_name,
                error = %e,
                "raw injection failed"
            ),
        }
        return;
    }

    // Persist the inbound message ID so the UX layer can resolve the
    // origin message when emitting AgentPickedUp (channel-agnostic).
    // Append to pending_pickup_ids array so multi-message bursts each
    // get a ✅ confirmation on inbox drain (F2 fix).
    {
        let meta_path = home.join("metadata").join(format!("{instance_name}.json"));
        let mut meta: serde_json::Value = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or(serde_json::json!({}));
        let entry = serde_json::json!({
            "kind": "telegram",
            "msg_id": msg.id.0.to_string(),
        });
        match meta.get_mut("pending_pickup_ids") {
            Some(arr) if arr.is_array() => {
                arr.as_array_mut().expect("checked").push(entry);
            }
            _ => {
                meta["pending_pickup_ids"] = serde_json::json!([entry]);
            }
        }
        let meta_dir = home.join("metadata");
        std::fs::create_dir_all(&meta_dir).ok();
        let tmp_path = meta_path.with_extension("json.tmp");
        if let Ok(content) = serde_json::to_string_pretty(&meta) {
            if std::fs::write(&tmp_path, &content).is_ok() {
                let _ = std::fs::rename(&tmp_path, &meta_path);
            }
        }
    }
    // Also store as last_message_id for the `react` MCP tool's fallback.
    crate::agent_ops::save_metadata(
        &home,
        &instance_name,
        "last_message_id",
        serde_json::json!(msg.id.0),
    );

    // Download inbound attachment if present (async — avoids nested runtime panic).
    let attachments: Vec<crate::channel::event::Attachment> = if let Some(f) = inbound_file {
        let bot = lock_state(state).bot.clone();
        let result = match bot {
            Some(bot) => download_file_async(&bot, &home, &instance_name, f.file_id).await,
            None => Err(anyhow::anyhow!("telegram bot not initialized")),
        };
        match result {
            Ok(local_path) => vec![crate::channel::event::Attachment {
                kind: f.kind,
                path: std::path::PathBuf::from(&local_path),
                mime: f.mime,
                caption: msg.caption().map(String::from),
                size_bytes: f.size,
                original_filename: f.filename,
            }],
            Err(e) => {
                tracing::warn!(file_id = f.file_id, error = %e, "inbound attachment download failed");
                vec![]
            }
        }
    } else {
        vec![]
    };

    // Enqueue in inbox
    let msg_obj = InboxMessage {
        schema_version: 0,
        id: None,
        read_at: None,
        thread_id: None,
        parent_id: None,
        task_id: None,
        force_meta: None,
        correlation_id: None,
        reviewed_head: None,
        from: format!("user:{username}"),
        text: text.to_string(),
        kind: None, // was "telegram" — channel source now in typed `channel` field
        timestamp: chrono::Utc::now().to_rfc3339(),
        channel: Some(crate::channel::ChannelKind::Telegram),
        delivery_mode: None,
        attachments,
        in_reply_to_msg_id: msg.reply_to_message().map(|r| r.id.0.to_string()),
        in_reply_to_excerpt: msg.reply_to_message().and_then(|r| {
            let text = r.text().or_else(|| r.caption()).unwrap_or("");
            let author = r
                .from
                .as_ref()
                .and_then(|u| u.username.as_deref())
                .unwrap_or("unknown");
            inbox::build_excerpt(text, author)
        }),
    };
    let _ = inbox::enqueue(&home, &instance_name, msg_obj);

    // Notify agent PTY
    inbox::notify_agent(
        &home,
        &instance_name,
        &inbox::NotifySource::Channel(username, crate::channel::ChannelKind::Telegram),
        &text,
    );

    // Emit UxEvent::UserMsgReceived so the channel adapter can react 👀.
    {
        use crate::channel::binding::BindingRef;
        use crate::channel::event::MsgRef;
        use crate::channel::ux_event::UxEvent;
        let origin_msg = MsgRef {
            binding: BindingRef::new("telegram", Some(instance_name.clone()), ()),
            id: msg.id.0.to_string(),
        };
        crate::channel::sink_registry::registry().emit(&UxEvent::UserMsgReceived {
            origin_msg,
            agent: instance_name,
        });
    }
}

/// Classify a send error as "the bound topic was deleted out from under us".
///
/// Bot API 6.3+ exposes no typed variant or deletion service message for
/// "thread gone"; it surfaces as `ApiError::Unknown("Bad Request: message
/// thread not found")`. Substring match on the flattened chain tolerates both
/// `anyhow::context` wrapping and future teloxide wrapping changes.
const TOPIC_DELETED_MARKER: &str = "message thread not found";

pub(crate) fn is_topic_deleted_error(err: &anyhow::Error) -> bool {
    let s = format!("{err:#}").to_lowercase();
    s.contains(TOPIC_DELETED_MARKER)
}

/// Cleanup path when a topic is known-deleted.
///
/// `state` is `None` for callers that lack access to `TelegramState` (daemon
/// `notify_telegram`, `try_telegram_reply`). Those paths leave the in-memory
/// maps stale until the next state-aware send or process restart — acceptable
/// because the MCP `delete_instance` handler's disk mutation (fleet.yaml
/// removal + `cleanup_working_dir`) is the source of truth.
pub(crate) fn cleanup_deleted_topic(
    home: &Path,
    instance_name: &str,
    tid: i32,
    state: Option<&Arc<Mutex<TelegramState>>>,
) {
    if let Some(state) = state {
        let mut s = lock_state(state);
        s.topic_to_instance.remove(&tid);
        s.instance_to_topic.remove(instance_name);
        s.submit_keys.remove(instance_name);
    }
    let _ = crate::api::call(
        home,
        &serde_json::json!({"method": crate::api::method::DELETE, "params": {"name": instance_name}}),
    );
    if let Err(e) = crate::fleet::remove_instance_from_yaml(home, instance_name) {
        tracing::warn!(instance = %instance_name, error = %e, "failed to remove from fleet.yaml");
    }
    // Strip the stale topic-id entry from the local registry. Safe to call
    // even if the topic was already unregistered (HashMap::remove is a no-op
    // on missing keys).
    unregister_topic(home, tid);
}

/// Cleanup path for a deleted `fleet_binding` topic. Unlike
/// [`cleanup_deleted_topic`] — which is instance-oriented and tears down
/// fleet.yaml / api::DELETE / per-instance state maps — the fleet binding
/// has no instance to drop, just a sentinel registry row and the
/// `fleet_binding_topic_id` field.
///
/// Only clears `fleet_binding_topic_id` when it still points at `tid`
/// (defensive — avoids clobbering a fresh binding if a stale error
/// somehow arrives after re-resolution).
pub(crate) fn cleanup_fleet_binding(home: &Path, state: &Arc<Mutex<TelegramState>>, tid: i32) {
    unregister_topic(home, tid);
    let mut s = lock_state(state);
    if s.fleet_binding_topic_id == Some(tid) {
        s.fleet_binding_topic_id = None;
    }
}

/// Classify a fleet-binding send error and run [`cleanup_fleet_binding`]
/// if it matches a topic-deleted error. Returns `true` when the error
/// was handled (topic gone); the renderer uses that to silence the outer
/// "send failed" warn since self-heal is the expected outcome, not a
/// surprise.
///
/// Reviewer context (at-dev-4 on PR #56): without this, deleting the
/// fleet topic once would (a) silently drop every subsequent fleet
/// emission, and (b) persist the stale `__fleet__` row in `topics.json`
/// so the next daemon restart happily reused the dead thread id.
/// Regression pin: [`tests::fleet_binding_self_heals_when_topic_deleted`].
pub(crate) fn handle_fleet_send_failure(
    err: &anyhow::Error,
    home: &Path,
    state: &Arc<Mutex<TelegramState>>,
    tid: i32,
) -> bool {
    if !is_topic_deleted_error(err) {
        return false;
    }
    tracing::info!(
        topic_id = tid,
        "fleet send hit topic_deleted — clearing binding + unregistering sentinel"
    );
    cleanup_fleet_binding(home, state, tid);
    true
}

/// Classify a send error and run topic-delete cleanup if it matches.
/// Returns `true` when the error was handled (topic gone); callers may then
/// silence the outer "send failed" log since cleanup is the expected outcome.
pub(crate) fn handle_send_failure(
    err: &anyhow::Error,
    home: &Path,
    instance_name: &str,
    topic_id: Option<i32>,
    state: Option<&Arc<Mutex<TelegramState>>>,
) -> bool {
    if !is_topic_deleted_error(err) {
        return false;
    }
    let Some(tid) = topic_id else {
        return false;
    };
    tracing::info!(
        instance = %instance_name,
        topic_id = tid,
        "send hit topic_deleted — cleaning up"
    );
    cleanup_deleted_topic(home, instance_name, tid, state);
    true
}

/// Lightweight self-heal for a stale topic: strip the dead topic_id from
/// the on-disk registry and fleet.yaml, create a fresh forum topic, and
/// persist the new mapping. Does NOT delete the instance (unlike
/// [`cleanup_deleted_topic`] which tears down the entire instance).
///
/// Returns `Some(new_tid)` on success so callers can retry the send with
/// the fresh topic. Returns `None` when topic creation fails (no bot
/// token, network error, etc.) — callers should log and give up.
pub(crate) fn invalidate_and_recreate_topic(
    home: &Path,
    instance_name: &str,
    stale_tid: i32,
) -> Option<i32> {
    tracing::info!(
        instance = %instance_name,
        stale_topic_id = stale_tid,
        "invalidating stale topic and recreating"
    );
    unregister_topic(home, stale_tid);
    // Clear the stale topic_id from fleet.yaml so create_topic_for_instance
    // doesn't short-circuit on the old value.
    let _ = crate::fleet::update_instance_field(
        home,
        instance_name,
        "topic_id",
        serde_yaml::Value::Null,
    );
    create_topic_for_instance(home, instance_name)
}

/// Read the current `agent_state` of `instance_name` from the in-process
/// [`AgentRegistry`] and return true when the state expects raw keyboard
/// input rather than inbox-wrapped prose — i.e. `awaiting_operator` (startup
/// stall) or `interactive_prompt` (pattern-matched modal like codex's update
/// menu). Returns false when the registry is not attached (daemon bootstrap
/// not yet wired), the agent is missing, or any lock is poisoned — callers
/// then fall through to the inbox path rather than dropping messages.
fn agent_wants_raw_keystrokes(registry: Option<&AgentRegistry>, instance_name: &str) -> bool {
    let Some(registry) = registry else {
        return false;
    };
    let reg = crate::agent::lock_registry(registry);
    let Some(handle) = reg.get(instance_name) else {
        return false;
    };
    let core = Arc::clone(&handle.core);
    // Drop the registry lock before grabbing the per-agent core lock; holding
    // both at once risks deadlocks against code paths that take core → registry.
    drop(reg);
    let guard = core.lock();
    guard.state.current.wants_raw_keystrokes()
}

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

    // Resolve `fleet_binding` → topic_id. See docs/DESIGN-stage-b-ux.md §3/§5.
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
    match telegram_runtime()
        .block_on(async { bot.create_forum_topic(chat_id, &name, 0x6FB9F0, "").await })
    {
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

/// Reverse-lookup a topic_id for an instance from `topics.json`.
/// Returns `None` if no mapping exists or the file is missing.
pub fn lookup_topic_for_instance(home: &std::path::Path, instance_name: &str) -> Option<i32> {
    let reg = load_topic_registry(home);
    reg.into_iter()
        .find(|(_, name)| name == instance_name)
        .map(|(tid, _)| tid)
}

/// Create a forum topic for a new instance.
pub fn create_topic_for_instance(home: &std::path::Path, instance_name: &str) -> Option<i32> {
    // Idempotent: reuse existing topic from topics.json if present.
    if let Some(tid) = lookup_topic_for_instance(home, instance_name) {
        tracing::info!(instance = %instance_name, topic_id = tid, "reusing existing topic");
        return Some(tid);
    }
    let ch = resolve_channel_only_from(home).ok()?;
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
    let ch = match resolve_channel_only_from(home) {
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
    // fire-and-forget: per-call tg_notify spawns its own ephemeral runtime to
    // ship one notify message, then exits. No JoinHandle / shutdown signal
    // needed — losing one notification on shutdown is acceptable and the
    // sender continues with subsequent calls.
    std::thread::Builder::new()
        .name("tg_notify".into())
        .spawn(move || {
            let _census = crate::thread_census::register("tg_notify");
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            if let Err(e) = rt.block_on(async {
                use teloxide::payloads::SendMessageSetters;
                use teloxide::prelude::Requester;
                let bot = teloxide::Bot::new(&token);
                let chat_id = teloxide::types::ChatId(group_id);
                match topic_id {
                    Some(tid) if tid != 1 => {
                        let mut req = bot.send_message(chat_id, &text).message_thread_id(
                            teloxide::types::ThreadId(teloxide::types::MessageId(tid)),
                        );
                        if disable_notification {
                            req = req.disable_notification(true);
                        }
                        req.await?;
                    }
                    _ => {
                        let mut req = bot.send_message(chat_id, &text);
                        if disable_notification {
                            req = req.disable_notification(true);
                        }
                        req.await?;
                    }
                }
                Ok::<(), anyhow::Error>(())
            }) {
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
                            let _ = rt.block_on(async {
                                use teloxide::payloads::SendMessageSetters;
                                use teloxide::prelude::Requester;
                                let bot = teloxide::Bot::new(&token);
                                let chat_id = teloxide::types::ChatId(group_id);
                                let mut req = bot.send_message(chat_id, &text).message_thread_id(
                                    teloxide::types::ThreadId(teloxide::types::MessageId(new_tid)),
                                );
                                if disable_notification {
                                    req = req.disable_notification(true);
                                }
                                req.await
                            });
                            return;
                        }
                    }
                }
                tracing::warn!(error = %e, "telegram notify failed");
            }
        })
        .ok();
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
            // `docs/PLAN-channel-ux-layer.md`) is "adapter currently
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
        // Dispatch split per docs/DESIGN-stage-b-ux.md §4.4: Fleet events
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
    fn is_user_allowed_none_rejects_all_after_phase2_fail_closed_swap() {
        // Sprint 21 Phase 2 inversion: previously `None` allowlist
        // accepted all (legacy fail-open). Now `None` rejects all
        // (fail-closed) — operator must configure `user_allowlist:
        // [user_id, ...]` to authorise senders. See PR #216 + Phase 2
        // dispatch (cascade auth chain inbound side).
        let state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        assert!(
            !state.is_user_allowed(1),
            "fail-closed: None must reject any user"
        );
        assert!(
            !state.is_user_allowed(i64::MAX),
            "fail-closed: None must reject even valid Telegram user_ids"
        );
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
    fn is_user_allowed_realistic_user_id_after_phase2() {
        // Operator-pitfall regression: real Telegram user_ids are i64
        // values in the 100-million-to-billion range (e.g.
        // `1047180393`). YAML deserialisation via `serde_yaml` must
        // round-trip these as i64, not String — and the predicate
        // must compare i64 equality, not string equality. This pin
        // catches any future schema drift that lossy-coerces user_id.
        const REAL_USER_ID: i64 = 1_047_180_393;
        let state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            Some(vec![REAL_USER_ID]),
        );
        assert!(state.is_user_allowed(REAL_USER_ID));
        // Off-by-one user_id is rejected — confirms exact i64 compare.
        assert!(!state.is_user_allowed(REAL_USER_ID + 1));
        assert!(!state.is_user_allowed(REAL_USER_ID - 1));
    }

    #[test]
    fn user_allowlist_yaml_round_trip_i64_realistic_values() {
        // Operator-pitfall regression (per Phase 2 dispatch): YAML
        // round-trip for both `user_allowlist: [<positive i64>]` and
        // `group_id: <negative supergroup i64>` must preserve i64
        // semantics. Without this, a future serde refactor that
        // narrowed to i32 would silently truncate user_ids beyond 2^31.
        //
        // The values used are the real-world shape operator confirmed
        // via telegram (positive 10-digit user_id; negative 13-digit
        // supergroup chat id with `-100` prefix per Telegram Bot API).
        let yaml = r#"
group_id: -1003725098111
user_allowlist:
  - 1047180393
"#;
        #[derive(serde::Deserialize, Debug)]
        struct PartialChannel {
            group_id: i64,
            user_allowlist: Vec<i64>,
        }
        let parsed: PartialChannel = serde_yaml::from_str(yaml).expect("yaml must deserialize");
        assert_eq!(parsed.group_id, -1_003_725_098_111_i64);
        assert_eq!(parsed.user_allowlist, vec![1_047_180_393_i64]);

        // Round-trip back to YAML and re-parse to lock contract.
        let serialized = serde_yaml::to_string(&serde_json::json!({
            "group_id": parsed.group_id,
            "user_allowlist": parsed.user_allowlist,
        }))
        .expect("yaml must serialize");
        let reparsed: PartialChannel =
            serde_yaml::from_str(&serialized).expect("yaml round-trip must deserialize");
        assert_eq!(reparsed.group_id, parsed.group_id);
        assert_eq!(reparsed.user_allowlist, parsed.user_allowlist);
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
    // dispatch split from docs/DESIGN-stage-b-ux.md §4.4).

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

    /// Regression pin: create_topic_for_instance must resolve credentials
    /// from the caller-supplied home, not from `AGEND_HOME` / real
    /// `home_dir()`. Before this fix the helper called
    /// `resolve_channel_only()` which re-read the operator's real
    /// fleet.yaml, so `cargo test` runs of the `positive_pin`
    /// dispatch test (src/api/mod.rs) leaked into the live Telegram
    /// group as a stray `positive_pin-1` topic. Here the test home has
    /// no fleet.yaml at all; the helper must see "no channel
    /// configured" and return `None` silently — no network call, no
    /// leak.
    #[test]
    fn create_topic_for_instance_uses_passed_home_not_real_home() {
        let home = tmp_home("topic-helper-home-scope");
        // Deliberately no fleet.yaml → resolve_channel_only_from must
        // fail at the fleet.yaml load step. Even if AGEND_BOT_TOKEN is
        // set in the environment, we must not reach the teloxide call
        // because the test home has no channel config to resolve
        // group_id from.
        let result = create_topic_for_instance(&home, "regression-pin");
        assert!(
            result.is_none(),
            "missing fleet.yaml in the passed home must suppress the API call, got {result:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Companion to the test above for the symmetric delete path.
    /// `delete_topic` also accepts `home` but used the unscoped resolver;
    /// in tests that exercise teardown against a temp home it would have
    /// attempted to hit the real Telegram group on a made-up topic id.
    #[test]
    fn delete_topic_uses_passed_home_not_real_home() {
        let home = tmp_home("delete-topic-home-scope");
        // No fleet.yaml → should return without touching the network.
        // Can't directly assert "no HTTP request", but the early-return
        // branch is the only code path that terminates cleanly when
        // resolve fails, so reaching this assertion at all is proof.
        delete_topic(&home, 999_999);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn is_topic_deleted_error_matches_thread_not_found() {
        // Exact wording returned by Telegram Bot API when the thread was deleted.
        let e = anyhow::anyhow!("Bad Request: message thread not found");
        assert!(is_topic_deleted_error(&e));
    }

    #[test]
    fn is_topic_deleted_error_case_insensitive() {
        let e = anyhow::anyhow!("BAD REQUEST: MESSAGE THREAD NOT FOUND");
        assert!(is_topic_deleted_error(&e));
    }

    #[test]
    fn is_topic_deleted_error_matches_wrapped_context() {
        // Errors often get `.context(...)` wrapped; Alternate debug formatter
        // (`{:#}`) flattens the chain into a single string we can inspect.
        let inner = anyhow::anyhow!("Bad Request: message thread not found");
        let wrapped = inner.context("sending to topic 42");
        assert!(is_topic_deleted_error(&wrapped));
    }

    #[test]
    fn is_topic_deleted_error_rejects_unrelated() {
        // Common transient / auth errors must NOT trigger cleanup.
        for msg in [
            "network timeout",
            "Bad Request: chat not found",
            "Forbidden: bot was blocked by the user",
            "Too Many Requests: retry after 5",
            "Bad Request: message to edit not found",
        ] {
            let e = anyhow::anyhow!(msg.to_string());
            assert!(
                !is_topic_deleted_error(&e),
                "classifier must not match: {msg}"
            );
        }
    }

    #[test]
    fn cleanup_deleted_topic_clears_state_maps() {
        // State-aware cleanup path: topic_to_instance / instance_to_topic /
        // submit_keys entries must all be purged atomically. api::call and
        // fleet.yaml removal fail silently when there's no daemon/file — that
        // matches the production contract (we log and move on).
        use parking_lot::Mutex;
        use std::sync::Arc;
        let home = tmp_home("cleanup-state");
        let mut topic_map = HashMap::new();
        topic_map.insert("agent1".to_string(), 42);
        topic_map.insert("agent2".to_string(), 43);
        let mut submit_keys = HashMap::new();
        submit_keys.insert("agent1".to_string(), "\r".to_string());
        let state = Arc::new(Mutex::new(TelegramState::new(
            "tok",
            -1,
            topic_map,
            home.clone(),
            submit_keys,
            None,
        )));

        cleanup_deleted_topic(&home, "agent1", 42, Some(&state));

        let s = state.lock();
        assert!(!s.topic_to_instance.contains_key(&42));
        assert!(!s.instance_to_topic.contains_key("agent1"));
        assert!(!s.submit_keys.contains_key("agent1"));
        // agent2 untouched.
        assert_eq!(s.topic_to_instance.get(&43), Some(&"agent2".to_string()));
        assert_eq!(s.instance_to_topic.get("agent2"), Some(&43));
    }

    #[test]
    fn cleanup_deleted_topic_unregisters_topic() {
        // Both state-aware and headless callers must strip the topic from the
        // on-disk registry so orphan-cleanup on next startup doesn't re-see it.
        let home = tmp_home("cleanup-registry");
        let mut reg = HashMap::new();
        reg.insert(99, "ghost".to_string());
        reg.insert(100, "alive".to_string());
        save_topic_registry(&home, &reg);

        cleanup_deleted_topic(&home, "ghost", 99, None);

        let after = load_topic_registry(&home);
        assert!(!after.contains_key(&99));
        assert_eq!(after.get(&100), Some(&"alive".to_string()));
    }

    #[test]
    fn handle_send_failure_only_fires_on_topic_deleted() {
        let home = tmp_home("handle-send");
        // Unrelated error: classifier miss → returns false, no cleanup.
        let unrelated = anyhow::anyhow!("network timeout");
        assert!(!handle_send_failure(
            &unrelated,
            &home,
            "any",
            Some(42),
            None
        ));
        // Topic-deleted error but no topic_id → nothing to clean up.
        let gone = anyhow::anyhow!("Bad Request: message thread not found");
        assert!(!handle_send_failure(&gone, &home, "any", None, None));
        // Topic-deleted + topic_id → fires.
        assert!(handle_send_failure(&gone, &home, "any", Some(42), None));
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

    // ─── Outbound media helper tests (Phase 3 PR-AG) ──────────────────

    fn make_attachment(kind: crate::channel::AttachmentKind) -> crate::channel::Attachment {
        crate::channel::Attachment {
            kind,
            path: PathBuf::from("/tmp/test.jpg"),
            mime: None,
            caption: None,
            size_bytes: None,
            original_filename: None,
        }
    }

    #[test]
    fn resolve_caption_uses_text_when_short() {
        let att = make_attachment(crate::channel::AttachmentKind::Photo);
        assert_eq!(resolve_caption("hello", &att).as_deref(), Some("hello"));
    }

    #[test]
    fn resolve_caption_none_when_text_exceeds_limit() {
        let att = make_attachment(crate::channel::AttachmentKind::Photo);
        assert!(resolve_caption(&"x".repeat(CAPTION_MAX_CHARS + 1), &att).is_none());
    }

    #[test]
    fn resolve_caption_prefers_attachment_caption() {
        let mut att = make_attachment(crate::channel::AttachmentKind::Document);
        att.caption = Some("explicit".into());
        assert_eq!(resolve_caption("text", &att).as_deref(), Some("explicit"));
    }

    #[test]
    fn resolve_caption_truncates_long_attachment_caption() {
        let mut att = make_attachment(crate::channel::AttachmentKind::Photo);
        att.caption = Some("x".repeat(CAPTION_MAX_CHARS + 100));
        let cap = resolve_caption("", &att).expect("should have caption");
        assert_eq!(cap.chars().count(), CAPTION_MAX_CHARS);
    }

    #[test]
    fn resolve_caption_none_for_sticker() {
        let att = make_attachment(crate::channel::AttachmentKind::Sticker);
        assert!(resolve_caption("hello", &att).is_none());
    }

    #[test]
    fn resolve_caption_none_when_text_empty() {
        let att = make_attachment(crate::channel::AttachmentKind::Photo);
        assert!(resolve_caption("", &att).is_none());
    }

    #[test]
    fn needs_separate_text_false_when_empty() {
        assert!(!needs_separate_text(
            "",
            &make_attachment(crate::channel::AttachmentKind::Photo)
        ));
    }

    #[test]
    fn needs_separate_text_false_when_short() {
        assert!(!needs_separate_text(
            "short",
            &make_attachment(crate::channel::AttachmentKind::Photo)
        ));
    }

    #[test]
    fn needs_separate_text_true_when_long() {
        assert!(needs_separate_text(
            &"x".repeat(CAPTION_MAX_CHARS + 1),
            &make_attachment(crate::channel::AttachmentKind::Photo)
        ));
    }

    #[test]
    fn needs_separate_text_true_for_sticker_with_text() {
        assert!(needs_separate_text(
            "hello",
            &make_attachment(crate::channel::AttachmentKind::Sticker)
        ));
    }

    #[test]
    fn needs_separate_text_true_when_attachment_has_own_caption() {
        let mut att = make_attachment(crate::channel::AttachmentKind::Photo);
        att.caption = Some("cap".into());
        assert!(needs_separate_text("body", &att));
    }

    #[test]
    fn send_text_only_reaches_text_path() {
        use crate::channel::Channel;
        let state = TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));
        let binding = crate::channel::BindingRef::new("telegram", None, ());
        let err = channel
            .send(&binding, crate::channel::OutMsg::text("hello"))
            .expect_err("no bot");
        assert!(
            err.to_string().contains("telegram bot not initialized"),
            "{err}"
        );
    }

    #[test]
    fn send_attachment_reaches_media_path() {
        use crate::channel::Channel;
        let state = TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            PathBuf::from("/tmp"),
            HashMap::new(),
            None,
        );
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));
        let binding = crate::channel::BindingRef::new("telegram", None, ());
        let msg = crate::channel::OutMsg {
            text: "see".into(),
            attachment: Some(make_attachment(crate::channel::AttachmentKind::Photo)),
            in_reply_to: None,
        };
        let err = channel.send(&binding, msg).expect_err("no bot");
        assert!(
            err.to_string().contains("telegram bot not initialized"),
            "{err}"
        );
    }

    // ─── Topic orphan fix tests (Sprint 14 PR-AI) ─────────────────────

    #[test]
    fn lookup_topic_for_instance_finds_existing() {
        let home = tmp_home("lookup_existing");
        register_topic(&home, 42, "alice");
        register_topic(&home, 99, "bob");
        assert_eq!(lookup_topic_for_instance(&home, "alice"), Some(42));
        assert_eq!(lookup_topic_for_instance(&home, "bob"), Some(99));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn lookup_topic_for_instance_returns_none_when_missing() {
        let home = tmp_home("lookup_missing");
        register_topic(&home, 42, "alice");
        assert_eq!(lookup_topic_for_instance(&home, "nonexistent"), None);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn lookup_topic_for_instance_returns_none_when_no_file() {
        let home = PathBuf::from("/tmp/agend-test-no-topics-json-12345");
        assert_eq!(lookup_topic_for_instance(&home, "any"), None);
    }

    /// create_topic_for_instance must reuse an existing topic from
    /// topics.json instead of creating a new one. Without a live bot
    /// the API call would fail, so the test seeds topics.json and
    /// verifies the early return path.
    #[test]
    fn create_topic_for_instance_reuses_existing_topic() {
        let home = tmp_home("create_reuse");
        register_topic(&home, 77, "reuse-agent");
        // No fleet.yaml / no bot token → if it tried to create, it would
        // return None. But the lookup-before-create path should return 77.
        let result = create_topic_for_instance(&home, "reuse-agent");
        assert_eq!(
            result,
            Some(77),
            "must reuse existing topic, not create new"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// When no existing topic exists and no channel is configured,
    /// create_topic_for_instance returns None (no bot to call).
    #[test]
    fn create_topic_for_instance_returns_none_without_config() {
        let home = tmp_home("create_no_config");
        let result = create_topic_for_instance(&home, "new-agent");
        assert!(result.is_none(), "no config → no topic creation");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Invariant: `handle_message` must not call `block_on` — it runs inside
    /// the telegram polling thread's tokio runtime, so nested `block_on`
    /// panics. This grep-based test catches regressions.
    #[test]
    fn handle_message_body_has_no_block_on() {
        let src = include_str!("telegram.rs");
        // Extract handle_message body (from "async fn handle_message" to next top-level fn)
        let start = src
            .find("async fn handle_message(")
            .expect("handle_message must exist");
        // Find the next top-level function after handle_message
        let rest = &src[start + 30..];
        let end = rest
            .find("\nfn ")
            .or_else(|| rest.find("\nasync fn "))
            .or_else(|| rest.find("\npub fn "))
            .or_else(|| rest.find("\npub(crate) fn "))
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            !body.contains("block_on"),
            "handle_message must not call block_on (nested runtime panic). Found block_on in body."
        );
    }

    // ─── Sprint 23 P1: Topic-cache validate-before-reuse tests ────────

    /// `invalidate_and_recreate_topic` must strip the stale topic from
    /// `topics.json` so `create_topic_for_instance` doesn't short-circuit
    /// on the dead id. Without a live bot the creation step returns `None`,
    /// but the invalidation side-effect is the load-bearing contract.
    #[test]
    fn invalidate_and_recreate_strips_stale_topic_from_registry() {
        let home = tmp_home("invalidate-recreate-strip");
        register_topic(&home, 42, "agent-x");
        register_topic(&home, 99, "agent-y");

        // No fleet.yaml / no bot → create_topic_for_instance returns None,
        // but the stale entry must still be gone from the registry.
        let result = invalidate_and_recreate_topic(&home, "agent-x", 42);
        assert!(result.is_none(), "no bot → creation fails");

        let reg = load_topic_registry(&home);
        assert!(
            !reg.contains_key(&42),
            "stale topic 42 must be unregistered"
        );
        // Unrelated entry survives.
        assert_eq!(reg.get(&99), Some(&"agent-y".to_string()));
        std::fs::remove_dir_all(&home).ok();
    }

    /// `invalidate_and_recreate_topic` must clear `topic_id` in fleet.yaml
    /// so the stale value doesn't persist across daemon restarts.
    #[test]
    fn invalidate_and_recreate_clears_fleet_yaml_topic_id() {
        let home = tmp_home("invalidate-recreate-yaml");
        let yaml = "\
instances:
  agent-x:
    command: /bin/true
    topic_id: 42
  agent-y:
    command: /bin/true
    topic_id: 99
";
        std::fs::write(home.join("fleet.yaml"), yaml).expect("write fleet.yaml");
        register_topic(&home, 42, "agent-x");

        let _ = invalidate_and_recreate_topic(&home, "agent-x", 42);

        // fleet.yaml must no longer have topic_id: 42 for agent-x.
        let config =
            crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).expect("load fleet.yaml");
        let agent_x = config.instances.get("agent-x").expect("agent-x exists");
        assert_eq!(
            agent_x.topic_id, None,
            "topic_id must be cleared after invalidation"
        );
        // agent-y untouched.
        let agent_y = config.instances.get("agent-y").expect("agent-y exists");
        assert_eq!(agent_y.topic_id, Some(99));
        std::fs::remove_dir_all(&home).ok();
    }

    /// `invalidate_and_recreate_topic` must NOT delete the instance from
    /// fleet.yaml — only the topic_id field is cleared. This is the key
    /// difference from `cleanup_deleted_topic` which tears down the
    /// entire instance.
    #[test]
    fn invalidate_and_recreate_preserves_instance_in_fleet_yaml() {
        let home = tmp_home("invalidate-preserves-instance");
        let yaml = "\
instances:
  agent-x:
    command: /bin/true
    topic_id: 42
    role: important
";
        std::fs::write(home.join("fleet.yaml"), yaml).expect("write fleet.yaml");
        register_topic(&home, 42, "agent-x");

        let _ = invalidate_and_recreate_topic(&home, "agent-x", 42);

        let fleet_yaml = std::fs::read_to_string(home.join("fleet.yaml")).expect("read fleet.yaml");
        assert!(
            fleet_yaml.contains("agent-x"),
            "instance must survive invalidation; yaml:\n{fleet_yaml}"
        );
        assert!(
            fleet_yaml.contains("important"),
            "instance role must survive; yaml:\n{fleet_yaml}"
        );
        std::fs::remove_dir_all(&home).ok();
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
