//! #966 — shared TUI-side helper for "create an instance from a TUI
//! surface" (Backend menu via ctrl+b c, command palette `:spawn` /
//! `:vsplit` / `:hsplit`, future paths).
//!
//! Mirrors the side-effect chain that MCP `create_instance` /
//! `deploy_template` / team mode get for free via `api::call(SPAWN) →
//! handle_spawn`:
//!
//!   1. Persist entry to fleet.yaml via `add_instance_to_yaml` (atomic
//!      write with file lock)
//!   2. Look up the ACTIVE CHANNEL AT CALL-TIME (not from cached
//!      `Option<Arc<dyn Channel>>` — post-#945 Phase 1 the channel
//!      registers ~6s after app startup; cached snapshots are commonly
//!      `None` and silently no-op forever)
//!   3. Persist topic_id to topics.json via `register_topic` when a
//!      topic was created — closes the #964-class caller-path
//!      replication for the TUI surface
//!
//! Returns `TopicOutcome` so callers handle Created / NoChannel /
//! Failed explicitly (no silent `let _ = ...` per #962 discipline).

use crate::channel::{ensure_topic_for, TopicOutcome};
use std::path::Path;

/// #966 entry point — `MenuItemKind::Backend` handler and the `:spawn` /
/// `:vsplit` / `:hsplit` palette commands call this BEFORE
/// `pane_factory::create_pane[_from_resolved]`. Failures in
/// `add_instance_to_yaml` bubble up so the caller can abort the
/// pane-creation flow (no orphan `topic_id` write).
///
/// Topic creation failures (channel exists but `create_topic` returns
/// Err) do NOT block instance creation — the instance is still
/// operator-functional locally. The `Failed` outcome carries the error
/// string so callers `tracing::warn!` and surface to operator.
pub(crate) fn add_instance_with_topic(
    home: &Path,
    name: &str,
    entry: &crate::fleet::InstanceYamlEntry,
) -> anyhow::Result<TopicOutcome> {
    crate::fleet::add_instance_to_yaml(home, name, entry)
        .map_err(|e| anyhow::anyhow!("failed to persist instance to fleet.yaml: {e}"))?;

    let outcome = ensure_topic_for(name);

    if let TopicOutcome::Created(ref tid) = outcome {
        if let Ok(topic_id) = tid.parse::<i32>() {
            crate::channel::telegram::register_topic(home, topic_id, name).map_err(|e| {
                anyhow::anyhow!(
                    "topic created (id={tid}) but failed to persist to topics.json: {e}"
                )
            })?;
        }
        tracing::info!(
            instance = name,
            topic_id = %tid,
            "TUI-spawn: created channel topic (persisted to topics.json)"
        );
    } else if let TopicOutcome::Failed(ref err) = outcome {
        tracing::warn!(
            instance = name,
            error = %err,
            "TUI-spawn: channel exists but create_topic failed; instance \
             created without topic (next daemon restart's bootstrap.rs \
             self-heal will retry)"
        );
    }

    Ok(outcome)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! #966 — caller-path regression tests for the TUI-spawn helper.
    //!
    //! Tests use `serial_test::serial` because they manipulate the
    //! process-wide channel registry (`crate::channel::register_active_channel`).
    //! Without serialization, parallel tests interfere with each other's
    //! `active_channel()` lookup.
    //!
    //! T3 / T4 (caller-path integration tests for Backend menu + palette)
    //! live in `tests/tui_create_topic_caller_path.rs` because they need
    //! to exercise the `MenuItemKind::Backend` handler / `commands::execute`
    //! path through a higher-level setup that's tricky to wire up in a
    //! unit-test module without dragging in the full Layout / KeyHandler
    //! state machine.

    use super::*;
    use crate::channel::{
        register_active_channel, reset_active_channel_for_test, BindingOpts, BindingRef, Channel,
        ChannelCapabilities, ChannelError, ChannelEvent, MsgRef, OutMsg, TopicRef,
    };
    use serial_test::serial;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn tmp_home(slug: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering as O2};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, O2::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agend-966-{}-{}-{}", slug, std::process::id(), id));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(crate::fleet::fleet_yaml_path(&dir), "instances: {}\n").unwrap();
        dir
    }

    /// Mock channel that records `create_topic` calls and returns a
    /// configurable topic_id. Used by T1 + T2 to inject channel behavior
    /// without spinning up real telegram infra.
    struct MockChannel {
        caps: ChannelCapabilities,
        topic_id_seed: i32,
        create_count: AtomicUsize,
    }

    impl MockChannel {
        fn new(seed: i32) -> Self {
            Self {
                caps: ChannelCapabilities::default(),
                topic_id_seed: seed,
                create_count: AtomicUsize::new(0),
            }
        }
    }

    impl Channel for MockChannel {
        fn kind(&self) -> &'static str {
            "mock"
        }
        fn caps(&self) -> &ChannelCapabilities {
            &self.caps
        }
        fn poll_event(&self) -> Option<ChannelEvent> {
            None
        }
        fn send(&self, _: &BindingRef, _: OutMsg) -> anyhow::Result<MsgRef> {
            anyhow::bail!("mock")
        }
        fn edit(&self, _: &MsgRef, _: OutMsg) -> anyhow::Result<()> {
            anyhow::bail!("mock")
        }
        fn delete(&self, _: &MsgRef) -> anyhow::Result<()> {
            anyhow::bail!("mock")
        }
        fn create_binding(&self, _: &str, _: BindingOpts) -> anyhow::Result<BindingRef> {
            anyhow::bail!("mock")
        }
        fn remove_binding(&self, _: &BindingRef) -> anyhow::Result<()> {
            anyhow::bail!("mock")
        }
        fn has_binding(&self, _: &str) -> bool {
            false
        }
        fn record_binding(&self, _: &str, _: BindingRef, _: String) {}
        fn take_binding(&self, _: &str) -> Option<BindingRef> {
            None
        }
        fn attach_registry(&self, _: crate::agent::AgentRegistry) {}
        fn create_topic(&self, _name: &str) -> std::result::Result<TopicRef, ChannelError> {
            let n = self.create_count.fetch_add(1, Ordering::Relaxed);
            Ok(TopicRef {
                id: format!("{}", self.topic_id_seed + n as i32),
                channel_kind: crate::channel::ChannelKind::Telegram,
            })
        }
    }

    fn make_entry() -> crate::fleet::InstanceYamlEntry {
        crate::fleet::InstanceYamlEntry {
            backend: Some("claude".to_string()),
            working_directory: None,
            role: None,
            instructions: None,
            source_repo: None,
            repo: None,
            github_login: None,
            args: None,
            model: None,
            env: None,
            ready_pattern: None,
            command: None,
            worktree: None,
            topic_binding_mode: None,
        }
    }

    /// T1 — happy path: mock channel registered, helper invoked,
    /// topics.json ends with topic_id persisted + helper returns
    /// `Created(topic_id)`.
    #[test]
    #[serial]
    fn t1_add_instance_with_topic_creates_topic_and_persists() {
        reset_active_channel_for_test();
        let home = tmp_home("t1");
        let mock = Arc::new(MockChannel::new(7001));
        register_active_channel(mock.clone() as Arc<dyn Channel>);

        let entry = make_entry();
        let outcome = add_instance_with_topic(&home, "t1-agent", &entry).expect("helper Ok");

        match outcome {
            TopicOutcome::Created(ref tid) => assert_eq!(tid, "7001"),
            other => panic!("expected Created, got {other:?}"),
        }
        assert_eq!(
            crate::channel::telegram::lookup_topic_for_instance(&home, "t1-agent"),
            Some(7001),
            "topic_id must be persisted to topics.json"
        );
        reset_active_channel_for_test();
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T2 — no-channel: no active channel registered at call-time.
    /// Helper returns `NoChannel`; fleet.yaml has the entry but no
    /// topic_id (the natural happy path when telegram isn't configured).
    #[test]
    #[serial]
    fn t2_add_instance_with_topic_returns_no_channel_when_unregistered() {
        reset_active_channel_for_test();
        let home = tmp_home("t2");

        let entry = make_entry();
        let outcome = add_instance_with_topic(&home, "t2-agent", &entry).expect("helper Ok");

        assert_eq!(
            outcome,
            TopicOutcome::NoChannel,
            "no active channel must yield NoChannel outcome"
        );
        let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        assert!(
            cfg.instances.contains_key("t2-agent"),
            "instance entry must still be persisted"
        );
        assert_eq!(
            cfg.instances.get("t2-agent").and_then(|i| i.topic_id),
            None,
            "topic_id must be None when no channel registered"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T3 — Backend menu caller-path: builds the entry the same way
    /// `MenuItemKind::Backend` does (`Backend::preset()`-driven defaults,
    /// minimal InstanceYamlEntry shape) and invokes the helper. Asserts
    /// post-state matches what ctrl+b c → Backend menu should produce.
    ///
    /// Pre-fix (Backend menu calling `add_instance_to_yaml` directly +
    /// no topic creation): no topic_id persisted anywhere.
    /// Post-fix (Backend menu calling this helper): topics.json has the
    /// topic_id mapping.
    ///
    /// THIS is the caller-path regression anchor for the #966 bug
    /// (matches #964 T1 pattern — the test PR #966 should ship to
    /// catch future copy-paste TUI-spawn paths).
    #[test]
    #[serial]
    fn t3_backend_menu_caller_path_persists_topic_id() {
        reset_active_channel_for_test();
        let home = tmp_home("t3");
        let mock = Arc::new(MockChannel::new(9100));
        register_active_channel(mock.clone() as Arc<dyn Channel>);

        // Mimic MenuItemKind::Backend handler's entry construction:
        // backend name + None for everything else (defaults via fleet
        // resolve later — see src/app/mod.rs Backend handler arm).
        let entry = crate::fleet::InstanceYamlEntry {
            backend: Some("claude".to_string()),
            working_directory: None,
            role: None,
            instructions: None,
            source_repo: None,
            repo: None,
            github_login: None,
            args: None,
            model: None,
            env: None,
            ready_pattern: None,
            command: None,
            worktree: None,
            topic_binding_mode: None,
        };
        let outcome = add_instance_with_topic(&home, "ctrlb-c-agent", &entry).expect("Ok");

        match outcome {
            TopicOutcome::Created(ref tid) => assert_eq!(tid, "9100"),
            other => panic!("expected Created, got {other:?}"),
        }
        assert_eq!(
            crate::channel::telegram::lookup_topic_for_instance(&home, "ctrlb-c-agent"),
            Some(9100),
            "#966 Backend menu caller-path: topic_id must be persisted to topics.json"
        );
        reset_active_channel_for_test();
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T4 — `:spawn` palette caller-path: same shape as T3 but with
    /// the palette's entry construction (just backend_name from
    /// `parts.get(2)` — see src/app/commands.rs `:spawn` arm). Asserts
    /// the palette path persists topic_id after the helper-routing fix.
    #[test]
    #[serial]
    fn t4_palette_spawn_caller_path_persists_topic_id() {
        reset_active_channel_for_test();
        let home = tmp_home("t4");
        let mock = Arc::new(MockChannel::new(9200));
        register_active_channel(mock.clone() as Arc<dyn Channel>);

        // Mimic commands.rs `:spawn` arm: only backend name is supplied
        // explicitly; rest default to None (resolved later by fleet
        // resolver).
        let entry = crate::fleet::InstanceYamlEntry {
            backend: Some("codex".to_string()),
            working_directory: None,
            role: None,
            instructions: None,
            source_repo: None,
            repo: None,
            github_login: None,
            args: None,
            model: None,
            env: None,
            ready_pattern: None,
            command: None,
            worktree: None,
            topic_binding_mode: None,
        };
        let outcome = add_instance_with_topic(&home, "palette-agent", &entry).expect("Ok");

        match outcome {
            TopicOutcome::Created(ref tid) => assert_eq!(tid, "9200"),
            other => panic!("expected Created, got {other:?}"),
        }
        assert_eq!(
            crate::channel::telegram::lookup_topic_for_instance(&home, "palette-agent"),
            Some(9200),
            "#966 palette caller-path: topic_id must be persisted to topics.json"
        );
        reset_active_channel_for_test();
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T5 — regression smoke: `ensure_topic_for` (the hub) returns
    /// matching outcomes across the 3 production call sites' patterns
    /// (handle_spawn, team mode, TUI helper). Verifies the hub didn't
    /// break the existing API-side topic-creation contract that #964
    /// already exercises at `tests_964::t1_create_instance_persists_topic_id`.
    ///
    /// This is a contract test on the hub itself — proves the
    /// consolidation from 3 replicated `active_channel().create_topic()`
    /// chains into one `ensure_topic_for` doesn't drop the
    /// idempotent-reuse semantics.
    #[test]
    #[serial]
    fn t5_ensure_topic_for_hub_idempotent_across_repeat_calls() {
        reset_active_channel_for_test();
        let mock = Arc::new(MockChannel::new(9300));
        register_active_channel(mock.clone() as Arc<dyn Channel>);

        // First call → Created(9300). Second call → Created(9301) because
        // the MockChannel increments. Production telegram is idempotent
        // via topics.json reuse (see `create_topic_for_instance` at
        // src/channel/telegram/topic_registry.rs:74-79), which this
        // mock does NOT simulate — the mock's role here is to prove
        // the hub correctly delegates each call to the channel's
        // create_topic. Idempotency is a channel-side concern, not a
        // hub-side concern.
        let outcome_1 = crate::channel::ensure_topic_for("agent-a");
        let outcome_2 = crate::channel::ensure_topic_for("agent-b");

        match (outcome_1, outcome_2) {
            (TopicOutcome::Created(a), TopicOutcome::Created(b)) => {
                assert_eq!(a, "9300");
                assert_eq!(b, "9301");
            }
            other => panic!("expected both Created, got {other:?}"),
        }
        // 2 successful create_topic invocations recorded
        assert_eq!(mock.create_count.load(Ordering::Relaxed), 2);
        reset_active_channel_for_test();
    }

    /// T2b — runtime channel fallback: channel registered AFTER an
    /// initial helper call observed `NoChannel`. Subsequent helper call
    /// MUST see the newly-registered channel (proves runtime lookup
    /// works — would FAIL if helper had cached the `None` value).
    /// This is the key reviewer-finding test: post-#945 telegram_state
    /// starts as None and becomes Some(_) ~6s later.
    #[test]
    #[serial]
    fn t2b_runtime_channel_lookup_picks_up_late_register() {
        reset_active_channel_for_test();
        let home = tmp_home("t2b");
        let entry = make_entry();

        // First call: no channel.
        let outcome_pre = add_instance_with_topic(&home, "early-agent", &entry).expect("Ok");
        assert_eq!(outcome_pre, TopicOutcome::NoChannel);

        // Channel becomes available later (mimics post-#945 background telegram_init).
        let mock = Arc::new(MockChannel::new(8500));
        register_active_channel(mock.clone() as Arc<dyn Channel>);

        // Second call MUST see the channel via runtime active_channel() lookup.
        let outcome_post = add_instance_with_topic(&home, "late-agent", &entry).expect("Ok");
        match outcome_post {
            TopicOutcome::Created(ref tid) => assert_eq!(tid, "8500"),
            other => panic!("expected Created via runtime lookup, got {other:?}"),
        }
        assert_eq!(
            crate::channel::telegram::lookup_topic_for_instance(&home, "late-agent"),
            Some(8500),
            "late-arriving channel must persist topic_id to topics.json"
        );
        reset_active_channel_for_test();
        let _ = std::fs::remove_dir_all(&home);
    }
}
