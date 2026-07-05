//! Telegram topic lifecycle hooks — fire-and-forget wrappers tied to pane create/delete.
//!
//! State updates happen synchronously (so duplicate hooks are a no-op); the actual
//! Telegram API calls run on background threads to avoid blocking the TUI event loop.

use crate::agent::{self, AgentRegistry};
use crate::channel::{BindingOpts, TelegramStatus};
use crate::layout::Pane;

use std::path::Path;

/// Derive Telegram status from an already-loaded FleetConfig (no disk I/O).
pub(super) fn telegram_status_from_config(config: &crate::fleet::FleetConfig) -> TelegramStatus {
    // #2642: resolve Telegram from the unified multi-channel view so the TUI
    // status is correct in a telegram+discord fleet (not only when Telegram is
    // the first-sorted / singular channel). Single-channel telegram unchanged.
    match config.telegram_channel() {
        Some(crate::fleet::ChannelConfig::Telegram { bot_token_env, .. }) => {
            if std::env::var(bot_token_env).is_ok() {
                TelegramStatus::Connected
            } else {
                TelegramStatus::NoToken
            }
        }
        _ => TelegramStatus::NotConfigured,
    }
}

/// Create a Telegram topic for a newly spawned fleet instance (non-blocking).
/// Spawns a background thread for the Telegram API call to avoid freezing the TUI.
///
/// Looks up the "telegram" channel live at call time rather than taking a
/// pre-resolved reference: `telegram_init` has been backgrounded since #945
/// (its synchronous return is always `None`), so a reference captured once at
/// `setup_app_bootstrap` time stays `None` for the process lifetime and this
/// hook silently never fires. Mirrors `maybe_create_discord_binding`'s
/// `lookup_channel_by_name` pattern (#2588), which never had this bug because
/// Discord's init was async from the start.
pub(super) fn maybe_create_telegram_topic(registry: &AgentRegistry, _home: &Path, pane: &Pane) {
    let Some(tg) = crate::channel::lookup_channel_by_name("telegram") else {
        return;
    };
    let Some(fleet_name) = &pane.fleet_instance_name else {
        return;
    };
    if tg.has_binding(fleet_name) {
        return;
    }
    let submit_key = {
        let reg = agent::lock_registry(registry);
        reg.get(&pane.instance_id)
            .map(|h| h.submit_key.clone())
            .unwrap_or_else(|| "\r".to_string())
    };
    let fleet_name = fleet_name.clone();
    // fire-and-forget: create_binding posts to Telegram + records into
    // TelegramState (Arc-shared). Short-lived; tied to UI pane creation.
    // No JoinHandle / shutdown signal needed — failed binding is logged.
    std::thread::spawn(
        move || match tg.create_binding(&fleet_name, BindingOpts::default()) {
            Ok(binding) => tg.record_binding(&fleet_name, binding, submit_key),
            Err(e) => tracing::warn!(%fleet_name, error = %e, "failed to create channel binding"),
        },
    );
}

/// Delete Telegram topic for a fleet instance (non-blocking).
/// State is updated immediately; the Telegram API call runs on a background thread.
///
/// Same live-lookup fix as [`maybe_create_telegram_topic`] — a frozen
/// pre-#945 reference here meant killing a pane via the TUI never cleaned up
/// its Telegram topic.
pub(super) fn maybe_delete_telegram_topic(_home: &Path, fleet_name: &str) {
    let Some(tg) = crate::channel::lookup_channel_by_name("telegram") else {
        return;
    };
    let Some(binding) = tg.take_binding(fleet_name) else {
        return;
    };
    // fire-and-forget: remove_binding posts to Telegram delete-topic API.
    // Short-lived; tied to UI pane delete. State already updated synchronously
    // (take_binding returned the binding). No JoinHandle needed — Telegram
    // API failure is logged warn but doesn't block the UI pane teardown.
    std::thread::spawn(move || {
        if let Err(e) = tg.remove_binding(&binding) {
            tracing::warn!(error = %e, "remove_binding failed");
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::channel::{
        active_channel, register_active_channel, reset_active_channel_for_test, BindingRef,
        Channel, ChannelCapabilities, ChannelEvent, MsgRef, OutMsg,
    };
    use crate::layout::{Pane, PaneSource};
    use crate::vterm::VTerm;
    use serial_test::serial;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    fn test_pane(fleet_name: Option<&str>) -> Pane {
        Pane {
            agent_name: "agent".into(),
            instance_id: crate::types::InstanceId::default(),
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id: 1,
            backend: None,
            working_dir: None,
            display_name: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: fleet_name.map(String::from),
            last_input_at: None,
            pending_notification_count: 0,
            pending_decision_count: 0,
            selection: None,
            source: PaneSource::Local,
            offthread: None,
            _fwd_cancel: None,
        }
    }

    /// Records `has_binding` / `take_binding` calls synchronously (these run
    /// on the CALLER's thread, before either hook spawns its background
    /// thread) so tests can assert the live-lookup path was reached without
    /// synchronizing with the fire-and-forget thread.
    struct MockChannel {
        caps: ChannelCapabilities,
        has_binding_calls: AtomicUsize,
        has_binding_return: bool,
        take_binding_calls: AtomicUsize,
        take_binding_return: bool,
    }

    impl MockChannel {
        fn new(has_binding_return: bool, take_binding_return: bool) -> Self {
            Self {
                caps: ChannelCapabilities::default(),
                has_binding_calls: AtomicUsize::new(0),
                has_binding_return,
                take_binding_calls: AtomicUsize::new(0),
                take_binding_return,
            }
        }
    }

    impl Channel for MockChannel {
        fn kind(&self) -> &'static str {
            "telegram"
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
            self.has_binding_calls.fetch_add(1, Ordering::SeqCst);
            self.has_binding_return
        }
        fn record_binding(&self, _: &str, _: BindingRef, _: String) {}
        fn take_binding(&self, _: &str) -> Option<BindingRef> {
            self.take_binding_calls.fetch_add(1, Ordering::SeqCst);
            self.take_binding_return
                .then(|| BindingRef::new("telegram", Some("mock".to_string()), 0i32))
        }
        fn attach_registry(&self, _: crate::agent::AgentRegistry) {}
    }

    fn empty_registry() -> crate::agent::AgentRegistry {
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()))
    }

    /// #2550-adjacent (telegram topic lifecycle): pre-fix, `maybe_create_telegram_topic`
    /// took a pre-resolved `&Option<Arc<dyn Channel>>` that stayed frozen at `None` for
    /// the process lifetime post-#945 backgrounding — this hook never reached
    /// `has_binding` at all, TUI-created panes silently got no topic tracking.
    /// Post-fix: a channel registered under kind "telegram" is found via a live
    /// `lookup_channel_by_name` at call time.
    #[test]
    #[serial]
    fn maybe_create_telegram_topic_finds_channel_via_live_lookup() {
        reset_active_channel_for_test();
        let mock = Arc::new(MockChannel::new(/* has_binding */ true, false));
        register_active_channel(mock.clone() as Arc<dyn Channel>);

        let registry = empty_registry();
        let pane = test_pane(Some("agent-a"));
        maybe_create_telegram_topic(&registry, Path::new("/tmp"), &pane);

        assert_eq!(
            mock.has_binding_calls.load(Ordering::SeqCst),
            1,
            "has_binding must be reached via a live lookup of the registered \
             \"telegram\" channel — pre-fix this was unreachable (frozen None)"
        );
        reset_active_channel_for_test();
    }

    #[test]
    #[serial]
    fn maybe_create_telegram_topic_is_noop_without_registered_channel() {
        reset_active_channel_for_test();
        let registry = empty_registry();
        let pane = test_pane(Some("agent-b"));
        // Must not panic when no "telegram" channel is registered.
        maybe_create_telegram_topic(&registry, Path::new("/tmp"), &pane);
        assert!(active_channel().is_none());
    }

    #[test]
    #[serial]
    fn maybe_create_telegram_topic_is_noop_without_fleet_instance_name() {
        reset_active_channel_for_test();
        let mock = Arc::new(MockChannel::new(true, false));
        register_active_channel(mock.clone() as Arc<dyn Channel>);

        let registry = empty_registry();
        let pane = test_pane(None); // no fleet_instance_name
        maybe_create_telegram_topic(&registry, Path::new("/tmp"), &pane);

        assert_eq!(
            mock.has_binding_calls.load(Ordering::SeqCst),
            0,
            "no fleet_instance_name must short-circuit before checking has_binding"
        );
        reset_active_channel_for_test();
    }

    /// Same regression class as the create-side test above, for the "kill"
    /// command's teardown hook.
    #[test]
    #[serial]
    fn maybe_delete_telegram_topic_finds_channel_via_live_lookup() {
        reset_active_channel_for_test();
        let mock = Arc::new(MockChannel::new(false, /* take_binding */ false));
        register_active_channel(mock.clone() as Arc<dyn Channel>);

        maybe_delete_telegram_topic(Path::new("/tmp"), "agent-c");

        assert_eq!(
            mock.take_binding_calls.load(Ordering::SeqCst),
            1,
            "take_binding must be reached via a live lookup of the registered \
             \"telegram\" channel — pre-fix this was unreachable (frozen None)"
        );
        reset_active_channel_for_test();
    }

    #[test]
    #[serial]
    fn maybe_delete_telegram_topic_is_noop_without_registered_channel() {
        reset_active_channel_for_test();
        // Must not panic when no "telegram" channel is registered.
        maybe_delete_telegram_topic(Path::new("/tmp"), "agent-d");
        assert!(active_channel().is_none());
    }

    /// End-to-end happy path: a genuine binding exists, so the hook spawns its
    /// background thread and `remove_binding` actually fires. Bounded poll
    /// (not an indefinite sleep) waits for the fire-and-forget thread.
    #[test]
    #[serial]
    fn maybe_delete_telegram_topic_removes_binding_on_background_thread() {
        struct RemovingMock {
            caps: ChannelCapabilities,
            removed: Arc<AtomicBool>,
        }
        impl Channel for RemovingMock {
            fn kind(&self) -> &'static str {
                "telegram"
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
                self.removed.store(true, Ordering::SeqCst);
                Ok(())
            }
            fn has_binding(&self, _: &str) -> bool {
                false
            }
            fn record_binding(&self, _: &str, _: BindingRef, _: String) {}
            fn take_binding(&self, _: &str) -> Option<BindingRef> {
                Some(BindingRef::new("telegram", Some("mock".to_string()), 0i32))
            }
            fn attach_registry(&self, _: crate::agent::AgentRegistry) {}
        }

        reset_active_channel_for_test();
        let removed = Arc::new(AtomicBool::new(false));
        let mock = Arc::new(RemovingMock {
            caps: ChannelCapabilities::default(),
            removed: removed.clone(),
        });
        register_active_channel(mock as Arc<dyn Channel>);

        maybe_delete_telegram_topic(Path::new("/tmp"), "agent-e");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !removed.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            removed.load(Ordering::SeqCst),
            "remove_binding must fire on the background thread once a live \
             \"telegram\" channel is found and take_binding returns a binding"
        );
        reset_active_channel_for_test();
    }
}
