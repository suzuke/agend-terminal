//! Channel bootstrap — dispatches on [`ChannelConfig`] variant to initialize
//! the appropriate adapter (Telegram or Discord).

use crate::fleet::FleetConfig;
use crate::telegram::TelegramState;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Collect submit_keys from fleet config (shared by both adapters).
fn collect_submit_keys(config: &FleetConfig) -> HashMap<String, String> {
    config
        .instances
        .keys()
        .filter_map(|name| {
            config
                .resolve_instance(name)
                .map(|r| (name.clone(), r.submit_key))
        })
        .collect()
}

/// Initialize Telegram when the channel config is Telegram variant.
pub(super) fn init_telegram(
    config: &FleetConfig,
    home: &Path,
) -> Option<Arc<Mutex<TelegramState>>> {
    match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram { .. }) => {
            let submit_keys = collect_submit_keys(config);
            crate::telegram::init_from_config(config, home, submit_keys)
        }
        _ => None,
    }
}

/// Initialize Discord when the channel config is Discord variant.
#[cfg(feature = "discord")]
pub(super) fn init_discord(
    config: &FleetConfig,
    home: &Path,
) -> Option<Arc<Mutex<crate::discord::DiscordState>>> {
    match &config.channel {
        Some(crate::fleet::ChannelConfig::Discord { .. }) => {
            let submit_keys = collect_submit_keys(config);
            crate::discord::init_from_config(config, home, submit_keys)
        }
        _ => None,
    }
}
