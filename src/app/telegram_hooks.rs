//! Telegram topic lifecycle hooks — fire-and-forget wrappers tied to pane create/delete.
//!
//! State updates happen synchronously (so duplicate hooks are a no-op); the actual
//! Telegram API calls run on background threads to avoid blocking the TUI event loop.

use crate::agent::{self, AgentRegistry};
use crate::layout::Pane;
use crate::render;

use std::path::Path;
use std::sync::{Arc, Mutex};

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
    tg: &Option<Arc<Mutex<crate::telegram::TelegramState>>>,
    registry: &AgentRegistry,
    home: &Path,
    pane: &Pane,
) {
    let Some(tg) = tg else { return };
    let Some(fleet_name) = &pane.fleet_instance_name else {
        return;
    };
    {
        let s = crate::telegram::lock_state(tg);
        if s.instance_to_topic.contains_key(fleet_name) {
            return;
        }
    }
    let submit_key = {
        let reg = agent::lock_registry(registry);
        reg.get(&pane.agent_name)
            .map(|h| h.submit_key.clone())
            .unwrap_or_else(|| "\r".to_string())
    };
    let tg = Arc::clone(tg);
    let home = home.to_path_buf();
    let fleet_name = fleet_name.clone();
    std::thread::spawn(move || {
        match crate::telegram::create_topic_for_instance(&home, &fleet_name) {
            Some(tid) => {
                let mut s = crate::telegram::lock_state(&tg);
                s.instance_to_topic.insert(fleet_name.clone(), tid);
                s.topic_to_instance.insert(tid, fleet_name.clone());
                s.submit_keys.insert(fleet_name, submit_key);
            }
            None => tracing::warn!(%fleet_name, "failed to create Telegram topic"),
        }
    });
}

/// Delete Telegram topic for a fleet instance (non-blocking).
/// State is updated immediately; the Telegram API call runs on a background thread.
pub(super) fn maybe_delete_telegram_topic(
    tg: &Option<Arc<Mutex<crate::telegram::TelegramState>>>,
    home: &Path,
    fleet_name: &str,
) {
    let Some(tg) = tg else { return };
    let tid = {
        let mut s = crate::telegram::lock_state(tg);
        match s.instance_to_topic.remove(fleet_name) {
            Some(tid) => {
                s.topic_to_instance.remove(&tid);
                s.submit_keys.remove(fleet_name);
                tid
            }
            None => return,
        }
    };
    let home = home.to_path_buf();
    std::thread::spawn(move || {
        crate::telegram::delete_topic(&home, tid);
    });
}
