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
    let allowlist = user_allowlist.clone();

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

    let mut topic_map: HashMap<String, i32> = config
        .instances
        .iter()
        .filter_map(|(name, inst)| inst.topic_id.map(|tid| (name.clone(), tid)))
        .collect();

    // Merge topics.json entries not already in fleet.yaml.
    for (tid, inst_name) in &reg {
        if *tid != 1 && inst_name != FLEET_BINDING_SENTINEL && !topic_map.contains_key(inst_name) {
            tracing::info!(
                instance = %inst_name, topic_id = tid,
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

    // Write back topic_ids + update registry
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
