//! Telegram bootstrap — wraps [`crate::channel::telegram::init_from_config`] with the
//! submit-keys collection step that `cli::start_with_fleet` and `app::run`
//! each duplicated.

use crate::channel::sink_registry::registry as ux_sink_registry;
use crate::channel::telegram::TelegramChannel;
use crate::channel::ux_event::UxEventSink;
use crate::channel::Channel;
use crate::fleet::FleetConfig;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Initialize Telegram polling when `channel:` is configured and the bot
/// token env var is set. Returns `None` otherwise (including when the channel
/// block is missing — not an error).
///
/// When the channel comes up, this also:
/// - Registers it as a [`UxEventSink`] on the process-wide `ux_sink_registry`
///   so Stage B-UX `FleetEvent`s get rendered into the configured
///   `fleet_binding` topic.
/// - Registers it as the process-wide active channel via
///   [`crate::channel::register_active_channel`] so call sites outside the
///   adapter boundary can use trait methods (e.g. `create_topic`, `notify`)
///   without importing Telegram-specific code.
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
    let channel_concrete: Arc<TelegramChannel> = Arc::new(TelegramChannel::new(state));
    ux_sink_registry().register(channel_concrete.clone() as Arc<dyn UxEventSink>);
    let channel_dyn: Arc<dyn Channel> = channel_concrete as Arc<dyn Channel>;
    crate::channel::register_active_channel(Arc::clone(&channel_dyn));
    Some(channel_dyn)
}
