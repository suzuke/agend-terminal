use super::*;
use crate::agent::AgentRegistry;
use crate::channel::{
    BindingRef, Channel, ChannelCapabilities, ChannelError, ChannelEvent, MarkdownDialect,
    MentionStyle, MsgRef, OutMsg, RateBudget,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::mpsc;

// ---------------------------------------------------------------------------
// Bootstrap (#2562 P1)
// ---------------------------------------------------------------------------

/// Intents this adapter requests — matches the shape already pinned by
/// `discord_gateway_identify_shape_matches_spec` (PR1, 2026-04-29): guild
/// membership + message content, the minimum needed to receive
/// `MESSAGE_CREATE` for a bound channel.
fn discord_intents() -> twilight_model::gateway::Intents {
    twilight_model::gateway::Intents::GUILDS
        | twilight_model::gateway::Intents::GUILD_MESSAGES
        | twilight_model::gateway::Intents::MESSAGE_CONTENT
}

/// Construct the Discord outbound HTTP client.
///
/// Must run through [`discord_runtime()`] (via the local [`block_on_value`]),
/// not on a bare thread. `twilight_http::Client::new` builds a
/// `twilight_http_ratelimiting::RateLimiter` whose constructor does
/// `tokio::spawn(actor::runner(..))` internally — called with no active
/// Tokio runtime in scope (as `bootstrap::discord_init::init` does, since it
/// runs `init_from_config` on a plain `std::thread`) that panics with
/// "there is no reactor running, must be called from the context of a
/// Tokio 1.x runtime" (twilight-http-ratelimiting 0.17.1 src/lib.rs:279 —
/// found via #2562 P3 isolated-smoke-home boot, 2026-07-03: the daemon's
/// real bootstrap path never got an end-to-end run before, only
/// `#[tokio::test]`-shielded unit tests, which supply an ambient runtime
/// this call site doesn't have on real boot). Routing through the same
/// `discord_runtime()` the outbound `send`/`edit`/`delete` calls already use
/// keeps the Client's rate-limiter actor task on one runtime for its whole
/// lifetime instead of init and outbound each managing their own.
pub(super) fn build_http_client(token: String) -> twilight_http::Client {
    block_on_value(async move { twilight_http::Client::new(token) })
}

/// Initialize Discord from fleet config: on `Some`, the gateway connection
/// is ALREADY running (via [`start_gateway`]) and the returned
/// [`DiscordChannel`] is ready to register. Returns `None` when Discord
/// isn't configured or the bot token env var isn't set — mirrors
/// `channel::telegram::init_from_config`'s not-configured contract so
/// callers can treat both channels symmetrically.
pub fn init_from_config(config: &crate::fleet::FleetConfig) -> Option<DiscordChannel> {
    // Production event source: open the real gateway. The config→channel
    // assembly lives in `init_from_config_with_source`; this default path is
    // byte-identical to the pre-#2562-P4 inline body.
    init_from_config_with_source(config, |token, intents, allowlist, tx| {
        spawn_gateway_supervisor(token, intents, allowlist, tx);
    })
}

/// #2562 P4 test-injection seam for [`init_from_config`]. `start_source`
/// receives exactly the `(token, intents, allowlist, tx)` production feeds to
/// [`spawn_gateway_supervisor`], but lets a test substitute a source that
/// pushes fixture events into `tx` instead of opening a live WebSocket — giving
/// the config→allowlist→channel→inbox assembly (the path the manual smoke used
/// to guard) CI coverage without real Discord creds/network. The production
/// caller ([`init_from_config`]) passes `spawn_gateway_supervisor` unchanged,
/// so its behavior is byte-identical.
pub(crate) fn init_from_config_with_source(
    config: &crate::fleet::FleetConfig,
    start_source: impl FnOnce(
        String,
        twilight_model::gateway::Intents,
        Option<Vec<i64>>,
        mpsc::Sender<ChannelEvent>,
    ),
) -> Option<DiscordChannel> {
    let (bot_token_env, guild_id, user_allowlist) = match config.channel.as_ref()? {
        crate::fleet::ChannelConfig::Telegram { .. } => return None,
        crate::fleet::ChannelConfig::Discord {
            bot_token_env,
            guild_id,
            user_allowlist,
        } => (bot_token_env, *guild_id, user_allowlist.clone()),
    };
    let token = match std::env::var(bot_token_env) {
        Ok(t) => t,
        Err(_) => {
            tracing::info!(env = %bot_token_env, "discord bot token env not set, skipping");
            return None;
        }
    };
    if user_allowlist.is_none() {
        tracing::warn!(
            "discord channel.user_allowlist is not set — fail-closed default: ALL inbound \
             messages are dropped. Set `user_allowlist: [123456789012345678]` in fleet.yaml \
             to enable the channel."
        );
    }

    // twilight_gateway::Config::new and twilight_http::Client::new each
    // add the `Bot ` prefix themselves if it's missing (checked, so this
    // stays correct even if an operator pastes an already-prefixed token)
    // — pass the raw token to both, per their documented contract.
    let (tx, rx) = mpsc::channel();
    start_source(token.clone(), discord_intents(), user_allowlist.clone(), tx);
    let http_client = std::sync::Arc::new(build_http_client(token));
    Some(DiscordChannel::new(
        rx,
        user_allowlist,
        http_client,
        guild_id,
    ))
}
