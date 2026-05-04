//! Telegram credential resolution — loads bot token + group id from fleet.yaml.

/// Resolved Telegram channel credentials — avoids repeated fleet.yaml loads.
pub(super) struct TelegramCreds {
    pub(super) token: String,
    pub(super) group_id: i64,
}

pub(super) fn resolve_channel() -> anyhow::Result<(TelegramCreds, crate::fleet::FleetConfig)> {
    resolve_channel_from(&crate::home_dir())
}

pub(super) fn resolve_channel_from(
    home: &std::path::Path,
) -> anyhow::Result<(TelegramCreds, crate::fleet::FleetConfig)> {
    let config = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))?;
    match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram {
            bot_token_env,
            group_id,
            ..
        }) => {
            let token = std::env::var(bot_token_env)
                .or_else(|_| {
                    let legacy = std::env::var("AGEND_BOT_TOKEN");
                    if legacy.is_ok() {
                        tracing::warn!(
                            "AGEND_BOT_TOKEN is deprecated — migrate to {bot_token_env}"
                        );
                    }
                    legacy
                })
                .map_err(|_| anyhow::anyhow!("bot token env '{bot_token_env}' not set"))?;
            Ok((
                TelegramCreds {
                    token,
                    group_id: *group_id,
                },
                config,
            ))
        }
        Some(crate::fleet::ChannelConfig::Discord { .. }) => {
            anyhow::bail!("Discord channel configured but telegram resolver called")
        }
        None => anyhow::bail!("No Telegram channel configured"),
    }
}

pub(super) fn resolve_channel_only() -> anyhow::Result<TelegramCreds> {
    resolve_channel().map(|(ch, _)| ch)
}

/// Like [`resolve_channel_only`] but reads `fleet.yaml` from a caller-
/// supplied home instead of the process-wide `AGEND_HOME`. Telegram
/// helpers that already receive a `home` argument (e.g.
/// `create_topic_for_instance`, `delete_topic`) must use this so a
/// `cargo test` pointing at a throwaway temp home doesn't silently
/// bleed into the operator's real bot channel — the `positive_pin-1`
/// topics the user observed were exactly this: the positive-pin dispatch
/// test creating a team via the API, reaching the topic helper, and the
/// unscoped resolver loading the real fleet.yaml instead of the test's.
pub(super) fn resolve_channel_only_from(home: &std::path::Path) -> anyhow::Result<TelegramCreds> {
    resolve_channel_from(home).map(|(ch, _)| ch)
}
