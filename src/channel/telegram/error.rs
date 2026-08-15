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
/// Returns whether the durable `topics.json` row is PROVABLY gone. #3232: the
/// in-memory teardown below always runs, but the caller must not report a
/// completed self-heal when the row survived — that row still maps the live
/// instance name, so the next same-name instance inherits the dead topic.
#[must_use]
pub(crate) fn cleanup_deleted_topic(
    home: &Path,
    instance_name: &str,
    tid: i32,
    state: Option<&Arc<Mutex<TelegramState>>>,
) -> bool {
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
    match unregister_topic(home, tid) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                instance = %instance_name,
                topic_id = tid,
                error = %e,
                "stale topic row could not be removed — a later same-name instance could inherit it"
            );
            false
        }
    }
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
/// Returns whether the durable `__fleet__` row is PROVABLY gone. The in-memory
/// binding is cleared either way — this process must stop writing to a dead
/// thread — but a surviving row is reused on the next daemon restart, so the
/// caller may not call that a completed self-heal (#3232).
#[must_use]
pub(crate) fn cleanup_fleet_binding(
    home: &Path,
    state: &Arc<Mutex<TelegramState>>,
    tid: i32,
) -> bool {
    let cleared = match unregister_topic(home, tid) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                topic_id = tid,
                error = %e,
                "fleet-binding row could not be removed — the next restart would reuse the dead thread"
            );
            false
        }
    };
    let mut s = lock_state(state);
    if s.fleet_binding_topic_id == Some(tid) {
        s.fleet_binding_topic_id = None;
    }
    cleared
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
    // #3232: "handled" means the self-heal COMPLETED. A surviving durable row
    // leaves the stale `__fleet__` mapping for the next restart to reuse, so
    // the outer send-failure log must not be silenced.
    cleanup_fleet_binding(home, state, tid)
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
    // #3232: same rule as the fleet path — an unremovable row means cleanup did
    // not complete, and reporting `handled` would hide exactly the residue that
    // lets a later same-name instance inherit this topic.
    cleanup_deleted_topic(home, instance_name, tid, state)
}

/// Classify a Telegram send error as "the bound chat was migrated to a
/// supergroup". The Bot API returns `migrate_to_chat_id` in the response
/// parameters when the configured `group_id` points at a regular group
/// that has been upgraded — typically because the operator enabled
/// Topics, which forces a supergroup. teloxide-core decodes that
/// parameter into [`teloxide::RequestError::MigrateToChatId`]; we match
/// on the typed variant rather than the description string so the
/// classifier is independent of locale / Bot API copy changes.
pub(crate) fn is_supergroup_migration_error(err: &anyhow::Error) -> bool {
    extract_migrated_chat_id(err).is_some()
}

/// Pull the new supergroup chat_id out of a migration error.
///
/// Returns `Some(new_id)` for [`RequestError::MigrateToChatId`], `None`
/// otherwise. Callers use this to (a) classify and (b) read the new
/// `-100…` prefixed id in a single pass.
pub(crate) fn extract_migrated_chat_id(err: &anyhow::Error) -> Option<i64> {
    err.downcast_ref::<teloxide::RequestError>().and_then(|e| {
        if let teloxide::RequestError::MigrateToChatId(teloxide::types::ChatId(id)) = e {
            Some(*id)
        } else {
            None
        }
    })
}

/// Self-heal path for supergroup migration: extract the new chat_id,
/// atomically rewrite `channel.group_id` in `fleet.yaml`, and update
/// `state.group_id` in-memory so cached-state callers (`Channel::send`,
/// `apply_fleet_action`) pick up the new id without a daemon restart.
///
/// Returns `Some(new_id)` when applied (caller may retry the failed
/// send with the new id). Returns `None` when (a) the error is not a
/// migration error, or (b) yaml persistence failed — in case (b) we log
/// and give up; the next migration error will retry persistence so the
/// daemon doesn't loop forever on a disk-IO failure.
///
/// `state` is `Option` because callers reached from non-state-aware
/// paths (`try_telegram_reply_from`) only have `home` to work with;
/// `fleet.yaml` is the source of truth, so a `None` call still heals
/// disk state and the next state-aware send picks up the fresh value.
pub(crate) fn handle_supergroup_migration(
    home: &Path,
    state: Option<&Arc<Mutex<TelegramState>>>,
    err: &anyhow::Error,
) -> Option<i64> {
    let new_id = extract_migrated_chat_id(err)?;
    tracing::warn!(
        new_chat_id = new_id,
        "telegram chat migrated to supergroup — rewriting fleet.yaml channel.group_id"
    );
    if let Err(e) = crate::fleet::update_channel_telegram_group_id(home, new_id) {
        tracing::error!(
            %e, new_chat_id = new_id,
            "failed to persist migrated group_id to fleet.yaml — next migration error will retry"
        );
        return None;
    }
    if let Some(state) = state {
        let mut s = lock_state(state);
        s.group_id = teloxide::types::ChatId(new_id);
    }
    Some(new_id)
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
    if let Err(e) = unregister_topic(home, stale_tid) {
        // The reuse check below matches on NAME, so a surviving row would be
        // handed straight back as "the existing topic" — refuse instead.
        tracing::error!(stale_topic_id = stale_tid, error = %e, "stale topic row persisted; refusing to recreate");
        return None;
    }
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
        load_topic_registry, register_topic, save_topic_registry, topic_registry_path,
        FLEET_BINDING_SENTINEL,
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

        assert!(cleanup_deleted_topic(&home, "agent1", 42, Some(&state)));

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
        save_topic_registry(&home, &reg).unwrap();

        assert!(cleanup_deleted_topic(&home, "ghost", 99, None));

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
        register_topic(&home, 42, "agent-x").unwrap();
        register_topic(&home, 99, "agent-y").unwrap();

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
    fn invalidate_and_recreate_clears_topics_json() {
        let home = tmp_home("invalidate-recreate-json");
        register_topic(&home, 42, "agent-x").unwrap();
        register_topic(&home, 99, "agent-y").unwrap();

        let _ = invalidate_and_recreate_topic(&home, "agent-x", 42);

        let reg = load_topic_registry(&home);
        assert!(
            !reg.contains_key(&42),
            "stale topic_id must be removed from topics.json"
        );
        assert_eq!(reg.get(&99), Some(&"agent-y".to_string()));
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
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).expect("write fleet.yaml");
        register_topic(&home, 42, "agent-x").unwrap();

        let _ = invalidate_and_recreate_topic(&home, "agent-x", 42);

        let fleet_yaml =
            std::fs::read_to_string(crate::fleet::fleet_yaml_path(&home)).expect("read fleet.yaml");
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
        save_topic_registry(&home, &reg).unwrap();
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

    // ── Sprint 56 Track A — supergroup migration self-heal ────────────

    fn migration_err(new_id: i64) -> anyhow::Error {
        anyhow::Error::from(teloxide::RequestError::MigrateToChatId(
            teloxide::types::ChatId(new_id),
        ))
    }

    fn write_telegram_fleet_yaml(home: &std::path::Path, group_id: i64, mode: &str) {
        let yaml = format!(
            "channel:\n  type: telegram\n  bot_token_env: AGEND_BOT_TOKEN\n  group_id: {group_id}\n  mode: {mode}\n  user_allowlist:\n  - 42\n"
        );
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).expect("write fleet.yaml");
    }

    fn channel_group_id_from_yaml(home: &std::path::Path) -> Option<i64> {
        let text = std::fs::read_to_string(crate::fleet::fleet_yaml_path(home)).ok()?;
        let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).ok()?;
        doc.get("channel")
            .and_then(|c| c.get("group_id"))
            .and_then(|v| v.as_i64())
    }

    #[test]
    fn classifier_matches_typed_migrate_to_chat_id() {
        let err = migration_err(-1001234567890);
        assert!(is_supergroup_migration_error(&err));
        assert_eq!(extract_migrated_chat_id(&err), Some(-1001234567890));
    }

    #[test]
    fn classifier_rejects_unrelated_request_errors() {
        // RetryAfter is also a RequestError but not a migration.
        let err = anyhow::Error::from(teloxide::RequestError::RetryAfter(
            teloxide::types::Seconds::from_seconds(5),
        ));
        assert!(!is_supergroup_migration_error(&err));
        assert_eq!(extract_migrated_chat_id(&err), None);
    }

    #[test]
    fn classifier_rejects_anyhow_string_errors() {
        // Stringly-typed errors (anyhow::anyhow!) carry no typed
        // RequestError payload — match must miss, not false-positive.
        for msg in [
            "network timeout",
            "Bad Request: message thread not found",
            "Too Many Requests: retry after 5",
            "Bad Request: chat upgraded to supergroup", // legacy description text alone
        ] {
            let err = anyhow::anyhow!(msg.to_string());
            assert!(
                !is_supergroup_migration_error(&err),
                "must not match string-only: {msg}"
            );
            assert_eq!(extract_migrated_chat_id(&err), None);
        }
    }

    #[test]
    fn handle_migration_persists_to_fleet_yaml_and_returns_new_id() {
        let home = tmp_home("migration-persist");
        write_telegram_fleet_yaml(&home, -100111111, "topic");
        let new_id = -1009999999999_i64;

        let applied = handle_supergroup_migration(&home, None, &migration_err(new_id));

        assert_eq!(applied, Some(new_id));
        assert_eq!(channel_group_id_from_yaml(&home), Some(new_id));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn handle_migration_with_state_mutates_in_memory_group_id() {
        let home = tmp_home("migration-state");
        write_telegram_fleet_yaml(&home, -100111111, "topic");
        let new_id = -1008888888888_i64;
        let state = Arc::new(Mutex::new(TelegramState::new(
            "tok",
            -100111111,
            HashMap::new(),
            home.clone(),
            HashMap::new(),
            None,
        )));

        let applied = handle_supergroup_migration(&home, Some(&state), &migration_err(new_id));

        assert_eq!(applied, Some(new_id));
        assert_eq!(lock_state(&state).group_id.0, new_id);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn handle_migration_returns_none_for_unrelated_error() {
        let home = tmp_home("migration-unrelated");
        write_telegram_fleet_yaml(&home, -100222222, "topic");
        let unrelated = anyhow::anyhow!("network timeout");

        let applied = handle_supergroup_migration(&home, None, &unrelated);

        assert!(applied.is_none());
        // fleet.yaml must not be mutated when the error is unrelated.
        assert_eq!(channel_group_id_from_yaml(&home), Some(-100222222));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn handle_migration_is_idempotent_when_yaml_already_has_new_id() {
        // If a parallel send already healed fleet.yaml, the second call's
        // mutate_fleet_yaml is a no-op-style overwrite (same value); the
        // function must still return Some(new_id) so the caller retries
        // the original send rather than treating it as fatal.
        let home = tmp_home("migration-idempotent");
        let new_id = -1007777777777_i64;
        write_telegram_fleet_yaml(&home, new_id, "topic");

        let applied = handle_supergroup_migration(&home, None, &migration_err(new_id));

        assert_eq!(applied, Some(new_id));
        assert_eq!(channel_group_id_from_yaml(&home), Some(new_id));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fleet_binding_self_heal_ignores_unrelated_errors() {
        let home = tmp_home("fleet-self-heal-neg");
        let mut reg = HashMap::new();
        reg.insert(42, FLEET_BINDING_SENTINEL.to_string());
        save_topic_registry(&home, &reg).unwrap();
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

    // ── #3232 — a surviving durable row is not a completed self-heal ──────
    //
    // Both handlers report `handled`, which the renderer uses to SILENCE the
    // outer "send failed" log. `store::fail_next_atomic_write_for_test` forces
    // the one `topics.json` write inside `unregister_topic` to fail, so the
    // residue is real rather than simulated: the row is still on disk, still
    // mapping the name the next instance would look up. Each test then repeats
    // the same call unarmed as a negative control, proving the `false` came
    // from the write failure and not from the fixture's shape.

    #[test]
    fn handle_send_failure_is_unhandled_when_the_row_survives_3232() {
        let home = tmp_home("send-failure-residue");
        let mut reg = HashMap::new();
        reg.insert(42, "agent1".to_string());
        reg.insert(100, "alive".to_string());
        save_topic_registry(&home, &reg).unwrap();
        let mut topic_map = HashMap::new();
        topic_map.insert("agent1".to_string(), 42);
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
        let err = anyhow::anyhow!("Bad Request: message thread not found");

        crate::store::fail_next_atomic_write_for_test(&topic_registry_path(&home));
        assert!(
            !handle_send_failure(&err, &home, "agent1", Some(42), Some(&state)),
            "an unremovable topics.json row must not be reported as handled"
        );

        let after = load_topic_registry(&home);
        assert_eq!(
            after.get(&42),
            Some(&"agent1".to_string()),
            "the residue is real: the row still maps the live name a later \
             same-name instance would inherit"
        );
        assert_eq!(after.get(&100), Some(&"alive".to_string()));
        {
            // In-memory teardown runs either way — this process must stop
            // routing to the dead topic even when the durable row survives.
            let s = lock_state(&state);
            assert!(!s.topic_to_instance.contains_key(&42));
            assert!(!s.instance_to_topic.contains_key("agent1"));
            assert!(!s.submit_keys.contains_key("agent1"));
        }

        // Negative control: same call, no forced failure.
        assert!(
            handle_send_failure(&err, &home, "agent1", Some(42), Some(&state)),
            "a removable row is a completed self-heal"
        );
        let healed = load_topic_registry(&home);
        assert!(!healed.contains_key(&42));
        assert_eq!(healed.get(&100), Some(&"alive".to_string()));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn handle_fleet_send_failure_is_unhandled_when_the_sentinel_row_survives_3232() {
        let home = tmp_home("fleet-failure-residue");
        let mut reg = HashMap::new();
        reg.insert(42, FLEET_BINDING_SENTINEL.to_string());
        reg.insert(100, "at-dev-1".to_string());
        save_topic_registry(&home, &reg).unwrap();
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

        crate::store::fail_next_atomic_write_for_test(&topic_registry_path(&home));
        assert!(
            !handle_fleet_send_failure(&err, &home, &state, 42),
            "an unremovable __fleet__ row must not be reported as handled"
        );

        assert_eq!(
            load_topic_registry(&home).get(&42),
            Some(&FLEET_BINDING_SENTINEL.to_string()),
            "the residue is real: the next daemon restart would resolve this \
             row and write to the dead thread"
        );
        // The in-memory binding is cleared regardless, so this process stops
        // emitting into the deleted topic.
        assert_eq!(lock_state(&state).fleet_binding_topic_id, None);

        // Negative control: same call, no forced failure.
        assert!(
            handle_fleet_send_failure(&err, &home, &state, 42),
            "a removable sentinel row is a completed self-heal"
        );
        let healed = load_topic_registry(&home);
        assert!(!healed.values().any(|v| v == FLEET_BINDING_SENTINEL));
        assert_eq!(healed.get(&100), Some(&"at-dev-1".to_string()));
        std::fs::remove_dir_all(&home).ok();
    }
}
