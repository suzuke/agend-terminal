//! Telegram bootstrap — init_from_config, attach_registry, resolve_fleet_binding.

use crate::agent::AgentRegistry;
use crate::channel::telegram::inbound::*;
use crate::channel::telegram::state::*;
use crate::channel::telegram::topic_registry::*;
use crate::fleet::ChannelConfig;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use teloxide::prelude::Requester;

/// Wire the in-process [`AgentRegistry`] into an already-initialized
/// [`TelegramState`].
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
        Err(_) => match std::env::var("AGEND_BOT_TOKEN") {
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
        },
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
    // Split the allowlist into the bare-id list (authz, unchanged downstream)
    // and an id→name map (display: surfaced as `[user:NAME via telegram]` when
    // the sender has no public @username).
    let allowlist_ids: Option<Vec<i64>> = user_allowlist
        .as_ref()
        .map(|l| l.iter().map(|e| e.id()).collect());
    // Sanitize each configured name at ingestion (the single chokepoint): the
    // stored map — and therefore every consumer via `username_for` (both inbound
    // paths + status-summary) — holds only header-safe names. A name that
    // sanitizes to empty is dropped, so its sender falls back to `unknown`.
    let user_names: HashMap<i64, String> = user_allowlist
        .as_ref()
        .map(|l| {
            l.iter()
                .filter_map(|e| {
                    e.name()
                        .and_then(super::state::sanitize_display_name)
                        .map(|n| (e.id(), n))
                })
                .collect()
        })
        .unwrap_or_default();

    // Clean up orphaned topics
    let mut reg = load_topic_registry(home);
    let instance_names: std::collections::HashSet<&String> = config.instances.keys().collect();
    let mut orphan_count = 0;
    for (tid, inst_name) in reg.clone() {
        if tid != 1 && inst_name != FLEET_BINDING_SENTINEL && !instance_names.contains(&inst_name) {
            tracing::info!(topic_id = tid, instance = %inst_name, "orphaned topic, deleting");
            delete_topic(home, tid);
            orphan_count += 1;
        }
    }
    if orphan_count > 0 {
        reg = load_topic_registry(home);
        tracing::info!(count = orphan_count, "cleaned up orphaned topics");
    }

    let bot = teloxide::Bot::new(&token);
    let chat_id = teloxide::types::ChatId(*group_id);

    let mut topic_map: HashMap<String, i32> = reg
        .iter()
        .filter(|(tid, name)| **tid != 1 && name.as_str() != FLEET_BINDING_SENTINEL)
        .map(|(tid, name)| (name.clone(), *tid))
        .collect();

    // Auto-create topics for instances without topic_id.
    //
    // Sprint 59 Wave 2 PR-IMPL (F2 — α-a' track-on-create refactor):
    // route through `create_topic_for_instance` instead of inline
    // `bot.create_forum_topic`. This closes S1 (duplicate accumulation
    // on registry-state loss): `create_topic_for_instance` at
    // topic_registry.rs:74-79 already implements idempotent same-
    // name reuse — if a topic with the same instance name is in
    // `topics.json`, it returns the existing topic_id rather than
    // calling the create API again. Bootstrap now benefits from
    // the same dedup. Combined with the existing orphan-cleanup at
    // bootstrap.rs:71-78 (which scans topics.json for retired
    // instances), the (α-a)+(α-b) pair from RCA design collapses
    // into a single track-on-create flow + existing orphan scan.
    //
    // Note: chat-side enumeration to detect "duplicate-named topic
    // already exists in chat but not in topics.json" remains
    // technically impossible per teloxide 0.11.2 + Telegram Bot API
    // gap (no list_forum_topics method). That edge case requires
    // operator intervention via the (γ) `agend-terminal doctor
    // topics` surface (Sprint 60+ candidate: teloxide upgrade
    // evaluation if a future Bot API version exposes enumeration).
    for name in config.instances.keys() {
        if topic_map.contains_key(name.as_str()) {
            continue;
        }
        if name == "general" {
            topic_map.insert("general".to_string(), 1);
            if let Err(e) = register_topic(home, 1, "general") {
                tracing::warn!(error = %e, "failed to register general topic");
            }
        } else {
            tracing::info!(instance = %name, "auto-creating topic via track-on-create");
            if let Some(tid) = create_topic_for_instance(home, name) {
                topic_map.insert(name.clone(), tid);
            }
        }
    }

    // Ensure topic registry reflects any auto-created entries
    for (name, tid) in &topic_map {
        reg.insert(*tid, name.clone());
    }
    if let Err(e) = save_topic_registry(home, &reg) {
        tracing::warn!(error = %e, "failed to save topic registry");
    }

    let fleet_binding_topic_id =
        resolve_fleet_binding(&bot, chat_id, home, &mut reg, fleet_binding);

    let mut raw_state = TelegramState::new(
        &token,
        *group_id,
        topic_map,
        home.to_path_buf(),
        submit_keys,
        allowlist_ids,
    );
    raw_state.fleet_binding_topic_id = fleet_binding_topic_id;
    raw_state.user_names = user_names;
    let state = Arc::new(Mutex::new(raw_state));
    start_polling(Arc::clone(&state));
    Some(state)
}

/// Resolve the `fleet_binding` block to a concrete Telegram forum topic id.
pub(super) fn resolve_fleet_binding(
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
    match block_on_value(async { bot.create_forum_topic(chat_id, &name).await }) {
        Ok(topic) => {
            let tid = topic.thread_id.0 .0;
            tracing::info!(topic_id = tid, %name, "created fleet_binding topic");
            reg.insert(tid, FLEET_BINDING_SENTINEL.to_string());
            let _ = save_topic_registry(home, reg);
            Some(tid)
        }
        Err(e) => {
            tracing::error!(error = %e, %name, "failed to create fleet_binding topic");
            None
        }
    }
}
