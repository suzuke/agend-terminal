//! Telegram bootstrap — wraps [`crate::telegram::init_from_config`] with the
//! submit-keys collection step that `cli::start_with_fleet` and `app::run`
//! each duplicated.

use crate::fleet::FleetConfig;
use crate::telegram::TelegramState;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Initialize Telegram polling when `channel:` is configured and the bot
/// token env var is set. Returns `None` otherwise (including when the channel
/// block is missing — not an error).
pub(super) fn init(config: &FleetConfig, home: &Path) -> Option<Arc<Mutex<TelegramState>>> {
    let submit_keys: HashMap<String, String> = config
        .instances
        .keys()
        .filter_map(|name| {
            config
                .resolve_instance(name)
                .map(|r| (name.clone(), r.submit_key))
        })
        .collect();
    crate::telegram::init_from_config(config, home, submit_keys)
}
