//! Telegram credential resolution — loads bot token + group id from fleet.yaml.

/// Resolved Telegram channel credentials — avoids repeated fleet.yaml loads.
#[derive(Debug)]
pub(crate) struct TelegramCreds {
    pub(crate) token: String,
    pub(crate) group_id: i64,
}

pub(super) fn resolve_channel() -> anyhow::Result<(TelegramCreds, crate::fleet::FleetConfig)> {
    resolve_channel_from(&crate::home_dir())
}

// pub(crate): the #2005 regression tests (quickstart) exercise the REAL
// resolve entry against quickstart-generated fleet.yaml shapes.
pub(crate) fn resolve_channel_from(
    home: &std::path::Path,
) -> anyhow::Result<(TelegramCreds, crate::fleet::FleetConfig)> {
    let config = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))?;
    match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram {
            bot_token_env,
            group_id,
            ..
        }) => {
            // #2005: SYMMETRIC fallback. Pre-#2005 the only fallback was
            // legacy-ward (`AGEND_BOT_TOKEN`) — dead code whenever
            // `bot_token_env` already WAS the legacy name (exactly the pair
            // the old quickstart template generated: fleet pinned the legacy
            // name while `.env` was written under the canonical key, so a
            // fresh install failed to resolve at startup). Order: the
            // configured name first, then whichever of canonical/legacy it
            // isn't — covering both drift directions (old fleet template +
            // migrated `.env`, new template + legacy `.env`).
            const CANONICAL: &str = "AGEND_TELEGRAM_BOT_TOKEN";
            const LEGACY: &str = "AGEND_BOT_TOKEN";
            let token = std::env::var(bot_token_env)
                .or_else(|_| {
                    let fallback_name = if bot_token_env == CANONICAL {
                        LEGACY
                    } else {
                        CANONICAL
                    };
                    let fallback = std::env::var(fallback_name);
                    if fallback.is_ok() {
                        if fallback_name == LEGACY {
                            tracing::warn!(
                                "AGEND_BOT_TOKEN is deprecated — migrate to {bot_token_env}"
                            );
                        } else {
                            tracing::warn!(
                                "fleet.yaml bot_token_env '{bot_token_env}' is not set; using \
                                 {CANONICAL} — update bot_token_env to the canonical name"
                            );
                        }
                    }
                    fallback
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

/// #2207: resolve `bot_token_env` to its value using the SAME symmetric
/// canonical/legacy fallback as [`resolve_channel_from`], but without loading
/// config or emitting tracing warnings. Returns `None` when no token is set
/// under either name (telegram will not run).
///
/// Shared so the doctor's detached-start pre-flight gate (#2207 A1) decides
/// "telegram will actually run" with the exact same token resolution the
/// runtime uses — a divergence would false-pass or false-block the
/// empty-allowlist fail-fast.
pub(crate) fn resolve_token_value(bot_token_env: &str) -> Option<String> {
    const CANONICAL: &str = "AGEND_TELEGRAM_BOT_TOKEN";
    const LEGACY: &str = "AGEND_BOT_TOKEN";
    std::env::var(bot_token_env).ok().or_else(|| {
        let fallback_name = if bot_token_env == CANONICAL {
            LEGACY
        } else {
            CANONICAL
        };
        std::env::var(fallback_name).ok()
    })
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
pub(crate) fn resolve_channel_only_from(home: &std::path::Path) -> anyhow::Result<TelegramCreds> {
    resolve_channel_from(home).map(|(ch, _)| ch)
}
