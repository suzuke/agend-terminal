//! Discord bootstrap — wraps [`crate::channel::discord::init_from_config`],
//! mirroring `bootstrap::telegram_init`'s fire-and-forget shape (#2562 P1).
//!
//! Unlike Telegram's `init_from_config` (which does ~6s of sequential
//! `create_forum_topic` HTTP calls before returning), Discord's
//! `init_from_config` returns quickly — it only resolves the token and
//! starts the gateway connection in its own background thread ([`start_gateway`],
//! #2562 P0). Still run on a background thread here for the same reason
//! Telegram is: keep `resolve_fleet_and_reconcile` off any per-channel I/O,
//! and to match the established two-step pattern (`register_active_channel`
//! then `attach_pending_registry_when_ready`) exactly.
//!
//! [`start_gateway`]: crate::channel::discord::start_gateway

use crate::channel::Channel;
use crate::fleet::FleetConfig;
use std::sync::Arc;

/// Initialize the Discord channel when `channel:`/`channels:` configures
/// `type: discord` and the bot token env var is set. Always returns `None`
/// in production — actual init runs in a background thread and registers
/// the channel via [`crate::channel::register_active_channel`] on
/// completion. Callers should query `active_channel()` rather than relying
/// on this return (mirrors `bootstrap::telegram_init::init` exactly).
///
/// Discord has no `UxEventSink`/`fleet_binding` equivalent yet (Telegram-only
/// today — no current Discord consumer), so unlike telegram_init this does
/// NOT register with `ux_sink_registry()`.
pub(super) fn init(config: &FleetConfig) -> Option<Arc<dyn Channel>> {
    let config_clone = config.clone();
    // fire-and-forget: mirrors telegram_init's rationale — init resolves the
    // token and starts the gateway connection on ITS OWN thread (#2562 P0),
    // so this thread's own work is fast, but keeping it backgrounded avoids
    // adding a synchronous env-var/config-parse step to the critical cold-
    // boot path for a channel most fleets don't use. NO JoinHandle needed —
    // thread terminates once init completes; process exit cleans it up.
    let spawn_result = std::thread::Builder::new()
        .name("discord_init".into())
        .spawn(move || {
            let _census = crate::thread_census::register("discord_init");
            let Some(channel) = crate::channel::discord::init_from_config(&config_clone) else {
                tracing::info!("discord init: skipped (no token / no config)");
                return;
            };
            let channel_dyn: Arc<dyn Channel> = Arc::new(channel) as Arc<dyn Channel>;
            crate::channel::register_active_channel(Arc::clone(&channel_dyn));
            tracing::info!("discord init: channel registered");
            attach_pending_registry_when_ready(channel_dyn);
        });
    if let Err(e) = spawn_result {
        tracing::error!(error = %e, "discord_init: failed to spawn background thread");
    }
    // Always return None — callers must use `active_channel()` to discover
    // the registered channel post-init.
    None
}

/// Poll `crate::agent::get_pending_registry` until the caller has published
/// the registry, then call `attach_registry`. Bounded poll (30s, 100ms
/// cadence) — identical contract to
/// `bootstrap::telegram_init::attach_pending_registry_when_ready`.
fn attach_pending_registry_when_ready(channel: Arc<dyn Channel>) {
    attach_pending_registry_when_ready_with_deadline(
        channel,
        std::time::Instant::now() + std::time::Duration::from_secs(30),
    );
}

/// Test seam: deadline-injectable variant of
/// [`attach_pending_registry_when_ready`].
fn attach_pending_registry_when_ready_with_deadline(
    channel: Arc<dyn Channel>,
    deadline: std::time::Instant,
) {
    loop {
        if let Some(registry) = crate::agent::get_pending_registry() {
            channel.attach_registry(registry);
            tracing::info!("discord init: attach_registry complete");
            return;
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                "discord init: pending registry never published within deadline — inbound \
                 discord messages will not route to agents until daemon restart. This \
                 indicates a bug in caller setup ordering."
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Mock Channel that records whether `attach_registry` was called.
    /// Mirrors `bootstrap::telegram_init::tests::MockChannel`.
    struct MockChannel {
        attached: AtomicBool,
        caps: crate::channel::ChannelCapabilities,
    }

    impl MockChannel {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                attached: AtomicBool::new(false),
                caps: crate::channel::ChannelCapabilities::default(),
            })
        }
    }

    impl crate::channel::Channel for MockChannel {
        fn kind(&self) -> &'static str {
            "mock-2562-test"
        }
        fn caps(&self) -> &crate::channel::ChannelCapabilities {
            &self.caps
        }
        fn poll_event(&self) -> Option<crate::channel::ChannelEvent> {
            None
        }
        fn send(
            &self,
            _: &crate::channel::BindingRef,
            _: crate::channel::OutMsg,
        ) -> anyhow::Result<crate::channel::MsgRef> {
            anyhow::bail!("mock")
        }
        fn edit(
            &self,
            _: &crate::channel::MsgRef,
            _: crate::channel::OutMsg,
        ) -> anyhow::Result<()> {
            anyhow::bail!("mock")
        }
        fn delete(&self, _: &crate::channel::MsgRef) -> anyhow::Result<()> {
            anyhow::bail!("mock")
        }
        fn create_binding(
            &self,
            _: &str,
            _: crate::channel::BindingOpts,
        ) -> anyhow::Result<crate::channel::BindingRef> {
            anyhow::bail!("mock")
        }
        fn remove_binding(&self, _: &crate::channel::BindingRef) -> anyhow::Result<()> {
            anyhow::bail!("mock")
        }
        fn has_binding(&self, _: &str) -> bool {
            false
        }
        fn record_binding(&self, _: &str, _: crate::channel::BindingRef, _: String) {}
        fn take_binding(&self, _: &str) -> Option<crate::channel::BindingRef> {
            None
        }
        fn attach_registry(&self, _registry: AgentRegistry) {
            self.attached.store(true, Ordering::Relaxed);
        }
    }

    /// attach_pending_registry_when_ready calls `attach_registry` once the
    /// caller publishes a registry — deterministic, no sleep-based timing.
    /// Mirrors `bootstrap::telegram_init::tests::attach_pending_registry_when_ready_attaches_after_publish_945`.
    #[test]
    fn attach_pending_registry_when_ready_attaches_after_publish_2562() {
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        crate::agent::set_pending_registry(Arc::clone(&registry));

        let mock = MockChannel::new();
        let channel_dyn: Arc<dyn crate::channel::Channel> =
            mock.clone() as Arc<dyn crate::channel::Channel>;
        attach_pending_registry_when_ready(channel_dyn);

        assert!(
            mock.attached.load(Ordering::Relaxed),
            "attach_registry must have been called on the mock channel once the pending \
             registry was published"
        );
    }
}
