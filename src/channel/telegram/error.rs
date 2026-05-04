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
