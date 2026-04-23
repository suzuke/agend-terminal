//! Discord bootstrap — wraps [`crate::channel::discord::init_from_config`].

use crate::channel::discord::DiscordChannel;
use crate::channel::Channel;
use crate::fleet::FleetConfig;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

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
    let state = crate::channel::discord::init_from_config(config, home, submit_keys)?;
    Some(Arc::new(DiscordChannel::new(state)) as Arc<dyn Channel>)
}
