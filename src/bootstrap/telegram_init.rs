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
/// When the channel comes up, this also registers it as a
/// [`UxEventSink`] on the process-wide `ux_sink_registry` so Stage B-UX
/// `FleetEvent`s (emitted by MCP handlers) get rendered into the
/// configured `fleet_binding` topic. See `docs/DESIGN-stage-b-ux.md` §4.1.
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
    // state. Construct `Arc<TelegramChannel>` once and clone-upcast to
    // BOTH `Arc<dyn Channel>` (returned to the caller, used by dispatch)
    // AND `Arc<dyn UxEventSink>` (registered for Fleet-event fan-out).
    // `Arc<dyn Channel>` cannot itself be cast to `Arc<dyn UxEventSink>`
    // without nightly `trait_upcasting`; the concrete `Arc` is the bridge.
    let channel_concrete: Arc<TelegramChannel> = Arc::new(TelegramChannel::new(state));
    ux_sink_registry().register(channel_concrete.clone() as Arc<dyn UxEventSink>);
    Some(channel_concrete as Arc<dyn Channel>)
}
