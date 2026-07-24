//! Watchdog topology config — which agent the idle watchdog watches and who
//! receives each watchdog / anti-stall / decision-timeout notification.
//!
//! These are fleet *topology* (agent + recipient names), so their home is
//! `fleet.yaml`, not env vars. Five `AGEND_*` env vars previously carried them;
//! they remain as a **deprecated fallback for one window**, so existing setups
//! keep working unchanged. Resolution precedence for every field:
//!
//! 1. `fleet.yaml` `watchdog:` block (when the field is set / non-empty)
//! 2. the legacy `AGEND_*` env var (deprecated — warns once per process)
//! 3. the built-in default
//!
//! Remove the env layer after operators have migrated to `fleet.yaml`.

use super::{fleet_yaml_path, FleetConfig};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// `fleet.yaml` top-level `watchdog:` block. Every field is optional; an omitted
/// field falls through to the env fallback, then the built-in default (see module
/// docs). Defaults reproduce the pre-migration hard-coded behaviour exactly, so a
/// fleet.yaml without a `watchdog:` block is byte-for-byte unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WatchdogConfig {
    /// Legacy **single-agent mode** override for the dev-vantage idle watchdog.
    /// When set, the watchdog watches ONLY this agent (with the global
    /// `dev_idle_threshold_secs`) instead of iterating every fleet instance —
    /// identical to the old `AGEND_IDLE_WATCHDOG_AGENT` behaviour. Omit it
    /// (the default) to keep the modern per-instance iteration. Default: unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_watchdog_agent: Option<String>,
    /// Recipient for dev-vantage idle alerts. Default `lead`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_recipient: Option<String>,
    /// Recipient for fleet-vantage idle alerts ("the whole fleet is quiet").
    /// Default `lead` (#1563: was `general`, which spammed the general assistant).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet_recipient: Option<String>,
    /// Recipients for task-stall warnings. Default `[general, lead]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_stall_recipients: Vec<String>,
    /// Recipients for helper-staleness alerts. Default `[general, lead]`,
    /// filtered against the `instances:` map (ghost-inbox guard) — see
    /// [`resolve_helper_staleness_recipients`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub helper_staleness_recipients: Vec<String>,
    /// Recipient for the decision-timeout auto-default (operator-proceed)
    /// emission. Default `general`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_timeout_recipient: Option<String>,
}

/// Load the `watchdog:` block from `fleet.yaml` (cached by mtime via
/// [`FleetConfig::load`]). `None` when fleet.yaml is missing/unparseable — the
/// resolvers then fall through to the env / default layers.
fn load(home: &Path) -> Option<WatchdogConfig> {
    FleetConfig::load(&fleet_yaml_path(home))
        .ok()
        .map(|c| c.watchdog)
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

fn env_nonempty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|s| !s.trim().is_empty())
}

/// Warn once per process that a watchdog topology value came from the deprecated
/// env fallback rather than `fleet.yaml`. Called per-tick, so dedup is mandatory.
fn warn_env_deprecated(var: &str, warned: &AtomicBool) {
    if !warned.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            env = var,
            "watchdog topology via env is deprecated — move it to the fleet.yaml \
             `watchdog:` block. The env fallback will be removed after the \
             deprecation window."
        );
    }
}

/// Single-agent override for the dev-vantage idle watchdog. `Some` switches the
/// watchdog to legacy single-agent mode; `None` keeps per-instance iteration.
/// Precedence: fleet `watchdog.idle_watchdog_agent` > `AGEND_IDLE_WATCHDOG_AGENT`.
pub fn resolve_idle_watchdog_agent(home: &Path) -> Option<String> {
    if let Some(v) = load(home).and_then(|w| nonempty(w.idle_watchdog_agent)) {
        return Some(v);
    }
    static WARNED: AtomicBool = AtomicBool::new(false);
    match env_nonempty("AGEND_IDLE_WATCHDOG_AGENT") {
        Some(v) => {
            warn_env_deprecated("AGEND_IDLE_WATCHDOG_AGENT", &WARNED);
            Some(v)
        }
        None => None,
    }
}

/// Recipient for dev-vantage idle alerts. Default `lead`.
pub fn resolve_dev_idle_recipient(home: &Path) -> String {
    if let Some(v) = load(home).and_then(|w| nonempty(w.dev_recipient)) {
        return v;
    }
    static WARNED: AtomicBool = AtomicBool::new(false);
    match env_nonempty("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT") {
        Some(v) => {
            warn_env_deprecated("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT", &WARNED);
            v
        }
        None => "lead".to_string(),
    }
}

/// Recipient for fleet-vantage idle alerts. Default `lead` (#1563).
pub fn resolve_fleet_idle_recipient(home: &Path) -> String {
    if let Some(v) = load(home).and_then(|w| nonempty(w.fleet_recipient)) {
        return v;
    }
    static WARNED: AtomicBool = AtomicBool::new(false);
    match env_nonempty("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT") {
        Some(v) => {
            warn_env_deprecated("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT", &WARNED);
            v
        }
        None => "lead".to_string(),
    }
}

/// Recipients for task-stall warnings. Default `[general, lead]`. The env form
/// is comma-separated; entries are trimmed and empties filtered.
pub fn resolve_task_stall_recipients(home: &Path) -> Vec<String> {
    if let Some(w) = load(home) {
        if !w.task_stall_recipients.is_empty() {
            return w.task_stall_recipients;
        }
    }
    static WARNED: AtomicBool = AtomicBool::new(false);
    if let Some(custom) = env_nonempty("AGEND_TASK_STALL_RECIPIENTS") {
        let parsed: Vec<String> = custom
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !parsed.is_empty() {
            warn_env_deprecated("AGEND_TASK_STALL_RECIPIENTS", &WARNED);
            return parsed;
        }
    }
    vec!["general".to_string(), "lead".to_string()]
}

/// Recipients for helper-staleness alerts. Default `[general, lead]`. No env
/// fallback — the env layer is deprecated and this field is new.
///
/// Ghost-inbox guard (t-20260723093520705757-38191-14): the resolved list is
/// filtered against the fleet.yaml `instances:` map before use — enqueueing to
/// a recipient with no instance just grows `~/.agend/inbox/<name>.jsonl`
/// forever with nobody to drain it. A missing/unparseable fleet.yaml skips the
/// filter (no fleet = no restriction, mirroring `tasks::acl::instance_exists`).
/// If the filter empties the list, the proactive page is dropped (warned,
/// deduped per process) — the operator-pull `agend-terminal doctor` still
/// surfaces the same staleness.
pub fn resolve_helper_staleness_recipients(home: &Path) -> Vec<String> {
    let base = match load(home) {
        Some(w) if !w.helper_staleness_recipients.is_empty() => w.helper_staleness_recipients,
        _ => vec!["general".to_string(), "lead".to_string()],
    };
    let Some(instances) = fleet_instance_names(home) else {
        return base;
    };
    let (kept, dropped): (Vec<String>, Vec<String>) =
        base.into_iter().partition(|r| instances.contains(r));
    if !dropped.is_empty() {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                dropped = ?dropped,
                "helper-staleness recipients without a fleet.yaml instance \
                 skipped (ghost-inbox guard)"
            );
        }
    }
    kept
}

/// The fleet.yaml `instances:` name set; `None` when fleet.yaml is missing or
/// unparseable — callers then skip existence filtering (permissive).
fn fleet_instance_names(home: &Path) -> Option<std::collections::HashSet<String>> {
    let p = fleet_yaml_path(home);
    if !p.exists() {
        return None;
    }
    FleetConfig::load(&p)
        .ok()
        .map(|c| c.instances.keys().cloned().collect())
}

/// Recipient for the decision-timeout auto-default emission. Default `general`.
pub fn resolve_decision_timeout_recipient(home: &Path) -> String {
    if let Some(v) = load(home).and_then(|w| nonempty(w.decision_timeout_recipient)) {
        return v;
    }
    static WARNED: AtomicBool = AtomicBool::new(false);
    match env_nonempty("AGEND_DECISION_TIMEOUT_RECIPIENT") {
        Some(v) => {
            warn_env_deprecated("AGEND_DECISION_TIMEOUT_RECIPIENT", &WARNED);
            v
        }
        None => "general".to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Serialize tests that mutate the process-global env vars + the fleet cache.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static G: std::sync::Mutex<()> = std::sync::Mutex::new(());
        G.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn tmp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-watchdog-cfg-{}-{}-{}",
            tag,
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&dir).ok();
        dir
    }

    fn write_fleet(home: &Path, yaml: &str) {
        fs::write(fleet_yaml_path(home), yaml).expect("write fleet.yaml");
    }

    fn clear_env() {
        for v in [
            "AGEND_IDLE_WATCHDOG_AGENT",
            "AGEND_IDLE_WATCHDOG_DEV_RECIPIENT",
            "AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT",
            "AGEND_TASK_STALL_RECIPIENTS",
            "AGEND_DECISION_TIMEOUT_RECIPIENT",
        ] {
            std::env::remove_var(v);
        }
    }

    #[test]
    fn parses_watchdog_block() {
        let _g = env_guard();
        let home = tmp_home("parse");
        write_fleet(
            &home,
            r#"
watchdog:
  idle_watchdog_agent: dev
  dev_recipient: lead
  fleet_recipient: ops-bot
  task_stall_recipients:
    - alice
    - bob
  decision_timeout_recipient: carol
instances: {}
"#,
        );
        let cfg = FleetConfig::load(&fleet_yaml_path(&home)).expect("load");
        assert_eq!(cfg.watchdog.idle_watchdog_agent.as_deref(), Some("dev"));
        assert_eq!(cfg.watchdog.dev_recipient.as_deref(), Some("lead"));
        assert_eq!(cfg.watchdog.fleet_recipient.as_deref(), Some("ops-bot"));
        assert_eq!(cfg.watchdog.task_stall_recipients, vec!["alice", "bob"]);
        assert_eq!(
            cfg.watchdog.decision_timeout_recipient.as_deref(),
            Some("carol")
        );
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn omitted_block_defaults_to_builtins() {
        // Zero-migration: a fleet.yaml without `watchdog:` + no env → built-ins.
        let _g = env_guard();
        clear_env();
        let home = tmp_home("defaults");
        write_fleet(&home, "instances: {}\n");
        assert_eq!(resolve_idle_watchdog_agent(&home), None);
        assert_eq!(resolve_dev_idle_recipient(&home), "lead");
        // #1563: fleet-idle default must be lead, not general.
        assert_eq!(resolve_fleet_idle_recipient(&home), "lead");
        assert_eq!(
            resolve_task_stall_recipients(&home),
            vec!["general".to_string(), "lead".to_string()]
        );
        assert_eq!(resolve_decision_timeout_recipient(&home), "general");
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fleet_config_wins_over_env() {
        // Precedence: fleet.yaml value beats the env fallback.
        let _g = env_guard();
        clear_env();
        std::env::set_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT", "env-dev");
        std::env::set_var("AGEND_TASK_STALL_RECIPIENTS", "env-a, env-b");
        std::env::set_var("AGEND_DECISION_TIMEOUT_RECIPIENT", "env-dec");
        let home = tmp_home("fleet-wins");
        write_fleet(
            &home,
            r#"
watchdog:
  dev_recipient: yaml-dev
  task_stall_recipients:
    - yaml-a
  decision_timeout_recipient: yaml-dec
instances: {}
"#,
        );
        assert_eq!(resolve_dev_idle_recipient(&home), "yaml-dev");
        assert_eq!(resolve_task_stall_recipients(&home), vec!["yaml-a"]);
        assert_eq!(resolve_decision_timeout_recipient(&home), "yaml-dec");
        clear_env();
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn env_used_when_fleet_field_absent() {
        // Deprecation fallback: with no fleet value, the env wins over default.
        let _g = env_guard();
        clear_env();
        std::env::set_var("AGEND_IDLE_WATCHDOG_AGENT", "single-dev");
        std::env::set_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT", "env-fleet");
        let home = tmp_home("env-fallback");
        write_fleet(&home, "watchdog: {}\ninstances: {}\n");
        assert_eq!(
            resolve_idle_watchdog_agent(&home),
            Some("single-dev".to_string())
        );
        assert_eq!(resolve_fleet_idle_recipient(&home), "env-fleet");
        clear_env();
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_stall_env_comma_split_and_whitespace_filtered() {
        let _g = env_guard();
        clear_env();
        std::env::set_var("AGEND_TASK_STALL_RECIPIENTS", " alice, bob ,, carol ");
        let home = tmp_home("stall-split");
        write_fleet(&home, "instances: {}\n");
        assert_eq!(
            resolve_task_stall_recipients(&home),
            vec!["alice".to_string(), "bob".to_string(), "carol".to_string()]
        );
        // Whitespace-only env falls back to the built-in default.
        std::env::set_var("AGEND_TASK_STALL_RECIPIENTS", "   ");
        assert_eq!(
            resolve_task_stall_recipients(&home),
            vec!["general".to_string(), "lead".to_string()]
        );
        clear_env();
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn missing_fleet_yaml_falls_back_to_env_then_default() {
        // No fleet.yaml at all (load fails) → env, then default.
        let _g = env_guard();
        clear_env();
        let home = tmp_home("no-yaml");
        // No fleet.yaml written.
        assert_eq!(resolve_dev_idle_recipient(&home), "lead");
        std::env::set_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT", "env-dev");
        assert_eq!(resolve_dev_idle_recipient(&home), "env-dev");
        clear_env();
        fs::remove_dir_all(&home).ok();
    }

    /// Ghost-inbox guard edges: recipients missing from `instances:` are
    /// dropped; a map naming NONE of them resolves empty (proactive page
    /// skipped); a missing fleet.yaml skips the filter entirely. Separate
    /// homes per shape — `FleetConfig::load` caches by mtime, and two writes
    /// to one path within the same second could serve the stale parse.
    #[test]
    fn helper_staleness_recipients_filtered_by_instances() {
        let _g = env_guard();
        clear_env();
        let partial = tmp_home("hs-filter-partial");
        write_fleet(&partial, "instances:\n  general: {}\n");
        assert_eq!(
            resolve_helper_staleness_recipients(&partial),
            vec!["general".to_string()],
            "lead has no instance — must be dropped, general kept"
        );
        let all_ghost = tmp_home("hs-filter-all-ghost");
        write_fleet(&all_ghost, "instances: {}\n");
        assert_eq!(
            resolve_helper_staleness_recipients(&all_ghost),
            Vec::<String>::new(),
            "no default recipient has an instance — resolve empty, page skipped"
        );
        let no_yaml = tmp_home("hs-filter-no-yaml");
        assert_eq!(
            resolve_helper_staleness_recipients(&no_yaml),
            vec!["general".to_string(), "lead".to_string()],
            "no fleet.yaml = no restriction — unfiltered built-in default"
        );
        for h in [partial, all_ghost, no_yaml] {
            fs::remove_dir_all(&h).ok();
        }
    }
}
