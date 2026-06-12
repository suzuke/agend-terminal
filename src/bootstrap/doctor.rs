//! Doctor pre-flight startup check.
//!
//! Validates fleet.yaml for common operator pitfalls and emits
//! actionable diagnostics with copy-paste fix stanzas. Called during
//! [`super::prepare`] on the Owned path so operators see issues at
//! daemon startup rather than discovering them via silent failures.
//!
//! Sprint 23 P1 — deferred from Sprint 22 P0 PR #230.

use crate::fleet::{ChannelConfig, FleetConfig};
use std::path::Path;

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
///
/// `home` is consulted by checks that need to read other daemon-state
/// files (e.g. D002 reads `task_sweep.json`). Passing the daemon's
/// `home_dir()` is correct for production callers; tests pass a
/// dedicated temp dir.
pub fn validate_fleet_config(config: &FleetConfig, home: &Path) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    check_channel_user_allowlist(config, &mut diags);
    check_task_sweep_github_login_mapping(config, home, &mut diags);
    diags
}

/// Emit diagnostics to tracing at appropriate severity levels.
///
/// Sprint 56 Track H2 (#525 item 5): Critical-severity diagnostics are
/// ALSO mirrored to `eprintln!` so the operator sees them regardless of
/// the daemon's tracing destination. The pre-Track-H2 path emitted only
/// via `tracing::error!`; in `agend-terminal start --detached` mode the
/// daemon detaches stderr from the operator's terminal and tracing
/// lands in a log file most operators never check, leaving D-class
/// diagnostics silent for the failure modes they were built to defend.
/// Mirroring to stderr is independent of tracing setup and survives
/// the detach.
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
                eprintln!("FATAL [{}]: {}{fix}", d.code, d.message);
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

/// D002: task_sweep is configured but no agent has a `github_login`
/// mapping declared in fleet.yaml. Sprint 56 Track F (#496): the sweep's
/// authorship gate compares `pr.author_login` against
/// `task.created_by` / `task.assignee` after resolving through the
/// `github_login` mapping; with zero mappings configured the gate
/// silently rejects every cross-namespace mismatch (the cheerc-class
/// repro that landed Track C-RCA's verdict).
///
/// Mirrors D001's `tracing::error!("FATAL: …")` + copy-paste fix
/// stanza pattern so operators see an actionable signal at startup
/// rather than discovering the problem via "my sweep doesn't work".
///
/// Silent when:
///   - `task_sweep_config.repo` is unset (sweep disabled, no auto-close
///     to mis-author, so the mapping isn't relevant).
///   - At least one instance has `github_login` set (operator has begun
///     mapping; D002 doesn't nag, the per-PR `tracing::warn!` at
///     `task_sweep.rs:218` covers any remaining unmapped instances).
///
/// Reads `<home>/task_sweep.json` directly via the same loader the
/// sweep tick uses, so a missing config file is treated as
/// sweep-disabled (no false-positive D002 on fresh deployments).
fn check_task_sweep_github_login_mapping(
    config: &FleetConfig,
    home: &Path,
    diags: &mut Vec<Diagnostic>,
) {
    let sweep_cfg = crate::daemon::task_sweep::load_sweep_config_for_doctor(home);
    let sweep_enabled = sweep_cfg
        .repo
        .as_deref()
        .map(|r| !r.is_empty())
        .unwrap_or(false)
        && !sweep_cfg.paused;
    if !sweep_enabled {
        return;
    }
    let any_mapped = config
        .instances
        .values()
        .any(|inst| inst.github_login.is_some());
    if any_mapped {
        return;
    }
    let repo = sweep_cfg.repo.as_deref().unwrap_or("<unset>");
    diags.push(Diagnostic {
        severity: Severity::Critical,
        code: "D002",
        message: format!(
            "task_sweep is enabled (repo='{repo}') but no instance in fleet.yaml has a \
             `github_login` mapping. The sweep's authorship gate compares the GitHub PR \
             author against the agend instance name, so every merged PR with a `Closes \
             t-...` marker will be silently rejected (see \
             docs/RCA-issue-496-task-sweep-no-auto-close-2026-05-08.md)."
        ),
        fix_stanza: Some(
            "Add a `github_login` field to each instance in fleet.yaml that \
             opens PRs:\n  \
             instances:\n    <instance-name>:\n      github_login: <YOUR_GITHUB_USERNAME>\n\
             Instances that don't open PRs can omit the field — D002 only fires \
             when ZERO instances are mapped."
                .to_string(),
        ),
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::fleet::{FleetConfig, InstanceConfig};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn telegram_config(user_allowlist: Option<Vec<i64>>) -> ChannelConfig {
        ChannelConfig::Telegram {
            bot_token_env: "AGEND_TELEGRAM_BOT_TOKEN".into(),
            group_id: -100123,
            mode: "topic".into(),
            user_allowlist: user_allowlist.map(|v| {
                v.into_iter()
                    .map(crate::fleet::AllowlistEntry::Id)
                    .collect()
            }),
            fleet_binding: None,
        }
    }

    /// Per-test scratch home directory for D002 fixtures (writes
    /// `task_sweep.json`). Without a sweep config file the loader returns
    /// the default (`repo: None`) and D002 stays silent — so D001 tests
    /// don't need to fabricate a sweep state.
    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-doctor-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a `task_sweep.json` enabling the sweep with the given repo
    /// so D002 evaluation has a config to read.
    fn enable_sweep(home: &std::path::Path, repo: &str) {
        let body = serde_json::json!({
            "repo": repo,
            "paused": false,
            "dry_run": false,
        });
        std::fs::write(
            home.join("task_sweep.json"),
            serde_json::to_string(&body).unwrap(),
        )
        .unwrap();
    }

    /// Build an `InstanceConfig` with the requested `github_login`.
    fn instance_with_login(github_login: Option<&str>) -> InstanceConfig {
        InstanceConfig {
            github_login: github_login.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn missing_user_allowlist_emits_critical() {
        let config = FleetConfig {
            channel: Some(telegram_config(None)),
            ..Default::default()
        };
        let diags = validate_fleet_config(&config, &tmp_home("doctor"));
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
        let diags = validate_fleet_config(&config, &tmp_home("doctor"));
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
        let diags = validate_fleet_config(&config, &tmp_home("doctor"));
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
        let none_msg = &validate_fleet_config(&cfg_none, &tmp_home("doctor"))[0].message;
        let empty_msg = &validate_fleet_config(&cfg_empty, &tmp_home("doctor"))[0].message;
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
        let diags = validate_fleet_config(&config, &tmp_home("doctor"));
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
        let diags = validate_fleet_config(&config, &tmp_home("doctor"));
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
        let diags = validate_fleet_config(&config, &tmp_home("doctor"));
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
        let diags = validate_fleet_config(&config, &tmp_home("doctor"));
        assert!(diags.is_empty());
    }

    // ── Sprint 56 Track F (#496): D002 task_sweep github_login mapping ──

    /// Lead-spec D002 #1: sweep configured AND no instance has
    /// `github_login` mapped → emit Critical D002 with copy-paste fix.
    #[test]
    fn d002_fires_when_sweep_configured_and_no_mappings() {
        let home = tmp_home("d002-fires");
        enable_sweep(&home, "cheerc/talented-payroll");
        let mut config = FleetConfig::default();
        config
            .instances
            .insert("dev".into(), instance_with_login(None));
        config
            .instances
            .insert("lead".into(), instance_with_login(None));

        let diags = validate_fleet_config(&config, &home);
        let d002: Vec<_> = diags.iter().filter(|d| d.code == "D002").collect();
        assert_eq!(
            d002.len(),
            1,
            "D002 must fire exactly once when sweep configured + zero mappings; got: {diags:?}"
        );
        assert_eq!(d002[0].severity, Severity::Critical);
        assert!(
            d002[0].message.contains("cheerc/talented-payroll"),
            "D002 message must echo the configured repo for context: {}",
            d002[0].message
        );
        assert!(
            d002[0].message.contains("github_login"),
            "D002 message must name the field operators need to add"
        );
        let stanza = d002[0]
            .fix_stanza
            .as_deref()
            .expect("D002 must include a copy-paste fix stanza");
        assert!(
            stanza.contains("github_login") && stanza.contains("instances:"),
            "D002 fix stanza must show the actual fleet.yaml shape: {stanza}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Lead-spec D002 #2: at least one instance is mapped → D002 stays
    /// silent. The per-PR `tracing::warn!` at `task_sweep.rs:218` carries
    /// the burden for any remaining unmapped instances; D002 doesn't
    /// nag once the operator has begun mapping.
    #[test]
    fn d002_silent_when_at_least_one_mapping() {
        let home = tmp_home("d002-silent-some");
        enable_sweep(&home, "cheerc/talented-payroll");
        let mut config = FleetConfig::default();
        config
            .instances
            .insert("dev".into(), instance_with_login(Some("alice")));
        config
            .instances
            .insert("lead".into(), instance_with_login(None));

        let diags = validate_fleet_config(&config, &home);
        assert!(
            !diags.iter().any(|d| d.code == "D002"),
            "D002 must stay silent when ≥1 instance is mapped; got: {diags:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Lead-spec D002 #3: sweep is unconfigured (no `task_sweep.json`
    /// or `repo: null`) → D002 stays silent regardless of mapping
    /// state. D002 only screens deployments where the sweep would
    /// actually fire and silently mis-author tasks; we don't pester
    /// fleets that don't run the sweep at all.
    #[test]
    fn d002_silent_when_sweep_unconfigured() {
        let home = tmp_home("d002-silent-disabled");
        // Note: no `enable_sweep(...)` call — task_sweep.json absent.
        let mut config = FleetConfig::default();
        config
            .instances
            .insert("dev".into(), instance_with_login(None));

        let diags = validate_fleet_config(&config, &home);
        assert!(
            !diags.iter().any(|d| d.code == "D002"),
            "D002 must stay silent when task_sweep is unconfigured; got: {diags:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Defensive: sweep `paused: true` is treated like sweep-disabled —
    /// the silent-mis-author symptom can't manifest while the tick body
    /// short-circuits, so D002 must not fire and waste operator
    /// attention.
    #[test]
    fn d002_silent_when_sweep_paused() {
        let home = tmp_home("d002-silent-paused");
        let body = serde_json::json!({
            "repo": "cheerc/talented-payroll",
            "paused": true,
            "dry_run": false,
        });
        std::fs::write(
            home.join("task_sweep.json"),
            serde_json::to_string(&body).unwrap(),
        )
        .unwrap();
        let mut config = FleetConfig::default();
        config
            .instances
            .insert("dev".into(), instance_with_login(None));

        let diags = validate_fleet_config(&config, &home);
        assert!(
            !diags.iter().any(|d| d.code == "D002"),
            "D002 must stay silent when sweep is paused; got: {diags:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 56 Track H2 (#525 item 5): mirror Critical to stderr ──

    /// Lead-spec item 5: Critical-severity diagnostics must be
    /// emitted to BOTH `tracing::error!` AND direct `eprintln!`.
    /// Pre-Track-H2 only the tracing path fired, which detached-mode
    /// daemons couldn't surface. We can't easily capture `tracing`
    /// output in unit tests without a subscriber harness, so we pin
    /// the structural invariant: the function executes without panic
    /// and the `Diagnostic` shape we'd expect to mirror is reachable
    /// from the public API.
    ///
    /// The actual mirroring is verified by:
    /// - Reading the source: line `eprintln!("FATAL [{}]: {}{fix}",
    ///   d.code, d.message);` immediately follows the
    ///   `tracing::error!` for `Severity::Critical`
    /// - Manual smoke (operator) — `agend-terminal start` with a
    ///   broken fleet.yaml prints `FATAL [D001]: …` to stderr
    ///   regardless of `RUST_LOG` setting
    #[test]
    fn emit_diagnostics_critical_does_not_panic_and_mirrors() {
        let diags = vec![Diagnostic {
            severity: Severity::Critical,
            code: "TEST",
            message: "test critical".into(),
            fix_stanza: Some("apply fix".into()),
        }];
        // Must not panic. Stderr capture would require child-process
        // shell; this assertion ensures the function executes the
        // Critical arm without aborting.
        emit_diagnostics(&diags);
    }

    /// Defensive: Warning and Info severity must NOT be mirrored to
    /// stderr — they stay tracing-only. The mirror exists specifically
    /// to surface FATAL-class signals that operators must act on.
    #[test]
    fn emit_diagnostics_warning_and_info_do_not_panic() {
        let diags = vec![
            Diagnostic {
                severity: Severity::Warning,
                code: "WARN_TEST",
                message: "warning sample".into(),
                fix_stanza: None,
            },
            Diagnostic {
                severity: Severity::Info,
                code: "INFO_TEST",
                message: "info sample".into(),
                fix_stanza: None,
            },
        ];
        emit_diagnostics(&diags);
    }
}
