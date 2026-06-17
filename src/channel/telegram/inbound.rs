use crate::agent::AgentRegistry;
use crate::inbox::{self, InboxMessage};
use parking_lot::Mutex;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{MessageId, ThreadId};

use super::error::cleanup_deleted_topic;
use super::poll_supervisor;
use super::state::{lock_state, TelegramState};
use super::topic_registry::load_topic_registry;

/// Start Telegram polling in a dedicated thread with its own tokio runtime.
///
/// Supervisor loop (#2200): drives teloxide's `try_dispatch_with_listener`,
/// which RETURNS the initial `get_me` error instead of panicking like
/// `dispatch()` (`.expect("Couldn't prepare dispatching context")`). A cold
/// `api.telegram.org` therefore yields an `Err` we handle with exponential
/// backoff (5s → cap 60s) + a degraded latch + fire-once logging, instead of a
/// fixed-5s panic-backtrace flood that washed the TUI. A definitively-bad token
/// stops the loop. `catch_unwind` is retained as belt-and-suspenders for
/// teloxide's OTHER internal `.expect()`s (worker-join / TX), which are not on
/// the network path and should now ~never fire.
pub fn start_polling(state: Arc<Mutex<TelegramState>>) {
    // fire-and-forget: telegram supervisor thread runs for the daemon's lifetime.
    if let Err(e) = std::thread::Builder::new()
        .name("telegram".into())
        .spawn(move || {
            let _census = crate::thread_census::register("telegram_poll");
            let mut health = poll_supervisor::PollingHealth::default();
            loop {
                tracing::info!("telegram dispatcher starting");
                let state_clone = Arc::clone(&state);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    else {
                        tracing::error!("failed to build tokio runtime");
                        // Treat as a benign transient exit so the loop backs off
                        // rather than spinning on a runtime-build failure.
                        return Ok(());
                    };
                    rt.block_on(drive_dispatch_once(&state_clone))
                }));

                // Normalize the attempt into success / classified-failure. An
                // outer `Err` is a teloxide-internal panic NOT on the get_me path
                // (rare) — treat as transient; the panic hook already logged it.
                let attempt = match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err((poll_supervisor::classify_connect_error(&e), e.to_string())),
                    Err(_) => Err((
                        poll_supervisor::ConnectErrorClass::Transient,
                        "dispatcher panicked (caught)".to_string(),
                    )),
                };

                match attempt {
                    Ok(()) => {
                        // get_me succeeded; the dispatcher ran and exited (stream
                        // end / shutdown). Reset backoff; log recovery once.
                        if health.on_success() {
                            tracing::info!("telegram channel recovered");
                        }
                        std::thread::sleep(poll_supervisor::POLL_BACKOFF_BASE);
                    }
                    Err((class, detail)) => match health.on_failure(class) {
                        poll_supervisor::FailureOutcome::Stop => {
                            tracing::error!(
                                detail = %detail,
                                "telegram bot token invalid/unauthorized — stopping polling (no retry)"
                            );
                            break;
                        }
                        poll_supervisor::FailureOutcome::Retry { delay, log } => {
                            match log {
                                poll_supervisor::FailureLog::FirstWarn => tracing::warn!(
                                    detail = %detail,
                                    backoff_secs = delay.as_secs(),
                                    "telegram polling failed — retrying with backoff"
                                ),
                                poll_supervisor::FailureLog::DegradedEntered => tracing::info!(
                                    consecutive_failures = poll_supervisor::POLL_DEGRADE_AFTER,
                                    "telegram channel degraded (offline) — retries continue silently until recovery"
                                ),
                                poll_supervisor::FailureLog::Silent => {}
                            }
                            std::thread::sleep(delay);
                        }
                    },
                }
            }
        })
    {
        tracing::error!(error = %e, "failed to spawn polling thread");
    }
}

/// One dispatch attempt. Builds a poll-only `Polling` listener (no
/// `delete_webhook` pre-flight — agend never sets a webhook, so skipping it
/// avoids an extra per-cycle network call + its error log) and drives
/// `try_dispatch_with_listener`, which returns the initial `get_me` error as
/// `Err` instead of panicking (#2200). `Ok(())` means the connection succeeded
/// and the dispatcher later exited normally.
async fn drive_dispatch_once(
    state: &Arc<Mutex<TelegramState>>,
) -> Result<(), teloxide::RequestError> {
    let bot = lock_state(state)
        .bot
        .clone()
        .expect("telegram bot not initialized (polling thread)");
    let state2 = Arc::clone(state);
    let handler = Update::filter_message().endpoint(move |_bot: Bot, msg: Message| {
        let state = Arc::clone(&state2);
        async move {
            handle_message(&state, &msg).await;
            respond(())
        }
    });
    let listener = teloxide::update_listeners::Polling::builder(bot.clone())
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let err_handler = teloxide::error_handlers::LoggingErrorHandler::with_custom_text(
        "telegram update listener error",
    );
    tracing::info!("polling started");
    Dispatcher::builder(bot, handler)
        .build()
        .try_dispatch_with_listener(listener, err_handler)
        .await
}

/// Resolve a topic_id to an instance name.
/// First checks the in-memory map, then reloads from fleet.yaml for
/// runtime-created topics (via create_instance).
pub(super) fn resolve_topic(state: &mut TelegramState, topic_id: Option<i32>) -> String {
    if let Some(tid) = topic_id {
        if let Some(name) = state.topic_to_instance.get(&tid).cloned() {
            return name;
        }
        // topics.json is the canonical source for topic_id → instance mapping.
        let reg = load_topic_registry(&state.home);
        if let Some(inst_name) = reg.get(&tid) {
            state.topic_to_instance.insert(tid, inst_name.clone());
            state.instance_to_topic.insert(inst_name.clone(), tid);
            return inst_name.clone();
        }
        // Fix C: warn before fallback so operator sees misroute.
        tracing::warn!(
            thread_id = tid,
            "resolve_topic: topic_id not found in memory or topics.json — falling back to \"general\""
        );
    }
    "general".to_string()
}

/// Sprint 54 silent-drop hotfix: emit the canonical attachment-download
/// failure WARN with sender + kind context. Extracted so the test suite
/// can verify the field shape via `tracing_test` without driving the
/// full `handle_message` async path.
///
/// The pre-hotfix log only carried `file_id` + `error`. Operators
/// triaging "agent never received this image" needed to grep the
/// inbox + cross-reference channel logs to identify the sender. This
/// version emits enough context to identify both the user and the
/// media kind in one line.
pub(super) fn emit_download_failure_warn(
    file_id: &str,
    error: &str,
    sender_id: Option<i64>,
    kind: &crate::channel::event::AttachmentKind,
) {
    tracing::warn!(
        file_id = file_id,
        error = error,
        sender_id = ?sender_id,
        kind = ?kind,
        "inbound attachment download failed"
    );
}

/// Sprint 54 silent-drop hotfix: pick the inbox `text` to enqueue
/// when an image download just failed. The pre-hotfix code path
/// produced `text="" attachments=[]` for pure-image-no-caption sends,
/// which the agent sees as a content-less message it can't action on.
///
/// Behavior:
/// - `initial = ""` (pure image, no caption) → user-visible fallback
///   `[image attached but download failed]` so the agent at minimum
///   knows the user tried to send something and can ask for re-send.
/// - `initial = "<caption>"` → caption passes through unchanged. The
///   download failure is still surfaced via the WARN, but the user's
///   own words take precedence.
pub(super) fn resolve_text_after_image_download_failure(initial: &str) -> String {
    if initial.is_empty() {
        "[image attached but download failed]".to_string()
    } else {
        initial.to_string()
    }
}

/// #1352 length-based delivery split: a telegram inbound message takes the
/// short PTY-inject path only when it is under the char cap AND carries no
/// attachments; otherwise it goes inbox + pointer hint. Extracted from
/// `handle_message` (byte-identical: `chars().count() < 200 && empty`) so the
/// routing decision is unit-testable. The `< 200` char cap is the production
/// behavior, kept by operator decision (d-20260617102838730641-2); the unused
/// `HEADER_SIZE_THRESHOLD` (300) const that production never consulted was
/// removed (#t-109).
pub(super) fn is_short_inject(
    text: &str,
    attachments: &[crate::channel::event::Attachment],
) -> bool {
    text.chars().count() < 200 && attachments.is_empty()
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

    let mut text = match msg.text() {
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
        persist_or_log!(
            crate::inbox::enqueue(
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
                    broadcast_context: None,
                    sequencing: None,
                    eta_minutes: None,
                    reporting_cadence: None,
                    worktree_binding_required: None,
                    pr_number: None,
                    terminal: None,
                },
            ),
            "status_summary",
            instance_name
        );
        // Also notify agent PTY so it picks up the summary
        let sid = msg.from.as_ref().map(|u| u.id.0 as i64);
        let allowlist_name: Option<String> = if msg
            .from
            .as_ref()
            .and_then(|u| u.username.as_deref())
            .is_none()
        {
            sid.and_then(|id| lock_state(state).username_for(id).map(str::to_string))
        } else {
            None
        };
        let username = msg
            .from
            .as_ref()
            .and_then(|u| u.username.as_deref())
            .or(allowlist_name.as_deref())
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
        // #1339 PR-2: route the task-create ACK through the single `gated_notify`
        // chokepoint too — there must be NO production `Channel::notify` call
        // that skips the operator-mode gate. An operator messaging the channel
        // does NOT auto-flip the mode (it is read from `operator-mode.json`,
        // explicitly set), so a direct `ch.notify` here would bypass the gate.
        // Routed through, an explicit `Sleep`/`Away` suppresses this `Info` ack
        // (consistent with the mode's "hold non-Error" semantics); `Active`
        // (the default) passes it unchanged. The distinct "an operator-initiated
        // request's ACK should always reach the operator regardless of mode"
        // semantic is a separate operator-response path, tracked as a follow-up
        // and out of scope here.
        if let Some(ch) = crate::channel::active_channel() {
            let _ = crate::channel::gated_notify(
                ch.as_ref(),
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
                file_id: p.file.id.0.as_str(),
                kind: AttachmentKind::Photo,
                mime: None,
                size: Some(p.file.size as u64),
                filename: None,
            })
        } else if let Some(doc) = msg.document() {
            Some(InboundFile {
                file_id: doc.file.id.0.as_str(),
                kind: AttachmentKind::Document,
                mime: doc.mime_type.as_ref().map(|m| m.to_string()),
                size: Some(doc.file.size as u64),
                filename: doc.file_name.clone(),
            })
        } else if let Some(voice) = msg.voice() {
            Some(InboundFile {
                file_id: voice.file.id.0.as_str(),
                kind: AttachmentKind::Voice,
                mime: voice.mime_type.as_ref().map(|m| m.to_string()),
                size: Some(voice.file.size as u64),
                filename: None,
            })
        } else if let Some(video) = msg.video() {
            Some(InboundFile {
                file_id: video.file.id.0.as_str(),
                kind: AttachmentKind::Video,
                mime: video.mime_type.as_ref().map(|m| m.to_string()),
                size: Some(video.file.size as u64),
                filename: video.file_name.clone(),
            })
        } else {
            msg.sticker().map(|sticker| InboundFile {
                file_id: sticker.file.id.0.as_str(),
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
    // Display name: prefer the public @username; if absent, fall back to the
    // configured allowlist name (`{ id, name }` in fleet.yaml) so the operator
    // shows as their name instead of `unknown`; else "unknown".
    let allowlist_name: Option<String> = if msg
        .from
        .as_ref()
        .and_then(|u| u.username.as_deref())
        .is_none()
    {
        sender_id.and_then(|id| lock_state(state).username_for(id).map(str::to_string))
    } else {
        None
    };
    let username = msg
        .from
        .as_ref()
        .and_then(|u| u.username.as_deref())
        .or(allowlist_name.as_deref())
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
        // #1682: read-modify-write pending_pickup_ids through the resolver. The
        // previous hand-coded `<name>.json` read + bare `rename` wrote the legacy
        // name file (and clobbered the resolver's `<uuid>.json` symlink), so the
        // pickup IDs split from the id file every other reader/writer uses —
        // multi-message bursts then lost their ✅ confirmations.
        let entry = serde_json::json!({
            "kind": "telegram",
            "msg_id": msg.id.0.to_string(),
        });
        // CR-2026-06-14 (concurrency): append under the metadata flock (locked
        // read-modify-write). The prior unlocked read + `save_metadata`
        // overwrite raced the inbox-drain FILTER (comms.rs handle_inbox): the
        // append could read a stale set, then write its precomputed array back
        // AFTER the filter had removed a processed id — resurrecting it (a
        // processed pickup re-confirmed). Both the append (here) and the filter
        // now run as `update_metadata` locked RMWs on the same key, so they
        // serialize and neither clobbers the other.
        crate::agent_ops::update_metadata(&home, &instance_name, "pending_pickup_ids", |current| {
            let mut ids: Vec<serde_json::Value> = current.as_array().cloned().unwrap_or_default();
            ids.push(entry);
            // M2: cap to prevent unbounded growth (keep newest 100)
            if ids.len() > 100 {
                ids = ids.split_off(ids.len() - 100);
            }
            serde_json::json!(ids)
        });
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
        // Sprint 54 silent-drop hotfix: capture `is_image` before `f`
        // moves into the Ok arm so the Err arm can still decide whether
        // to apply the image-specific text fallback.
        let is_image = matches!(f.kind, crate::channel::event::AttachmentKind::Photo);
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
                emit_download_failure_warn(f.file_id, &e.to_string(), sender_id, &f.kind);
                // User-visible text fallback for the dominant silent-drop
                // case (pure image, no caption, download fails). Without
                // this, the inbox enqueue lands as text="" attachments=[]
                // — the agent sees a content-less message from a user it
                // can't action on. Other media kinds (voice/video/doc)
                // are not yet covered; extend per the same pattern if
                // reported.
                if is_image {
                    text = resolve_text_after_image_download_failure(&text);
                }
                vec![]
            }
        }
    } else {
        vec![]
    };

    // #1352: length-based delivery split.
    // Short messages (< 200 chars, no attachments): PTY inject only.
    // Long messages or attachments: inbox enqueue + pointer-only PTY hint.
    // AGEND_POINTER_ONLY_INJECT=1: all messages go inbox + hint (unchanged).
    let is_short = is_short_inject(&text, &attachments);
    let pointer_only = inbox::notify::pointer_only_inject();

    if is_short && !pointer_only {
        inbox::notify_agent_with_attachments(
            &home,
            &instance_name,
            &inbox::NotifySource::Channel(username, crate::channel::ChannelKind::Telegram),
            &text,
            &attachments,
        );
    } else {
        let notify_attachments = attachments.clone();
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
            kind: None,
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
            broadcast_context: None,
            sequencing: None,
            eta_minutes: None,
            reporting_cadence: None,
            worktree_binding_required: None,
            pr_number: None,
            terminal: None,
        };
        persist_or_log!(
            inbox::enqueue(&home, &instance_name, msg_obj),
            "telegram_dispatch",
            instance_name
        );
        inbox::notify_agent_with_attachments(
            &home,
            &instance_name,
            &inbox::NotifySource::Channel(username, crate::channel::ChannelKind::Telegram),
            &text,
            &notify_attachments,
        );
    }

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
    // #1441: registry is UUID-keyed; this raw-keystroke check has no fleet
    // home in scope, so locate the live handle by display name.
    let Some(handle) = reg.values().find(|h| h.name.as_str() == instance_name) else {
        return false;
    };
    let core = Arc::clone(&handle.core);
    // Drop the registry lock before grabbing the per-agent core lock; holding
    // both at once risks deadlocks against code paths that take core → registry.
    drop(reg);
    let guard = core.lock();
    guard.state.current.wants_raw_keystrokes()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn handle_message_body_has_no_block_on() {
        let src = include_str!("inbound.rs");
        let start = src
            .find("async fn handle_message(")
            .expect("handle_message must exist");
        let rest = &src[start + 30..];
        let end = rest
            .find("\nfn ")
            .or_else(|| rest.find("\nasync fn "))
            .or_else(|| rest.find("\npub fn "))
            .or_else(|| rest.find("\npub(crate) fn "))
            .or_else(|| rest.find("\npub(super) fn "))
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            !body.contains("block_on"),
            "handle_message must not call block_on (nested runtime panic). Found block_on in body."
        );
    }

    fn photo_attachment() -> crate::channel::event::Attachment {
        crate::channel::event::Attachment {
            kind: crate::channel::event::AttachmentKind::Photo,
            path: std::path::PathBuf::from("/tmp/x.jpg"),
            mime: None,
            caption: None,
            size_bytes: None,
            original_filename: None,
        }
    }

    /// #t-3 / #t-109: drives the REAL #1352 long/short delivery split — the gate
    /// is `is_short_inject`'s `< 200` char cap (was inlined at the handle_message
    /// call-site). Pins the production behavior (char-count, 200 cap, attachments
    /// force the long path), kept by operator decision (d-20260617102838730641-2)
    /// when the unused 300 `HEADER_SIZE_THRESHOLD` const was removed.
    #[test]
    fn is_short_inject_routes_by_char_count_and_attachments() {
        assert!(
            is_short_inject(&"a".repeat(199), &[]),
            "199 chars, no attachment → short (PTY inject)"
        );
        assert!(
            !is_short_inject(&"a".repeat(200), &[]),
            "200 chars → not short (cap is `< 200`)"
        );
        assert!(
            !is_short_inject(&"a".repeat(201), &[]),
            "201 chars → not short"
        );
        // Char count, not bytes: 199 CJK chars = 597 bytes but still short.
        assert!(
            is_short_inject(&"你".repeat(199), &[]),
            "199 CJK chars → short (counted by chars, not bytes)"
        );
        assert!(
            !is_short_inject(&"你".repeat(200), &[]),
            "200 CJK chars → not short"
        );
        // Any attachment forces the long (inbox + pointer) path regardless of length.
        assert!(
            !is_short_inject("hi", &[photo_attachment()]),
            "attachment present → not short even for tiny text"
        );
    }

    // ── Sprint 54 silent-drop hotfix: image+no-caption+download-fail ────
    //
    // Operator m-9 dispatch m-20260507090314193553-38. The pre-hotfix
    // path produced `text="" attachments=[]` when the user sent a
    // pure image with no caption and the download failed (network /
    // token / size). Agent had nothing to act on; user had no
    // visibility. Each test pins one of the four contract gates from
    // the dispatch.
    //
    // EMPIRICAL REGRESSION-PROOF ANCHOR: collapsing
    // `resolve_text_after_image_download_failure` to a no-op (always
    // return `initial.to_string()`) makes
    // `caption_empty_download_fail_uses_image_fallback_text` fail
    // because text stays empty. PR description carries the captured
    // FAIL signature.

    #[test]
    fn caption_present_passes_through_unchanged() {
        // Gate 1: a user who typed a caption shouldn't have the
        // fallback overwrite their own words on download failure.
        // The WARN still fires at the call site so operators see why
        // the attachment is missing.
        let result = resolve_text_after_image_download_failure("look at this cat");
        assert_eq!(result, "look at this cat");
    }

    #[test]
    fn caption_empty_download_succeeds_no_fallback_needed() {
        // Gate 2: when download succeeds, the resolver is never
        // called — the inbox enqueue carries the original (empty)
        // text + the populated attachment. This test pins the
        // resolver's behavioral contract (deterministic on empty
        // input) and the call-site invariant via source inspection
        // — the resolver call lives inside the `Err(e) =>` arm of
        // the download match, never on the `Ok(...)` success path.
        let resolved = resolve_text_after_image_download_failure("");
        assert_eq!(resolved, "[image attached but download failed]");

        // Source-level invariant: the production call-site (handle_message)
        // invokes the resolver only inside the Err arm. Skip both the
        // `fn` definition AND the test-suite call-sites; what remains
        // are production call-sites, all of which must follow `Err(e)`.
        let src = include_str!("inbound.rs");
        let test_module_start = src.find("\nmod tests {").expect("test module must exist");
        let production_src = &src[..test_module_start];

        let mut iter = production_src.match_indices("resolve_text_after_image_download_failure(");
        // First production hit is the fn signature itself. Skip.
        let _ = iter.next();
        let production_callsites: Vec<_> = iter.collect();
        assert_eq!(
            production_callsites.len(),
            1,
            "expected exactly one production call-site (in handle_message Err arm); \
             grep 'resolve_text_after_image_download_failure' to inspect."
        );
        let (idx, _) = production_callsites[0];
        // Look back generously — the call sits ~15 lines inside the
        // Err arm. 2048 bytes covers comfortable nesting without
        // accidentally matching unrelated `Err(e) =>` arms outside.
        let preceding = &production_src[idx.saturating_sub(2048)..idx];
        assert!(
            preceding.contains("Err(e) =>"),
            "production call must live inside the download match's Err arm — \
             a call from the Ok arm would re-introduce the silent-drop bug"
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn caption_empty_download_fail_uses_image_fallback_text() {
        // Gate 3 (regression-proof anchor): empty caption + download
        // failure must produce the user-visible fallback text AND
        // emit the WARN with sender_id + file_id. Both behaviors
        // matter for the silent-drop class — agents see content,
        // operators triage causes.
        let kind = crate::channel::event::AttachmentKind::Photo;
        emit_download_failure_warn("FILE_ABC", "404 Not Found", Some(12345), &kind);
        let resolved = resolve_text_after_image_download_failure("");
        assert_eq!(resolved, "[image attached but download failed]");
        // tracing-test scans captured records.
        assert!(
            logs_contain("inbound attachment download failed"),
            "WARN must fire so operators see download failures in app.log"
        );
        assert!(
            logs_contain("FILE_ABC"),
            "WARN must include file_id for triage"
        );
        assert!(
            logs_contain("12345"),
            "WARN must include sender_id (Sprint 54 enrichment)"
        );
    }

    #[test]
    fn pure_text_no_image_path_unchanged() {
        // Gate 4: the silent-drop fix targets the image+no-caption
        // case only. A pure text message hits neither helper because
        // `inbound_file` is None — the resolver is gated behind
        // `if is_image`. We pin the source-level invariant: the
        // resolver call-site is inside `if is_image`, so a non-image
        // message can never trigger the fallback.
        let src = include_str!("inbound.rs");
        // Find the call site (skip the fn definition).
        let fn_def_marker = "pub(super) fn resolve_text_after_image_download_failure(";
        let after_def = src
            .find(fn_def_marker)
            .map(|i| i + fn_def_marker.len())
            .expect("resolver definition must exist");
        let call_site = src[after_def..]
            .find("resolve_text_after_image_download_failure(")
            .expect("resolver must be called somewhere")
            + after_def;
        // Look back ~256 chars for the `if is_image` guard.
        let preceding_256 = &src[call_site.saturating_sub(256)..call_site];
        assert!(
            preceding_256.contains("if is_image"),
            "resolver call must be gated behind `if is_image`; found context: {preceding_256}"
        );
    }
}
