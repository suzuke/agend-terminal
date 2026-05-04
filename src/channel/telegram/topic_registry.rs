use std::collections::HashMap;
use std::path::{Path, PathBuf};
use teloxide::prelude::Requester;

use super::state::telegram_runtime;

/// Reserved pseudo-instance name used in `topics.json` to pin the
/// `fleet_binding` topic across daemon restarts. Not a real instance —
/// chosen so it can never collide with a user-configured name
/// (`fleet.yaml` keys are slugs; underscores-bracketing is reserved).
/// See [`init_from_config`] orphan-cleanup filter and fleet-binding
/// resolution.
pub(super) const FLEET_BINDING_SENTINEL: &str = "__fleet__";

pub(super) fn topic_registry_path(home: &Path) -> PathBuf {
    home.join("topics.json")
}

pub(super) fn load_topic_registry(home: &Path) -> HashMap<i32, String> {
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

pub(super) fn save_topic_registry(home: &Path, registry: &HashMap<i32, String>) {
    let map: HashMap<String, &String> = registry.iter().map(|(k, v)| (k.to_string(), v)).collect();
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        // H2: atomic write to prevent partial-file on crash
        let _ = crate::store::atomic_write(&topic_registry_path(home), json.as_bytes());
    }
}

pub(super) fn register_topic(home: &Path, topic_id: i32, instance_name: &str) {
    // Write-side unification (Fix B): all three sources updated atomically.
    // 1. topics.json (disk registry)
    let mut reg = load_topic_registry(home);
    reg.insert(topic_id, instance_name.to_string());
    save_topic_registry(home, &reg);
    // 2. fleet.yaml topic_id field
    let _ = crate::fleet::update_instance_field(
        home,
        instance_name,
        "topic_id",
        serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(topic_id)),
    );
    // 3. in-memory state is updated by the caller (Channel trait methods
    //    that hold &self.state). Free-function callers without state access
    //    rely on resolve_topic's topics.json fallback as defense-in-depth.
}

pub(super) fn unregister_topic(home: &Path, topic_id: i32) {
    let mut reg = load_topic_registry(home);
    reg.remove(&topic_id);
    save_topic_registry(home, &reg);
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
    let ch = super::resolve_channel_only_from(home).ok()?;
    match telegram_runtime().block_on(async {
        let bot = teloxide::Bot::new(&ch.token);
        let topic = bot
            .create_forum_topic(
                teloxide::types::ChatId(ch.group_id),
                instance_name,
                teloxide::types::Rgb::from_u32(0x6FB9F0),
                "",
            )
            .await?;
        Ok::<i32, anyhow::Error>(topic.thread_id.0 .0)
    }) {
        Ok(tid) => {
            tracing::info!(instance = %instance_name, topic_id = tid, "created topic");
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
    let ch = match super::resolve_channel_only_from(home) {
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
