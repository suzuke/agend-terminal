//! Telegram topic lifecycle hooks — fire-and-forget wrappers tied to pane create/delete.
//!
//! State updates happen synchronously (so duplicate hooks are a no-op); the actual
//! Telegram API calls run on background threads to avoid blocking the TUI event loop.

use crate::agent::{self, AgentRegistry};
use crate::channel::{BindingOpts, Channel};
use crate::layout::Pane;
use crate::render;

use std::path::Path;
use std::sync::Arc;

/// Derive Telegram status from an already-loaded FleetConfig (no disk I/O).
pub(super) fn telegram_status_from_config(
    config: &crate::fleet::FleetConfig,
) -> render::TelegramStatus {
    match config.channel {
        Some(crate::fleet::ChannelConfig::Telegram {
            ref bot_token_env, ..
        }) => {
            if std::env::var(bot_token_env).is_ok() {
                render::TelegramStatus::Connected
            } else {
                render::TelegramStatus::NoToken
            }
        }
        None => render::TelegramStatus::NotConfigured,
    }
}

/// Create a Telegram topic for a newly spawned fleet instance (non-blocking).
/// Spawns a background thread for the Telegram API call to avoid freezing the TUI.
pub(super) fn maybe_create_telegram_topic(
    tg: &Option<Arc<dyn Channel>>,
    registry: &AgentRegistry,
    _home: &Path,
    pane: &Pane,
) {
    let Some(tg) = tg else { return };
    let Some(fleet_name) = &pane.fleet_instance_name else {
        return;
    };
    if tg.has_binding(fleet_name) {
        return;
    }
    let submit_key = {
        let reg = agent::lock_registry(registry);
        reg.get(&pane.agent_name)
            .map(|h| h.submit_key.clone())
            .unwrap_or_else(|| "\r".to_string())
    };
    let tg = Arc::clone(tg);
    let fleet_name = fleet_name.clone();
    std::thread::spawn(
        move || match tg.create_binding(&fleet_name, BindingOpts::default()) {
            Ok(binding) => tg.record_binding(&fleet_name, binding, submit_key),
            Err(e) => tracing::warn!(%fleet_name, error = %e, "failed to create channel binding"),
        },
    );
}

/// Delete Telegram topic for a fleet instance (non-blocking).
/// State is updated immediately; the Telegram API call runs on a background thread.
pub(super) fn maybe_delete_telegram_topic(
    tg: &Option<Arc<dyn Channel>>,
    _home: &Path,
    fleet_name: &str,
) {
    let Some(tg) = tg else { return };
    let Some(binding) = tg.take_binding(fleet_name) else {
        return;
    };
    let tg = Arc::clone(tg);
    std::thread::spawn(move || {
        if let Err(e) = tg.remove_binding(&binding) {
            tracing::warn!(error = %e, "remove_binding failed");
        }
    });
}
