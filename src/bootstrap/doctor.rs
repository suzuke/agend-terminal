//! Doctor pre-flight startup check.
//!
//! Validates fleet.yaml for common operator pitfalls and emits
//! actionable diagnostics with copy-paste fix stanzas. Called during
//! [`super::prepare`] on the Owned path so operators see issues at
//! daemon startup rather than discovering them via silent failures.
//!
//! Sprint 23 P1 — deferred from Sprint 22 P0 PR #230.

use crate::fleet::{ChannelConfig, FleetConfig};

/// Severity levels for doctor diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Fail-closed gate missing — notifications silently dropped.
    Critical,
    /// Deprecated config — works now but will break in a future release.
    #[allow(dead_code)]
    Warning,
    /// Suggestion for better operator experience.
    #[allow(dead_code)]
    Info,
}

/// A single diagnostic finding from the doctor check.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub fix_stanza: Option<String>,
}

/// Validate fleet config for operator pitfalls. Returns diagnostics
/// ordered by severity (critical first).
pub fn validate_fleet_config(config: &FleetConfig) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    check_channel_user_allowlist(config, &mut diags);
    diags
}

/// Emit diagnostics to tracing at appropriate severity levels.
pub fn emit_diagnostics(diags: &[Diagnostic]) {
    for d in diags {
        let fix = d
            .fix_stanza
            .as_deref()
            .map(|s| format!("\n{s}"))
            .unwrap_or_default();
        match d.severity {
            Severity::Critical => {
                tracing::error!(code = d.code, "FATAL: {}{fix}", d.message);
            }
            Severity::Warning => {
                tracing::warn!(code = d.code, "{}{fix}", d.message);
            }
            Severity::Info => {
                tracing::info!(code = d.code, "{}{fix}", d.message);
            }
        }
    }
}

/// D001: channel.user_allowlist missing — fail-closed gate drops all
/// outbound notifications (stall/crash/CI alerts).
fn check_channel_user_allowlist(config: &FleetConfig, diags: &mut Vec<Diagnostic>) {
    let check_single = |ch: &ChannelConfig, label: &str, diags: &mut Vec<Diagnostic>| {
        if let ChannelConfig::Telegram {
            user_allowlist: None,
            ..
        } = ch
        {
            diags.push(Diagnostic {
                severity: Severity::Critical,
                code: "D001",
                message: format!(
                    "channel '{label}' has no user_allowlist configured. \
                     All outbound notifications (stall / crash / CI alerts) \
                     will be silently dropped (fail-closed gate)."
                ),
                fix_stanza: Some(
                    "Add to fleet.yaml under the channel config:\n  \
                     channel:\n    type: telegram\n    user_allowlist:\n      \
                     - <YOUR_TELEGRAM_USER_ID>"
                        .to_string(),
                ),
            });
        }
    };

    // Check singular `channel:` field.
    if let Some(ch) = &config.channel {
        check_single(ch, "telegram", diags);
    }

    // Check plural `channels:` map.
    if let Some(channels) = &config.channels {
        for (name, ch) in channels {
            check_single(ch, name, diags);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::FleetConfig;

    fn telegram_config(user_allowlist: Option<Vec<i64>>) -> ChannelConfig {
        ChannelConfig::Telegram {
            bot_token_env: "AGEND_TELEGRAM_BOT_TOKEN".into(),
            group_id: -100123,
            mode: "topic".into(),
            user_allowlist,
            fleet_binding: None,
        }
    }

    #[test]
    fn missing_user_allowlist_emits_critical() {
        let config = FleetConfig {
            channel: Some(telegram_config(None)),
            ..Default::default()
        };
        let diags = validate_fleet_config(&config);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Critical);
        assert_eq!(diags[0].code, "D001");
        assert!(diags[0].message.contains("user_allowlist"));
        assert!(diags[0].fix_stanza.is_some());
    }

    #[test]
    fn configured_user_allowlist_silent() {
        let config = FleetConfig {
            channel: Some(telegram_config(Some(vec![12345]))),
            ..Default::default()
        };
        let diags = validate_fleet_config(&config);
        assert!(diags.is_empty());
    }

    #[test]
    fn empty_allowlist_explicit_opt_out_silent() {
        // `user_allowlist: []` is an explicit opt-out — operator
        // deliberately chose to reject all. Not a misconfiguration.
        let config = FleetConfig {
            channel: Some(telegram_config(Some(vec![]))),
            ..Default::default()
        };
        let diags = validate_fleet_config(&config);
        assert!(diags.is_empty());
    }

    #[test]
    fn no_channel_configured_silent() {
        let config = FleetConfig::default();
        let diags = validate_fleet_config(&config);
        assert!(diags.is_empty());
    }

    #[test]
    fn plural_channels_checked() {
        let mut channels = std::collections::HashMap::new();
        channels.insert("tg-main".into(), telegram_config(None));
        channels.insert("tg-ops".into(), telegram_config(Some(vec![999])));
        let config = FleetConfig {
            channels: Some(channels),
            ..Default::default()
        };
        let diags = validate_fleet_config(&config);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("tg-main"));
    }

    #[test]
    fn discord_channel_not_flagged() {
        let config = FleetConfig {
            channel: Some(ChannelConfig::Discord {
                bot_token_env: "AGEND_DISCORD_BOT_TOKEN".into(),
                guild_id: 123,
            }),
            ..Default::default()
        };
        let diags = validate_fleet_config(&config);
        assert!(diags.is_empty());
    }
}
