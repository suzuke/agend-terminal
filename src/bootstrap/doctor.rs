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

/// D001: channel.user_allowlist not actionable — fail-closed gate drops
/// all outbound notifications (stall/crash/CI alerts) AND all inbound
/// commands (Sprint 21 Phase 2 cascade-closed boundary).
///
/// Two surface variants share the D001 code:
///   - `user_allowlist:` field omitted (`None`): legacy shape. Operator
///     never wrote the field — usually a config that pre-dates the
///     Phase 2 fail-closed swap.
///   - `user_allowlist: []` (`Some(empty)`): Sprint 56 Track B addition.
///     Quickstart writes this stanza by default with a TODO-style
///     comment, so operators who skip the manual edit ship `[]` to
///     prod and watch every message silently drop. The "explicit
///     opt-out" interpretation that previously suppressed D001 here
///     is retired — true lock-downs should comment out the channel
///     block instead, matching the commented template at
///     `quickstart.rs:269`.
fn check_channel_user_allowlist(config: &FleetConfig, diags: &mut Vec<Diagnostic>) {
    let check_single = |ch: &ChannelConfig, label: &str, diags: &mut Vec<Diagnostic>| {
        let ChannelConfig::Telegram { user_allowlist, .. } = ch else {
            return;
        };
        let detail = match user_allowlist {
            None => Some("no user_allowlist field"),
            Some(list) if list.is_empty() => Some("user_allowlist is empty (`[]`)"),
            Some(_) => None,
        };
        if let Some(detail) = detail {
            diags.push(Diagnostic {
                severity: Severity::Critical,
                code: "D001",
                message: format!(
                    "channel '{label}' has {detail}. All inbound commands \
                     and outbound notifications (stall / crash / CI alerts) \
                     will be silently dropped (fail-closed gate)."
                ),
                fix_stanza: Some(
                    "Add to fleet.yaml under the channel config:\n  \
                     channel:\n    type: telegram\n    user_allowlist:\n      \
                     - <YOUR_TELEGRAM_USER_ID>\n\
                     If you want to disable the channel entirely, comment out the \
                     `channel:` block instead of leaving an empty allowlist."
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

    /// Sprint 56 Track B-critical (#525 item 1): `user_allowlist: []` —
    /// the quickstart-emitted default — must trigger D001 so operators
    /// see the FATAL diagnostic at startup. Pre-Sprint-56 this case
    /// was silenced as "explicit opt-out"; that interpretation is
    /// retired because quickstart writes `[]` automatically and
    /// operators have no idea their bot is silently dropping every
    /// message.
    #[test]
    fn empty_allowlist_emits_critical_d001() {
        let config = FleetConfig {
            channel: Some(telegram_config(Some(vec![]))),
            ..Default::default()
        };
        let diags = validate_fleet_config(&config);
        assert_eq!(diags.len(), 1, "empty allowlist must trigger one D001");
        assert_eq!(diags[0].severity, Severity::Critical);
        assert_eq!(diags[0].code, "D001");
        assert!(
            diags[0].message.contains("empty"),
            "diagnostic must distinguish empty-list case from missing-field case: {}",
            diags[0].message
        );
        assert!(diags[0].fix_stanza.is_some());
    }

    /// The two D001 surface variants must produce visibly distinct
    /// `message` text so an operator can tell whether they need to
    /// add a missing field or replace an empty list.
    #[test]
    fn d001_message_differentiates_none_from_empty() {
        let cfg_none = FleetConfig {
            channel: Some(telegram_config(None)),
            ..Default::default()
        };
        let cfg_empty = FleetConfig {
            channel: Some(telegram_config(Some(vec![]))),
            ..Default::default()
        };
        let none_msg = &validate_fleet_config(&cfg_none)[0].message;
        let empty_msg = &validate_fleet_config(&cfg_empty)[0].message;
        assert_ne!(
            none_msg, empty_msg,
            "missing-field vs empty-list diagnostics must read differently"
        );
        assert!(
            none_msg.contains("no user_allowlist field"),
            "None-case wording must name the missing field: {none_msg}"
        );
        assert!(
            empty_msg.contains("empty"),
            "empty-case wording must call out the empty list: {empty_msg}"
        );
    }

    /// The fix stanza must guide an operator who deliberately wants to
    /// disable the channel toward the right action (comment out the
    /// block) rather than the now-flagged `user_allowlist: []` form.
    #[test]
    fn d001_fix_stanza_mentions_channel_block_for_disabling() {
        let config = FleetConfig {
            channel: Some(telegram_config(Some(vec![]))),
            ..Default::default()
        };
        let diags = validate_fleet_config(&config);
        let stanza = diags[0]
            .fix_stanza
            .as_deref()
            .expect("fix stanza required for D001");
        assert!(
            stanza.contains("comment out") && stanza.contains("channel:"),
            "stanza must direct lock-down operators to comment out channel:\n{stanza}"
        );
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
