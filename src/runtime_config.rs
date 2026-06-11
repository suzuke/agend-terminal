//! #1085: Runtime-mutable configuration.
//!
//! `~/.agend-terminal/runtime-config.json` is read each daemon tick (10s).
//! Values override compile-time defaults. The MCP `config` tool provides
//! get/set access.

use crate::store::SchemaVersioned;
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
    /// state detection. Default true (opt-out — the operator wants the badge as a
    /// steady diagnostic). `default_true` also fills the field for older configs
    /// written before it existed. Hot-reloadable via the `config` MCP tool, like
    /// the other gates here.
    #[serde(default = "default_true")]
    pub show_pane_state: bool,
    /// #1990: on-disk schema version. `#[serde(default)]` → an older config
    /// written before this field reads back as 0 (≤ CURRENT, loads normally);
    /// a value > CURRENT means a newer daemon wrote it and is fail-closed in
    /// [`reload`] (keep-last-good / safe-default per #1576, never adopted).
    /// Additive bumps (new fields with serde defaults) do NOT need a version
    /// bump — only a non-additive change to an existing field does.
    #[serde(default)]
    pub schema_version: u32,
}

impl SchemaVersioned for RuntimeConfig {
    const CURRENT: u32 = 1;
    fn version_mut(&mut self) -> &mut u32 {
        &mut self.schema_version
    }
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
            show_pane_state: true,
            schema_version: RuntimeConfig::CURRENT,
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
            // #1990: a newer daemon wrote a schema we don't fully understand.
            // Route through the SAME #1576 fail-closed disposition as a corrupt
            // file (keep last-known-good at runtime, safe default at startup) —
            // never silently adopt a config we can't trust. NOT load_versioned,
            // which would reset to default at runtime and lose keep-last-good.
            Ok(config) if config.schema_version > RuntimeConfig::CURRENT => fail_closed(
                is_startup,
                &format!(
                    "schema_version {} > supported {}",
                    config.schema_version,
                    RuntimeConfig::CURRENT
                ),
            ),
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
    // #1990 (reviewer-2 P1): `config` comes from in-memory `get()` (the
    // keep-last-good snapshot), NOT the disk file — so a blind write here would
    // CLOBBER a future-version file a newer daemon wrote, downgrading it. Reads
    // are protected by keep-last-good; the write path needs its own guard. Check
    // the on-disk version first and refuse with a visible error (mirrors the
    // decisions-update fail-closed) rather than silently overwriting.
    if let Ok(disk) = std::fs::read_to_string(&path) {
        if let Ok(existing) = serde_json::from_str::<RuntimeConfig>(&disk) {
            if existing.schema_version > RuntimeConfig::CURRENT {
                return Err(format!(
                    "runtime-config.json was written by a newer schema version ({} > {}); refusing to overwrite — upgrade the daemon",
                    existing.schema_version,
                    RuntimeConfig::CURRENT
                ));
            }
        }
    }
    // #1990: stamp the current schema version on every write.
    config.schema_version = RuntimeConfig::CURRENT;
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

/// All config key names, derived from the serialized `Default` so callers (e.g.
/// the `config` MCP tool description) can never go stale — adding a struct field
/// makes it appear automatically. Keys equal the serde field names, which are
/// exactly what `get_key`/`set` match on.
pub fn keys() -> Vec<String> {
    serde_json::to_value(RuntimeConfig::default())
        .ok()
        .and_then(|v| v.as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()))
        .unwrap_or_default()
        .into_iter()
        // #1990: `schema_version` is on-disk metadata, not an operator-settable
        // key — keep it out of the `config` MCP tool's key list (set/get_key
        // reject it, so it must not appear as settable).
        .filter(|k| k != "schema_version")
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    // These tests mutate the process-global `RUNTIME_CONFIG` singleton via
    // `reload()`, so running them concurrently lets one test's reload clobber
    // another's value between its own `reload` + `get_key` — an intermittent
    // assertion flake that reddened UNRELATED PRs (#1752, #1758) and forced
    // churny CI reruns. Serialize the global-touching ones under a named group
    // (keeps unrelated `#[serial]` tests in other modules running in parallel).
    use serial_test::serial;

    #[test]
    fn default_values() {
        let c = RuntimeConfig::default();
        assert_eq!(c.dev_idle_threshold_secs, 3600);
        assert_eq!(c.fleet_idle_threshold_secs, 1800);
    }

    #[test]
    #[serial(runtime_config)]
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
    #[serial(runtime_config)]
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

    /// #1713/opt-out: the pane-state badge flag now defaults ON (operator wants
    /// it as a steady diagnostic) and stays runtime-toggleable via the same
    /// `set`/persist/reload path as the other gates (the `config` MCP tool reaches
    /// `set`), so the operator can flip it off without a rebuild.
    #[test]
    #[serial(runtime_config)]
    fn show_pane_state_default_on_and_toggleable() {
        assert!(
            RuntimeConfig::default().show_pane_state,
            "show_pane_state must default ON (opt-out)"
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

    /// `keys()` is the dynamic source for the `config` MCP tool description, so it
    /// must cover every settable key — including show_pane_state, whose omission
    /// from the hand-maintained description is what this change fixes. Every key
    /// `keys()` reports must also be a valid `set` target (round-trip via get_key).
    #[test]
    #[serial(runtime_config)]
    fn keys_cover_all_settable_keys_including_show_pane_state() {
        let ks = keys();
        assert!(
            ks.iter().any(|k| k == "show_pane_state"),
            "keys() must include show_pane_state (was missing from the MCP description): {ks:?}"
        );
        // Every reported key resolves via get_key — proves keys() ≡ valid keys.
        for k in &ks {
            assert!(
                get_key(k).is_ok(),
                "keys() reported a non-gettable key: {k}"
            );
        }
    }

    /// #1990 additive: a pre-#1990 config written WITHOUT a `schema_version`
    /// field must still load (the field defaults to 0 ≤ CURRENT).
    #[test]
    #[serial(runtime_config)]
    fn old_config_without_schema_version_loads() {
        let dir = std::env::temp_dir().join("agend-test-runtime-config-oldver");
        std::fs::create_dir_all(&dir).ok();
        std::fs::write(
            dir.join("runtime-config.json"),
            r#"{"dev_idle_threshold_secs": 4242}"#,
        )
        .unwrap();
        reload(&dir);
        assert_eq!(
            get_key("dev_idle_threshold_secs").unwrap(),
            "4242",
            "a pre-#1990 config (no schema_version) must still load"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #1990 + #1576: a config carrying a FUTURE `schema_version` must be
    /// fail-closed — NOT adopted — keeping the last-known-good value rather than
    /// reverting to defaults mid-run.
    #[test]
    #[serial(runtime_config)]
    fn future_schema_version_config_kept_last_good() {
        let dir = std::env::temp_dir().join("agend-test-runtime-config-futurever");
        std::fs::create_dir_all(&dir).ok();
        // Establish a known last-known-good first.
        set(&dir, "dev_idle_threshold_secs", "5555").unwrap();
        reload(&dir);
        assert_eq!(get_key("dev_idle_threshold_secs").unwrap(), "5555");
        // A newer daemon overwrites with a future schema + a different value.
        std::fs::write(
            dir.join("runtime-config.json"),
            r#"{"schema_version": 999, "dev_idle_threshold_secs": 1}"#,
        )
        .unwrap();
        reload(&dir);
        assert_eq!(
            get_key("dev_idle_threshold_secs").unwrap(),
            "5555",
            "a future-schema config must be rejected, keeping last-known-good (not adopting 1)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #1990 (reviewer-2 P1): `set` writes from the in-memory keep-last-good
    /// snapshot, so a blind write would clobber a future-version file on disk.
    /// It must refuse instead, leaving the newer daemon's file intact.
    #[test]
    #[serial(runtime_config)]
    fn set_refuses_to_overwrite_future_version_file() {
        let dir = std::env::temp_dir().join("agend-test-runtime-config-setfuture");
        std::fs::create_dir_all(&dir).ok();
        std::fs::write(
            dir.join("runtime-config.json"),
            r#"{"schema_version": 999, "dev_idle_threshold_secs": 1}"#,
        )
        .unwrap();
        let r = set(&dir, "dev_idle_threshold_secs", "7200");
        assert!(
            r.is_err(),
            "set must refuse to overwrite a future-version config: {r:?}"
        );
        let disk = std::fs::read_to_string(dir.join("runtime-config.json")).unwrap();
        assert!(
            disk.contains("999"),
            "the future-version file must be left intact, not downgraded: {disk}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
