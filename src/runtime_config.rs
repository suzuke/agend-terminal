//! #1085: Runtime-mutable configuration.
//!
//! `~/.agend-terminal/runtime-config.json` is read each daemon tick (10s).
//! Values override compile-time defaults. The MCP `config` tool provides
//! get/set access.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Runtime-mutable configuration values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Per-agent idle threshold before watchdog pings lead (seconds).
    #[serde(default = "default_dev_idle")]
    pub dev_idle_threshold_secs: i64,
    /// Fleet-wide idle threshold before watchdog alerts (seconds).
    #[serde(default = "default_fleet_idle")]
    pub fleet_idle_threshold_secs: i64,
    /// #1438: max TTL (seconds) for a fleet-idle ack. Backstop so an ack
    /// never suppresses forever when the task board makes no progress; once
    /// the ack is older than this it expires and the fleet is re-evaluated.
    #[serde(default = "default_fleet_ack_ttl")]
    pub fleet_idle_ack_ttl_secs: i64,
    /// #685 Phase 2: Master gate for hang auto-recovery stages 1-3.
    /// When true, Hung agents trigger ESC → restart → escalate.
    /// Default false (shadow mode only).
    #[serde(default)]
    pub hang_auto_recovery_enabled: bool,
    /// #1176: When true, UsageLimit on one agent propagates QuotaExceeded
    /// to all same-backend agents + gates new dispatches. Default false.
    #[serde(default)]
    pub usage_limit_propagation_enabled: bool,
    /// #1402: Master gate for idle watchdog. When false, scan_and_emit
    /// is a no-op — no dev-idle or fleet-idle alerts fire. Default true.
    #[serde(default = "default_true")]
    pub idle_watchdog_enabled: bool,
    /// #1713/#1523 aid: when true, each pane title appends a `[<State>]` text
    /// badge of the detected `AgentState` so the operator can eyeball-verify
    /// state detection. Default false (off — the colour dot is the steady-state
    /// signal; this is a temporary diagnostic toggle). Hot-reloadable via the
    /// `config` MCP tool, like the other gates here.
    #[serde(default)]
    pub show_pane_state: bool,
}

fn default_true() -> bool {
    true
}

fn default_dev_idle() -> i64 {
    3600
}
fn default_fleet_idle() -> i64 {
    1800
}
fn default_fleet_ack_ttl() -> i64 {
    2700
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            dev_idle_threshold_secs: default_dev_idle(),
            fleet_idle_threshold_secs: default_fleet_idle(),
            fleet_idle_ack_ttl_secs: default_fleet_ack_ttl(),
            hang_auto_recovery_enabled: false,
            usage_limit_propagation_enabled: false,
            idle_watchdog_enabled: true,
            show_pane_state: false,
        }
    }
}

static RUNTIME_CONFIG: OnceLock<RwLock<RuntimeConfig>> = OnceLock::new();

/// #1576: false until the first `reload()` — startup (no last-known-good →
/// missing/corrupt falls back to the safe DEFAULT) vs runtime (keep last-good).
static INITIALIZED: AtomicBool = AtomicBool::new(false);
/// #1576: de-dupes the corrupt-config warning across 10s ticks.
static CORRUPT_WARNED: AtomicBool = AtomicBool::new(false);

fn global() -> &'static RwLock<RuntimeConfig> {
    RUNTIME_CONFIG.get_or_init(|| RwLock::new(RuntimeConfig::default()))
}

/// Get a snapshot of the current runtime config.
pub fn get() -> RuntimeConfig {
    global().read().clone()
}

/// Reload config from disk. Called each daemon tick.
///
/// #1576 — fail-closed (no HMAC; runtime-config isn't an authority gate, so per
/// the single-user threat model it gets the footgun fix only, not a signature):
/// a corrupt or vanished file must NOT silently revert to defaults mid-run,
/// because the default flips watchdog/recovery gates and could silence alerts an
/// injected agent would want silenced. Disposition:
/// - valid → load it (clears the warn latch);
/// - corrupt/missing at STARTUP → the safe `Default` (watchdogs ON);
/// - corrupt/missing at RUNTIME → KEEP the last-known-good already in `global()`.
pub fn reload(home: &Path) {
    let is_startup = !INITIALIZED.swap(true, Ordering::SeqCst);
    let path = home.join("runtime-config.json");
    match std::fs::read_to_string(&path) {
        Ok(c) => match serde_json::from_str::<RuntimeConfig>(&c) {
            Ok(config) => {
                *global().write() = config;
                CORRUPT_WARNED.store(false, Ordering::Relaxed);
            }
            Err(e) => fail_closed(is_startup, &format!("unparseable: {e}")),
        },
        Err(_) if is_startup => {
            // No file on first load = fresh install → safe defaults.
            *global().write() = RuntimeConfig::default();
        }
        Err(_) => {
            // Vanished at runtime — keep last-known-good rather than resetting.
            fail_closed(false, "runtime-config.json disappeared");
        }
    }
}

/// #1576: on a corrupt/missing config, fall back safely. At startup adopt the
/// `Default` (watchdogs ON); at runtime keep the last-known-good (do not touch
/// `global()`). Warns once per episode either way.
fn fail_closed(is_startup: bool, reason: &str) {
    if is_startup {
        *global().write() = RuntimeConfig::default();
    }
    if !CORRUPT_WARNED.swap(true, Ordering::Relaxed) {
        let disposition = if is_startup {
            "using safe defaults (watchdogs enabled)"
        } else {
            "keeping last-known-good config"
        };
        tracing::warn!(
            reason,
            disposition,
            "#1576: runtime-config.json failed to load — {disposition}; not reverting \
             to defaults mid-run (a corrupt config must not silently disable watchdogs)."
        );
    }
}

/// Set a single config key and persist to disk.
pub fn set(home: &Path, key: &str, value: &str) -> Result<String, String> {
    let mut config = get();
    match key {
        "dev_idle_threshold_secs" => {
            config.dev_idle_threshold_secs = value
                .parse()
                .map_err(|_| format!("invalid integer: {value}"))?;
        }
        "fleet_idle_threshold_secs" => {
            config.fleet_idle_threshold_secs = value
                .parse()
                .map_err(|_| format!("invalid integer: {value}"))?;
        }
        "fleet_idle_ack_ttl_secs" => {
            config.fleet_idle_ack_ttl_secs = value
                .parse()
                .map_err(|_| format!("invalid integer: {value}"))?;
        }
        "hang_auto_recovery_enabled" => {
            config.hang_auto_recovery_enabled = match value {
                "true" | "1" => true,
                "false" | "0" => false,
                _ => return Err(format!("invalid boolean: {value} (use true/false)")),
            };
        }
        "usage_limit_propagation_enabled" => {
            config.usage_limit_propagation_enabled = match value {
                "true" | "1" => true,
                "false" | "0" => false,
                _ => return Err(format!("invalid boolean: {value} (use true/false)")),
            };
        }
        "idle_watchdog_enabled" => {
            config.idle_watchdog_enabled = match value {
                "true" | "1" => true,
                "false" | "0" => false,
                _ => return Err(format!("invalid boolean: {value} (use true/false)")),
            };
        }
        "show_pane_state" => {
            config.show_pane_state = match value {
                "true" | "1" => true,
                "false" | "0" => false,
                _ => return Err(format!("invalid boolean: {value} (use true/false)")),
            };
        }
        _ => return Err(format!("unknown config key: {key}")),
    }
    let path = home.join("runtime-config.json");
    let json = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    *global().write() = config.clone();
    Ok(serde_json::to_string(&config).unwrap_or_default())
}

/// Get a single config value by key.
pub fn get_key(key: &str) -> Result<String, String> {
    let config = get();
    match key {
        "dev_idle_threshold_secs" => Ok(config.dev_idle_threshold_secs.to_string()),
        "fleet_idle_threshold_secs" => Ok(config.fleet_idle_threshold_secs.to_string()),
        "fleet_idle_ack_ttl_secs" => Ok(config.fleet_idle_ack_ttl_secs.to_string()),
        "hang_auto_recovery_enabled" => Ok(config.hang_auto_recovery_enabled.to_string()),
        "usage_limit_propagation_enabled" => Ok(config.usage_limit_propagation_enabled.to_string()),
        "idle_watchdog_enabled" => Ok(config.idle_watchdog_enabled.to_string()),
        "show_pane_state" => Ok(config.show_pane_state.to_string()),
        _ => Err(format!("unknown config key: {key}")),
    }
}

/// List all config keys and values.
pub fn list() -> serde_json::Value {
    serde_json::to_value(get()).unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn default_values() {
        let c = RuntimeConfig::default();
        assert_eq!(c.dev_idle_threshold_secs, 3600);
        assert_eq!(c.fleet_idle_threshold_secs, 1800);
    }

    #[test]
    fn set_and_get_key() {
        let dir = std::env::temp_dir().join("agend-test-runtime-config");
        std::fs::create_dir_all(&dir).ok();
        set(&dir, "dev_idle_threshold_secs", "7200").unwrap();
        reload(&dir);
        assert_eq!(get_key("dev_idle_threshold_secs").unwrap(), "7200");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_key_rejected() {
        let dir = std::env::temp_dir().join("agend-test-runtime-config-bad");
        std::fs::create_dir_all(&dir).ok();
        assert!(set(&dir, "nonexistent", "123").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hang_auto_recovery_enabled_default_false() {
        let c = RuntimeConfig::default();
        assert!(!c.hang_auto_recovery_enabled);
    }

    #[test]
    fn set_hang_auto_recovery_enabled() {
        let dir = std::env::temp_dir().join("agend-test-runtime-config-hang");
        std::fs::create_dir_all(&dir).ok();
        set(&dir, "hang_auto_recovery_enabled", "true").unwrap();
        reload(&dir);
        assert_eq!(get_key("hang_auto_recovery_enabled").unwrap(), "true");
        set(&dir, "hang_auto_recovery_enabled", "false").unwrap();
        reload(&dir);
        assert_eq!(get_key("hang_auto_recovery_enabled").unwrap(), "false");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #1713: the pane-state badge flag is OFF by default and runtime-toggleable
    /// via the same `set`/persist/reload path as the other gates (the `config`
    /// MCP tool reaches `set`), so the operator can flip it without a rebuild.
    #[test]
    fn show_pane_state_default_off_and_toggleable_1713() {
        assert!(
            !RuntimeConfig::default().show_pane_state,
            "show_pane_state must default OFF"
        );
        let dir = std::env::temp_dir().join("agend-test-runtime-config-panestate");
        std::fs::create_dir_all(&dir).ok();
        set(&dir, "show_pane_state", "true").unwrap();
        reload(&dir);
        assert_eq!(get_key("show_pane_state").unwrap(), "true");
        set(&dir, "show_pane_state", "false").unwrap();
        reload(&dir);
        assert_eq!(get_key("show_pane_state").unwrap(), "false");
        std::fs::remove_dir_all(&dir).ok();
    }
}
