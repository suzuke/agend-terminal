//! Telegram bootstrap — wraps [`crate::channel::telegram::init_from_config`] with the
//! submit-keys collection step that `cli::start_with_fleet` and `app::run`
//! each duplicated.

use crate::channel::telegram::TelegramChannel;
use crate::channel::Channel;
use crate::fleet::FleetConfig;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Initialize Telegram polling when `channel:` is configured and the bot
/// token env var is set. Returns `None` otherwise (including when the channel
/// block is missing — not an error).
pub(super) fn init(config: &FleetConfig, home: &Path) -> Option<Arc<dyn Channel>> {
    let submit_keys: HashMap<String, String> = config
        .instances
        .keys()
        .filter_map(|name| {
            config
                .resolve_instance(name)
                .map(|r| (name.clone(), r.submit_key))
        })
        .collect();
    let state = crate::channel::telegram::init_from_config(config, home, submit_keys)?;
    // `init_from_config` already calls `start_polling` on the concrete
    // state. Wrap in the trait object so downstream code holds only
    // `Arc<dyn Channel>`.
    Some(Arc::new(TelegramChannel::new(state)) as Arc<dyn Channel>)
}
