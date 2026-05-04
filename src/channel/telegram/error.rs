use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;

use super::state::{lock_state, TelegramState};
use super::topic_registry::{create_topic_for_instance, unregister_topic};

/// Classify a send error as "the bound topic was deleted out from under us".
///
/// Bot API 6.3+ exposes no typed variant or deletion service message for
/// "thread gone"; it surfaces as `ApiError::Unknown("Bad Request: message
/// thread not found")`. Substring match on the flattened chain tolerates both
/// `anyhow::context` wrapping and future teloxide wrapping changes.
pub(super) const TOPIC_DELETED_MARKER: &str = "message thread not found";

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
        serde_yaml_ng::Value::Null,
    );
    create_topic_for_instance(home, instance_name)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use crate::channel::telegram::state::TelegramState;
    use crate::channel::telegram::topic_registry::{
        load_topic_registry, register_topic, save_topic_registry, FLEET_BINDING_SENTINEL,
    };

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
    fn is_topic_deleted_error_matches_thread_not_found() {
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
        let inner = anyhow::anyhow!("Bad Request: message thread not found");
        let wrapped = inner.context("sending to topic 42");
        assert!(is_topic_deleted_error(&wrapped));
    }

    #[test]
    fn is_topic_deleted_error_rejects_unrelated() {
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
        assert_eq!(s.topic_to_instance.get(&43), Some(&"agent2".to_string()));
        assert_eq!(s.instance_to_topic.get("agent2"), Some(&43));
    }

    #[test]
    fn cleanup_deleted_topic_unregisters_topic() {
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
        let unrelated = anyhow::anyhow!("network timeout");
        assert!(!handle_send_failure(
            &unrelated,
            &home,
            "any",
            Some(42),
            None
        ));
        let gone = anyhow::anyhow!("Bad Request: message thread not found");
        assert!(!handle_send_failure(&gone, &home, "any", None, None));
        assert!(handle_send_failure(&gone, &home, "any", Some(42), None));
    }

    #[test]
    fn invalidate_and_recreate_strips_stale_topic_from_registry() {
        let home = tmp_home("invalidate-recreate-strip");
        register_topic(&home, 42, "agent-x");
        register_topic(&home, 99, "agent-y");

        let result = invalidate_and_recreate_topic(&home, "agent-x", 42);
        assert!(result.is_none(), "no bot → creation fails");

        let reg = load_topic_registry(&home);
        assert!(
            !reg.contains_key(&42),
            "stale topic 42 must be unregistered"
        );
        assert_eq!(reg.get(&99), Some(&"agent-y".to_string()));
        std::fs::remove_dir_all(&home).ok();
    }

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

        let config =
            crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).expect("load fleet.yaml");
        let agent_x = config.instances.get("agent-x").expect("agent-x exists");
        assert_eq!(
            agent_x.topic_id, None,
            "topic_id must be cleared after invalidation"
        );
        let agent_y = config.instances.get("agent-y").expect("agent-y exists");
        assert_eq!(agent_y.topic_id, Some(99));
        std::fs::remove_dir_all(&home).ok();
    }

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

    #[test]
    fn fleet_binding_self_heals_when_topic_deleted() {
        let home = tmp_home("fleet-self-heal");
        let mut reg = HashMap::new();
        reg.insert(42, FLEET_BINDING_SENTINEL.to_string());
        reg.insert(100, "at-dev-1".to_string());
        save_topic_registry(&home, &reg);
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
        let err = anyhow::anyhow!("Bad Request: message thread not found");
        let handled = handle_fleet_send_failure(&err, &home, &state, 42);
        assert!(handled);
        assert_eq!(lock_state(&state).fleet_binding_topic_id, None);
        let reg_after = load_topic_registry(&home);
        assert!(!reg_after.values().any(|v| v == FLEET_BINDING_SENTINEL));
        assert_eq!(reg_after.get(&100), Some(&"at-dev-1".to_string()));
        std::fs::remove_dir_all(&home).ok();
    }

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
            assert!(!handle_fleet_send_failure(&err, &home, &state, 42));
        }
        assert_eq!(lock_state(&state).fleet_binding_topic_id, Some(42));
        let reg_after = load_topic_registry(&home);
        assert_eq!(
            reg_after.get(&42),
            Some(&FLEET_BINDING_SENTINEL.to_string())
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
