//! #1085: Runtime-mutable configuration.
//!
//! `~/.agend-terminal/runtime-config.json` is read each daemon tick (10s).
//! Values override compile-time defaults. The MCP `config` tool provides
//! get/set access.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::Path;
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

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            dev_idle_threshold_secs: default_dev_idle(),
            fleet_idle_threshold_secs: default_fleet_idle(),
            hang_auto_recovery_enabled: false,
            usage_limit_propagation_enabled: false,
            idle_watchdog_enabled: true,
        }
    }
}

static RUNTIME_CONFIG: OnceLock<RwLock<RuntimeConfig>> = OnceLock::new();

fn global() -> &'static RwLock<RuntimeConfig> {
    RUNTIME_CONFIG.get_or_init(|| RwLock::new(RuntimeConfig::default()))
}

/// Get a snapshot of the current runtime config.
pub fn get() -> RuntimeConfig {
    global().read().clone()
}

/// Reload config from disk. Called each daemon tick.
pub fn reload(home: &Path) {
    let path = home.join("runtime-config.json");
    let config = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str::<RuntimeConfig>(&c).ok())
        .unwrap_or_default();
    *global().write() = config;
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
        "hang_auto_recovery_enabled" => Ok(config.hang_auto_recovery_enabled.to_string()),
        "usage_limit_propagation_enabled" => Ok(config.usage_limit_propagation_enabled.to_string()),
        "idle_watchdog_enabled" => Ok(config.idle_watchdog_enabled.to_string()),
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
}
