use crate::agent::AgentRegistry;
use crate::inbox::{self, InboxMessage};
use parking_lot::Mutex;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{MessageId, ThreadId};

use super::error::cleanup_deleted_topic;
use super::state::{lock_state, TelegramState};
use super::topic_registry::load_topic_registry;

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
pub(super) fn resolve_topic(state: &mut TelegramState, topic_id: Option<i32>) -> String {
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
        // Fix B: check topics.json as third source (runtime-created topics
        // may exist there but not in fleet.yaml or in-memory state).
        let reg = load_topic_registry(&state.home);
        if let Some(inst_name) = reg.get(&tid) {
            state.topic_to_instance.insert(tid, inst_name.clone());
            state.instance_to_topic.insert(inst_name.clone(), tid);
            return inst_name.clone();
        }
        // Fix C: warn before fallback so operator sees misroute.
        tracing::warn!(
            thread_id = tid,
            "resolve_topic: topic_id not found in memory, fleet.yaml, or topics.json — falling back to \"general\""
        );
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
                superseded_by: None,
                from_id: None,
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
                let vec = arr.as_array_mut().expect("checked");
                vec.push(entry);
                // M2: cap to prevent unbounded growth (keep newest 100)
                if vec.len() > 100 {
                    *vec = vec.split_off(vec.len() - 100);
                }
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
            Some(bot) => super::download_file_async(&bot, &home, &instance_name, f.file_id).await,
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
        superseded_by: None,
        from_id: None,
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

/// Read the current `agent_state` of `instance_name` from the in-process
/// [`AgentRegistry`] and return true when the state expects raw keyboard
/// input rather than inbox-wrapped prose — i.e. `awaiting_operator` (startup
/// stall) or `interactive_prompt` (pattern-matched modal like codex's update
/// menu). Returns false when the registry is not attached (daemon bootstrap
/// not yet wired), the agent is missing, or any lock is poisoned — callers
/// then fall through to the inbox path rather than dropping messages.
pub(super) fn agent_wants_raw_keystrokes(
    registry: Option<&AgentRegistry>,
    instance_name: &str,
) -> bool {
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
