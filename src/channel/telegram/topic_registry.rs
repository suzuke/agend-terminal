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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use crate::channel::telegram::inbound::resolve_topic;
    use crate::channel::telegram::state::TelegramState;

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
        let mut state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            home.clone(),
            HashMap::new(),
            None,
        );
        assert_eq!(resolve_topic(&mut state, Some(229)), "alice");
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
        assert_eq!(resolve_topic(&mut state, Some(500)), "bob");
        std::fs::remove_file(home.join("fleet.yaml")).ok();
        assert_eq!(resolve_topic(&mut state, Some(500)), "bob");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_topic_falls_back_to_topics_json() {
        let home = tmp_home("resolve_topics_json");
        let mut reg = HashMap::new();
        reg.insert(2474, "test-gemini".to_string());
        save_topic_registry(&home, &reg);
        let mut state = TelegramState::new_for_contract_test(
            -1,
            HashMap::new(),
            home.clone(),
            HashMap::new(),
            None,
        );
        assert_eq!(
            resolve_topic(&mut state, Some(2474)),
            "test-gemini",
            "topic in topics.json must resolve, not fall through to general"
        );
        assert_eq!(resolve_topic(&mut state, Some(2474)), "test-gemini");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn register_topic_writes_fleet_yaml() {
        let home = tmp_home("register_writes_yaml");
        let yaml = "defaults:\n  backend: claude\ninstances:\n  alice:\n    backend: claude\n";
        std::fs::write(home.join("fleet.yaml"), yaml).ok();
        register_topic(&home, 42, "alice");
        let content =
            std::fs::read_to_string(home.join("fleet.yaml")).expect("fleet.yaml must exist");
        assert!(
            content.contains("topic_id"),
            "fleet.yaml must contain topic_id after register_topic: {content}"
        );
        let reg = load_topic_registry(&home);
        assert_eq!(reg.get(&42), Some(&"alice".to_string()));
        std::fs::remove_dir_all(&home).ok();
    }

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

    #[test]
    fn create_topic_for_instance_uses_passed_home_not_real_home() {
        let home = tmp_home("topic-helper-home-scope");
        let result = create_topic_for_instance(&home, "regression-pin");
        assert!(
            result.is_none(),
            "missing fleet.yaml in the passed home must suppress the API call, got {result:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn delete_topic_uses_passed_home_not_real_home() {
        let home = tmp_home("delete-topic-home-scope");
        delete_topic(&home, 999_999);
        std::fs::remove_dir_all(&home).ok();
    }

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

    #[test]
    fn create_topic_for_instance_reuses_existing_topic() {
        let home = tmp_home("create_reuse");
        register_topic(&home, 77, "reuse-agent");
        let result = create_topic_for_instance(&home, "reuse-agent");
        assert_eq!(
            result,
            Some(77),
            "must reuse existing topic, not create new"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn create_topic_for_instance_returns_none_without_config() {
        let home = tmp_home("create_no_config");
        let result = create_topic_for_instance(&home, "new-agent");
        assert!(result.is_none(), "no config → no topic creation");
        std::fs::remove_dir_all(&home).ok();
    }
}
