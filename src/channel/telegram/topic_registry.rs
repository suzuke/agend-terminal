use std::collections::HashMap;
use std::path::{Path, PathBuf};
use teloxide::prelude::Requester;

use super::state::block_on_value;

/// Reserved pseudo-instance name used in `topics.json` to pin the
/// `fleet_binding` topic across daemon restarts. Not a real instance —
/// chosen so it can never collide with a user-configured name
/// (`fleet.yaml` keys are slugs; underscores-bracketing is reserved).
/// See [`init_from_config`] orphan-cleanup filter and fleet-binding
/// resolution.
pub(crate) const FLEET_BINDING_SENTINEL: &str = "__fleet__";

pub(crate) fn topic_registry_path(home: &Path) -> PathBuf {
    home.join("topics.json")
}

pub(crate) fn load_topic_registry(home: &Path) -> HashMap<i32, String> {
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

pub(crate) fn save_topic_registry(
    home: &Path,
    registry: &HashMap<i32, String>,
) -> anyhow::Result<()> {
    let map: HashMap<String, &String> = registry.iter().map(|(k, v)| (k.to_string(), v)).collect();
    let json = serde_json::to_string_pretty(&map)?;
    crate::store::atomic_write(&topic_registry_path(home), json.as_bytes())?;
    Ok(())
}

pub(crate) fn register_topic(
    home: &Path,
    topic_id: i32,
    instance_name: &str,
) -> anyhow::Result<()> {
    // #1886 C1: locked read-modify-write — the flock spans load→insert→save so
    // two concurrent registrations (e.g. team-spawn registering N members) can't
    // each read the same map and clobber the other's insert. Operate on the
    // on-disk `topic_id-string → name` form (matches save_topic_registry) so the
    // round-trip is byte-identical to the prior load/save helpers.
    crate::store::with_json_state_or_create::<HashMap<String, String>, _, _, _>(
        &topic_registry_path(home),
        HashMap::new,
        |reg| {
            reg.insert(topic_id.to_string(), instance_name.to_string());
        },
    )?;
    Ok(())
}

pub(crate) fn unregister_topic(home: &Path, topic_id: i32) {
    // #1886 C1: same locked-RMW discipline as register_topic.
    let _ = crate::store::with_json_state_or_create::<HashMap<String, String>, _, _, _>(
        &topic_registry_path(home),
        HashMap::new,
        |reg| {
            reg.remove(&topic_id.to_string());
        },
    );
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
    //
    // #2550: this reuse is trusted without verifying the topic still exists /
    // still means what the mapping claims — the Bot API has no read-only
    // "does this forum topic still exist" call (only state-changing
    // edit/close/reopen/delete/unpin methods), so a clean, side-effect-free
    // verification isn't available. `full_delete_instance` unregisters this
    // mapping unconditionally on delete (regardless of whether the Telegram-
    // side delete itself succeeded), which closes the main way a stale
    // mapping used to survive to be reused here. The remaining exposure is
    // narrow: the Telegram-side topic vanishing unilaterally (not through our
    // own delete path) or the daemon crashing mid-teardown before reaching
    // the unregister step above — accepted residual risk, not guarded here.
    if let Some(tid) = lookup_topic_for_instance(home, instance_name) {
        tracing::info!(instance = %instance_name, topic_id = tid, "reusing existing topic");
        return Some(tid);
    }
    let ch = super::resolve_channel_only_from(home).ok()?;
    match block_on_value(async {
        let bot = teloxide::Bot::new(&ch.token);
        let topic = bot
            .create_forum_topic(teloxide::types::ChatId(ch.group_id), instance_name)
            .await?;
        Ok::<i32, anyhow::Error>(topic.thread_id.0 .0)
    }) {
        Ok(tid) => {
            tracing::info!(instance = %instance_name, topic_id = tid, "created topic");
            if let Err(e) = register_topic(home, tid, instance_name) {
                tracing::warn!(instance = %instance_name, topic_id = tid, error = %e, "failed to register topic");
                return None;
            }
            Some(tid)
        }
        Err(e) => {
            tracing::error!(instance = %instance_name, error = %e, "failed to create topic");
            None
        }
    }
}

/// #991: outcome of a `bind_topic` retrofit call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindTopicOutcome {
    /// Topic created; fleet.yaml + topics.json updated.
    Bound(i32),
    /// Instance already had a topic — idempotent no-op, nothing written.
    AlreadyBound(i32),
    /// `topic_binding_mode` is `"skip"` — retrofit refused.
    NotEligible { reason: String },
    /// `instance_name` not found in fleet.yaml.
    InstanceNotFound,
    /// Telegram channel unavailable (no bot token, unconfigured, or the
    /// ~6s post-boot `telegram_init` window — see
    /// BIND-TOPIC-PRERESEARCH.md §3).
    ChannelUnavailable,
    /// Telegram API call failed (network / rate limit / other).
    ApiError(String),
}

/// #991 Phase 2: retrofit a Telegram topic for an instance that was spawned
/// with `topic_binding=deferred` (or any mode that ended up without a topic).
/// Operator-triggered via the `bind_topic` MCP action — not automatic, so
/// unlike the spawn-time path a `ChannelUnavailable` here is a structured,
/// caller-visible result the operator can simply retry (no silent-skip
/// window concern — see BIND-TOPIC-PRERESEARCH.md §3).
///
/// `topic_binding_mode == "skip"` is refused (`NotEligible`): skip's literal
/// promise is "no topic, ever" — retrofitting it silently would defeat the
/// point of recording that choice explicitly in fleet.yaml. To bind a
/// skip-mode instance, change its `topic_binding_mode` first, then call this.
pub fn bind_topic_for_instance(home: &std::path::Path, instance_name: &str) -> BindTopicOutcome {
    let Ok(config) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) else {
        return BindTopicOutcome::InstanceNotFound;
    };
    let Some(inst) = config.instances.get(instance_name) else {
        return BindTopicOutcome::InstanceNotFound;
    };
    if inst.topic_binding_mode.as_deref() == Some("skip") {
        return BindTopicOutcome::NotEligible {
            reason: format!(
                "instance '{instance_name}' is in skip mode (topic_binding_mode: skip) — \
                 change topic_binding_mode before binding a topic"
            ),
        };
    }
    if let Some(tid) = lookup_topic_for_instance(home, instance_name) {
        return BindTopicOutcome::AlreadyBound(tid);
    }
    match create_topic_for_instance(home, instance_name) {
        Some(tid) => {
            if let Err(e) = crate::fleet::update_instance_field(
                home,
                instance_name,
                "topic_id",
                serde_yaml_ng::Value::Number(tid.into()),
            ) {
                tracing::warn!(
                    instance = %instance_name, topic_id = tid, error = %e,
                    "bind_topic: topics.json updated but fleet.yaml topic_id write failed"
                );
            }
            BindTopicOutcome::Bound(tid)
        }
        None => {
            // `create_topic_for_instance` collapses "no channel" and "API
            // error" into a single `None` (see its own doc comment — internal
            // callers don't need the distinction). `bind_topic` is an
            // operator-facing action that does, so re-derive the channel
            // check here (cheap: local fleet.yaml/env read, no network)
            // rather than plumbing a richer return type through every
            // existing caller of `create_topic_for_instance`.
            match super::resolve_channel_only_from(home) {
                Ok(_) => BindTopicOutcome::ApiError(
                    "telegram API call failed creating the topic — see daemon logs".to_string(),
                ),
                Err(_) => BindTopicOutcome::ChannelUnavailable,
            }
        }
    }
}

/// Sprint 59 Wave 2 PR-IMPL (F2 — α-shared helper): query whether
/// the bot has `can_manage_topics` permission in the chat. Returns
/// `false` on any error path (network failure / not-an-admin /
/// permission-introspection failure) so callers default to safe-
/// skip rather than risking a permission-denied API call mid-flight.
///
/// Used by [`delete_topic`] (α-c surfacing) and the (γ) `--cleanup`
/// flag pre-call check.
pub fn can_manage_topics_for(bot: &teloxide::Bot, chat_id: teloxide::types::ChatId) -> bool {
    let me = match block_on_value(async { bot.get_me().await }) {
        Ok(me) => me,
        Err(_) => return false,
    };
    let member = match block_on_value(async { bot.get_chat_member(chat_id, me.id).await }) {
        Ok(m) => m,
        Err(_) => return false,
    };
    match member.kind {
        teloxide::types::ChatMemberKind::Administrator(admin) => admin.can_manage_topics,
        teloxide::types::ChatMemberKind::Owner(_) => true, // chat owners have all rights
        _ => false,
    }
}

/// Sprint 59 Wave 2 PR-IMPL (F2 — α-c): outcome of a `delete_topic`
/// call, surfaced to callers so the (γ) `--cleanup` flag and
/// future operator-driven cleanup paths can distinguish silent-
/// success from permission-denied-skip from genuine error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteTopicOutcome {
    /// Topic was deleted successfully on chat side + unregistered
    /// from `topics.json`.
    Deleted,
    /// Bot lacks `can_manage_topics` permission. Topic remains in
    /// chat AND in `topics.json` (operator must manually delete via
    /// Telegram UI OR fix bot permissions then retry).
    PermissionDenied,
    /// Telegram API returned a non-permission error (network,
    /// rate limit, transient). Topic state on chat side is
    /// indeterminate; registry left untouched so a retry can
    /// re-attempt cleanup.
    ApiError(String),
    /// Channel resolution failed (no telegram channel configured
    /// or bot token missing). Topic remains in registry; no chat-
    /// side attempt was made.
    ChannelUnavailable,
}

/// Delete a forum topic.
///
/// Sprint 59 Wave 2 PR-IMPL (F2 — α-c): replaces the prior
/// `let _ = ...` swallowing with explicit match arms that
/// distinguish permission errors (warn-log with actionable hint)
/// from generic errors (error-log with full chain). Returns
/// [`DeleteTopicOutcome`] so callers can branch on outcome.
///
/// Pre-flight permission check via [`can_manage_topics_for`]
/// short-circuits the API call when the bot lacks the
/// `can_manage_topics` admin right — avoids a guaranteed-fail
/// API roundtrip + surfaces the actionable hint immediately.
pub fn delete_topic(home: &std::path::Path, topic_id: i32) -> DeleteTopicOutcome {
    let ch = match super::resolve_channel_only_from(home) {
        Ok(c) => c,
        Err(_) => {
            tracing::warn!(
                topic_id,
                "delete_topic: telegram channel unavailable — skipping"
            );
            return DeleteTopicOutcome::ChannelUnavailable;
        }
    };
    let bot = teloxide::Bot::new(&ch.token);
    let chat_id = teloxide::types::ChatId(ch.group_id);

    if !can_manage_topics_for(&bot, chat_id) {
        tracing::warn!(
            topic_id,
            "delete_topic skipped: bot lacks can_manage_topics permission. \
             Grant via Telegram → Chat → Manage admins → bot name → enable \
             'Manage topics'. Topic remains in chat AND topics.json registry."
        );
        return DeleteTopicOutcome::PermissionDenied;
    }

    let tid = teloxide::types::ThreadId(teloxide::types::MessageId(topic_id));
    let result = block_on_value(async {
        // close_forum_topic is best-effort — close errors are
        // non-fatal because the subsequent delete_forum_topic
        // will close the topic anyway.
        let _ = bot.close_forum_topic(chat_id, tid).await;
        bot.delete_forum_topic(chat_id, tid).await
    });
    match result {
        Ok(_) => {
            unregister_topic(home, topic_id);
            tracing::info!(topic_id, "delete_topic: deleted topic + unregistered");
            DeleteTopicOutcome::Deleted
        }
        Err(e) => {
            let err_str = e.to_string();
            tracing::error!(
                topic_id,
                error = %err_str,
                "delete_topic: API error — topic NOT deleted, registry unchanged for retry"
            );
            DeleteTopicOutcome::ApiError(err_str)
        }
    }
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
    fn concurrent_register_topic_no_lost_update_1886() {
        // #1886 C1 §3.9: N threads each register a DISTINCT topic on the same
        // topics.json. The locked RMW (flock spans load→insert→save) keeps
        // every mapping; the prior unlocked load+save would clobber updates
        // under contention.
        let home = tmp_home("concurrent-register-1886");
        const N: i32 = 12;
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let home = home.clone();
                std::thread::spawn(move || {
                    register_topic(&home, i, &format!("inst-{i}")).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let reg = load_topic_registry(&home);
        assert_eq!(
            reg.len(),
            N as usize,
            "every concurrent registration must survive"
        );
        for i in 0..N {
            assert_eq!(
                reg.get(&i).map(String::as_str),
                Some(format!("inst-{i}").as_str())
            );
        }
    }

    #[test]
    fn register_unregister_rmw_preserves_other_entries_1886() {
        // #1886 C1: register reloads the on-disk map under the lock and ADDS to
        // it (not a blind overwrite); unregister removes one and leaves the rest.
        let home = tmp_home("register-preserve-1886");
        register_topic(&home, 1, "alpha").unwrap();
        register_topic(&home, 2, "beta").unwrap();
        let reg = load_topic_registry(&home);
        assert_eq!(reg.get(&1).map(String::as_str), Some("alpha"));
        assert_eq!(reg.get(&2).map(String::as_str), Some("beta"));
        unregister_topic(&home, 1);
        let reg = load_topic_registry(&home);
        assert_eq!(reg.get(&1), None);
        assert_eq!(reg.get(&2).map(String::as_str), Some("beta"));
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
    fn resolve_topic_reloads_from_topics_json() {
        let home = tmp_home("resolve_reload");
        register_topic(&home, 229, "alice").unwrap();
        register_topic(&home, 1, "general").unwrap();
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
        register_topic(&home, 500, "bob").unwrap();
        let mut state = TelegramState::new(
            "tok",
            -1,
            HashMap::new(),
            home.clone(),
            HashMap::new(),
            None,
        );
        assert_eq!(resolve_topic(&mut state, Some(500)), "bob");
        std::fs::remove_file(topic_registry_path(&home)).ok();
        assert_eq!(resolve_topic(&mut state, Some(500)), "bob");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_topic_falls_back_to_topics_json() {
        let home = tmp_home("resolve_topics_json");
        let mut reg = HashMap::new();
        reg.insert(2474, "test-gemini".to_string());
        save_topic_registry(&home, &reg).unwrap();
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
    fn register_topic_writes_only_topics_json() {
        let home = tmp_home("register_writes_json");
        let yaml = "defaults:\n  backend: claude\ninstances:\n  alice:\n    backend: claude\n";
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).ok();
        register_topic(&home, 42, "alice").unwrap();
        let content = std::fs::read_to_string(crate::fleet::fleet_yaml_path(&home))
            .expect("fleet.yaml must exist");
        assert!(
            !content.contains("topic_id"),
            "fleet.yaml must NOT contain topic_id after register_topic: {content}"
        );
        let reg = load_topic_registry(&home);
        assert_eq!(reg.get(&42), Some(&"alice".to_string()));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn topic_registry_roundtrip() {
        let home = tmp_home("registry_roundtrip");
        assert!(load_topic_registry(&home).is_empty());
        register_topic(&home, 100, "alice").unwrap();
        register_topic(&home, 200, "bob").unwrap();
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
        register_topic(&home, 100, "alice").unwrap();
        register_topic(&home, 100, "bob").unwrap();
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
        register_topic(&home, 42, "alice").unwrap();
        register_topic(&home, 99, "bob").unwrap();
        assert_eq!(lookup_topic_for_instance(&home, "alice"), Some(42));
        assert_eq!(lookup_topic_for_instance(&home, "bob"), Some(99));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn lookup_topic_for_instance_returns_none_when_missing() {
        let home = tmp_home("lookup_missing");
        register_topic(&home, 42, "alice").unwrap();
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
        register_topic(&home, 77, "reuse-agent").unwrap();
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

    // ── #991 bind_topic_for_instance ─────────────────────────────────

    #[test]
    fn bind_topic_for_instance_not_found_when_no_fleet_yaml() {
        let home = tmp_home("bind-no-fleet-yaml");
        let result = bind_topic_for_instance(&home, "ghost");
        assert_eq!(result, BindTopicOutcome::InstanceNotFound);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn bind_topic_for_instance_not_found_when_instance_missing() {
        let home = tmp_home("bind-instance-missing");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  other:\n    backend: claude\n",
        )
        .unwrap();
        let result = bind_topic_for_instance(&home, "ghost");
        assert_eq!(result, BindTopicOutcome::InstanceNotFound);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn bind_topic_for_instance_refuses_skip_mode_991() {
        let home = tmp_home("bind-skip-refused");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  internal-only:\n    backend: claude\n    topic_binding_mode: skip\n",
        )
        .unwrap();
        let result = bind_topic_for_instance(&home, "internal-only");
        match result {
            BindTopicOutcome::NotEligible { reason } => {
                assert!(
                    reason.contains("skip"),
                    "NotEligible reason must explain the skip-mode refusal: {reason}"
                );
            }
            other => panic!("expected NotEligible, got {other:?}"),
        }
        // Refusal must not touch topics.json.
        assert_eq!(lookup_topic_for_instance(&home, "internal-only"), None);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn bind_topic_for_instance_already_bound_is_idempotent_991() {
        let home = tmp_home("bind-already-bound");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  deferred-agent:\n    backend: claude\n    topic_binding_mode: deferred\n",
        )
        .unwrap();
        register_topic(&home, 501, "deferred-agent").unwrap();
        let result = bind_topic_for_instance(&home, "deferred-agent");
        assert_eq!(result, BindTopicOutcome::AlreadyBound(501));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn bind_topic_for_instance_channel_unavailable_without_config_991() {
        let home = tmp_home("bind-no-channel");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  deferred-agent:\n    backend: claude\n    topic_binding_mode: deferred\n",
        )
        .unwrap();
        // No `channel:` section → resolve_channel_only_from errors.
        let result = bind_topic_for_instance(&home, "deferred-agent");
        assert_eq!(result, BindTopicOutcome::ChannelUnavailable);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn bind_topic_for_instance_auto_mode_without_topic_is_eligible_991() {
        // #991 design note (BIND-TOPIC-PRERESEARCH.md §1): only `skip` is
        // refused. An `auto`-mode instance that ended up without a topic
        // (e.g. spawned during the ~6s post-boot window) is a legitimate
        // bind_topic target too — this pins that `auto` is NOT rejected.
        let home = tmp_home("bind-auto-eligible");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  auto-agent:\n    backend: claude\n",
        )
        .unwrap();
        let result = bind_topic_for_instance(&home, "auto-agent");
        assert_eq!(
            result,
            BindTopicOutcome::ChannelUnavailable,
            "auto-mode instance must reach the channel-resolution step, not be refused as NotEligible"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
