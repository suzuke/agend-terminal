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
    /// #2325: text-selection copy mode. `true` (DEFAULT — Mode B "copy-on-select")
    /// auto-copies the selection to the clipboard when a drag is released, so a
    /// dedicated copy key is unnecessary (works in Terminal.app and other
    /// non-Kitty terminals out of the box). `false` (Mode A "explicit copy",
    /// #2302) keeps the highlight on release and copies only via the copy key
    /// (Cmd+C / Ctrl+Shift+C). Read by the TUI mouse handler at selection
    /// release; toggle via `Ctrl+B e` or `:set copy_on_select on|off`.
    /// `default_true` also fills the field for configs written before it existed.
    #[serde(default = "default_true")]
    pub copy_on_select: bool,
    /// dim-unfocused (t-…50430): when true (DEFAULT), every NON-focused pane's content is
    /// blended toward the (dark) terminal background so the focused pane stands out at a
    /// glance — a cross-terminal focus aid that does NOT rely on the terminal-dependent
    /// `Modifier::DIM`. Read live by `render_pane`; toggle via the `config` MCP tool /
    /// `:set dim_unfocused_panes on|off`. `default_true` also fills it for configs written
    /// before it existed (additive — no schema bump).
    #[serde(default = "default_true")]
    pub dim_unfocused_panes: bool,
    /// #2413 (A): when true (DEFAULT), the pane state badge shows the Shadow
    /// Observer's HIGH-CONFIDENCE correction (`observed_status`, gated to
    /// Hook/Stream + Confirmed/Strong) in place of the raw screen-scrape — e.g. an
    /// agent mid-API-call that renders `[Idle]` shows `[Thinking]`, an approval gate
    /// shows `[AwaitingOperator]`. Weak / screen-only backends keep the raw state
    /// (the gate can't fire), so this never regresses them. `false` keeps every
    /// badge on the raw screen state. The `AGEND_SHADOW_OBSERVER=0` kill-switch
    /// disables the whole observer (and so this) regardless. Read live by
    /// `render::build_agent_state_snapshot`; toggle via `:set observed_badge on|off`.
    /// `default_true` also fills the field for configs written before it existed.
    #[serde(default = "default_true")]
    pub observed_badge: bool,
    /// #1990: on-disk schema version. `#[serde(default)]` → an older config
    /// written before this field reads back as 0 (≤ CURRENT, loads normally);
    /// a value > CURRENT means a newer daemon wrote it and is fail-closed in
    /// [`reload`] (keep-last-good / safe-default per #1576, never adopted).
    /// Additive bumps (new fields with serde defaults) do NOT need a version
    /// bump — only a non-additive change to an existing field does.
    #[serde(default)]
    pub schema_version: u32,
    /// Context-window usage percent at which the per-tick context-alert watchdog notifies.
    #[serde(default = "default_context_alert")]
    pub context_alert_pct: f32,
    /// Context-window usage percent at which the context-handoff watchdog injects a handoff request.
    #[serde(default = "default_context_handoff")]
    pub context_handoff_pct: f32,
    /// Higher context-window percent at which the handoff watchdog escalates to the operator.
    #[serde(default = "default_context_handoff_escalate")]
    pub context_handoff_escalate_pct: f32,
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

pub const DEFAULT_ALERT_PCT: f32 = 80.0;
pub const DEFAULT_HANDOFF_PCT: f32 = 85.0;
pub const DEFAULT_ESCALATE_PCT: f32 = 92.0;

/// Re-arm requires the usage to drop this far below a threshold (compact/restart)
/// before a handler's latch re-arms — the single source of truth shared by the
/// per-tick consumers (`context_alert`/`context_handoff`) AND by
/// `validate_thresholds`' lower bound: a threshold <= this floor makes the
/// re-arm condition `pct < threshold - HYSTERESIS_PCT` impossible, so such
/// values are rejected as invalid.
pub const HYSTERESIS_PCT: f32 = 5.0;

fn default_dev_idle() -> i64 {
    3600
}
fn default_fleet_idle() -> i64 {
    1800
}
fn default_fleet_ack_ttl() -> i64 {
    2700
}
fn default_context_alert() -> f32 {
    DEFAULT_ALERT_PCT
}
fn default_context_handoff() -> f32 {
    DEFAULT_HANDOFF_PCT
}
fn default_context_handoff_escalate() -> f32 {
    DEFAULT_ESCALATE_PCT
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
            copy_on_select: true,
            dim_unfocused_panes: true,
            observed_badge: true,
            context_alert_pct: default_context_alert(),
            context_handoff_pct: default_context_handoff(),
            context_handoff_escalate_pct: default_context_handoff_escalate(),
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
                if let Err(msg) = validate_thresholds(
                    config.context_alert_pct,
                    config.context_handoff_pct,
                    config.context_handoff_escalate_pct,
                ) {
                    fail_closed(is_startup, &format!("invalid context thresholds: {msg}"));
                } else {
                    *global().write() = config;
                    CORRUPT_WARNED.store(false, Ordering::Relaxed);
                }
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

/// Validate the context threshold values triplet semantically.
pub fn validate_thresholds(alert: f32, handoff: f32, escalate: f32) -> Result<(), String> {
    if !alert.is_finite() || !handoff.is_finite() || !escalate.is_finite() {
        return Err("values must be finite".to_string());
    }
    // Values <= HYSTERESIS_PCT are invalid: the re-arm condition
    // `pct < threshold - HYSTERESIS_PCT` would be impossible, wedging the latch.
    if alert <= HYSTERESIS_PCT || alert > 100.0 {
        return Err(format!(
            "alert_pct must be in ({HYSTERESIS_PCT}, 100.0], got {alert}"
        ));
    }
    if handoff <= HYSTERESIS_PCT || handoff > 100.0 {
        return Err(format!(
            "handoff_pct must be in ({HYSTERESIS_PCT}, 100.0], got {handoff}"
        ));
    }
    if escalate <= HYSTERESIS_PCT || escalate > 100.0 {
        return Err(format!(
            "escalate_pct must be in ({HYSTERESIS_PCT}, 100.0], got {escalate}"
        ));
    }
    if alert >= handoff {
        return Err(format!(
            "alert_pct ({alert}) must be less than handoff_pct ({handoff})"
        ));
    }
    if handoff >= escalate {
        return Err(format!(
            "handoff_pct ({handoff}) must be less than escalate_pct ({escalate})"
        ));
    }
    Ok(())
}

/// Resolve and validate the effective context threshold triplet.
/// Checks the environment variables first, falling back to RuntimeConfig, then to defaults.
pub fn resolve_effective_thresholds() -> (f32, f32, f32) {
    let config = get();
    let alert = std::env::var("AGEND_CONTEXT_ALERT_PCT")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(config.context_alert_pct);
    let handoff = std::env::var("AGEND_CONTEXT_HANDOFF_PCT")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(config.context_handoff_pct);
    let escalate = std::env::var("AGEND_CONTEXT_HANDOFF_ESCALATE_PCT")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(config.context_handoff_escalate_pct);

    if validate_thresholds(alert, handoff, escalate).is_ok() {
        (alert, handoff, escalate)
    } else {
        tracing::warn!(
            alert,
            handoff,
            escalate,
            config_alert = config.context_alert_pct,
            config_handoff = config.context_handoff_pct,
            config_escalate = config.context_handoff_escalate_pct,
            "effective context thresholds combination is invalid. falling back to runtime config values."
        );
        let fallback_alert = config.context_alert_pct;
        let fallback_handoff = config.context_handoff_pct;
        let fallback_escalate = config.context_handoff_escalate_pct;
        if validate_thresholds(fallback_alert, fallback_handoff, fallback_escalate).is_ok() {
            (fallback_alert, fallback_handoff, fallback_escalate)
        } else {
            (DEFAULT_ALERT_PCT, DEFAULT_HANDOFF_PCT, DEFAULT_ESCALATE_PCT)
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
    // AUDIT2-012: serialize the whole read-modify-write under a cross-process file
    // lock AND base the mutation on the freshest ON-DISK config (read under the
    // lock), not the in-memory snapshot. The TUI and the daemon are separate
    // processes that both write this file (the TUI via `:set` / copy-on-select,
    // the daemon via the MCP `config set` tool); basing the write on each
    // process's stale `get()` global would let one clobber a key the other just
    // wrote (lost update). Fall back to the in-memory keep-last-good only if the
    // file is absent or corrupt. (The #1990 version guard below still refuses a
    // newer-schema file before any write.)
    let lock_path = home.join("runtime-config.json.lock");
    // AUDIT2-012 (review): FAIL CLOSED if the lock can't be acquired — proceeding
    // unlocked would silently re-open the cross-process lost-update this guard
    // exists to prevent. Surface the error instead.
    let _lock = crate::store::acquire_file_lock(&lock_path)
        .map_err(|e| format!("runtime-config lock unavailable ({lock_path:?}): {e}"))?;
    let mut config = std::fs::read_to_string(home.join("runtime-config.json"))
        .ok()
        .and_then(|d| serde_json::from_str::<RuntimeConfig>(&d).ok())
        .unwrap_or_else(get);
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
        "copy_on_select" => {
            // #2325: also accept on/off (the operator-facing `:set copy_on_select
            // on|off` vocabulary) alongside the usual true/false/1/0.
            config.copy_on_select = match value {
                "true" | "1" | "on" => true,
                "false" | "0" | "off" => false,
                _ => return Err(format!("invalid boolean: {value} (use on/off)")),
            };
        }
        "dim_unfocused_panes" => {
            // operator-facing UX toggle — accept on/off alongside true/false/1/0.
            config.dim_unfocused_panes = match value {
                "true" | "1" | "on" => true,
                "false" | "0" | "off" => false,
                _ => return Err(format!("invalid boolean: {value} (use on/off)")),
            };
        }
        "observed_badge" => {
            // #2413 (A): operator-facing badge-correction toggle — accept on/off
            // alongside true/false/1/0.
            config.observed_badge = match value {
                "true" | "1" | "on" => true,
                "false" | "0" | "off" => false,
                _ => return Err(format!("invalid boolean: {value} (use on/off)")),
            };
        }
        "context_alert_pct" => {
            config.context_alert_pct = value
                .parse()
                .map_err(|_| format!("invalid float: {value}"))?;
        }
        "context_handoff_pct" => {
            config.context_handoff_pct = value
                .parse()
                .map_err(|_| format!("invalid float: {value}"))?;
        }
        "context_handoff_escalate_pct" => {
            config.context_handoff_escalate_pct = value
                .parse()
                .map_err(|_| format!("invalid float: {value}"))?;
        }
        _ => return Err(format!("unknown config key: {key}")),
    }
    validate_thresholds(
        config.context_alert_pct,
        config.context_handoff_pct,
        config.context_handoff_escalate_pct,
    )?;
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
    // AUDIT2-012: atomic tmp+rename (was a plain std::fs::write) so a crash mid
    // write can't leave truncated JSON that reverts to DEFAULTS at next startup —
    // which would silently flip watchdog / recovery gates.
    crate::store::atomic_write(&path, json.as_bytes()).map_err(|e| e.to_string())?;
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
        "copy_on_select" => Ok(config.copy_on_select.to_string()),
        "dim_unfocused_panes" => Ok(config.dim_unfocused_panes.to_string()),
        "observed_badge" => Ok(config.observed_badge.to_string()),
        "context_alert_pct" => Ok(config.context_alert_pct.to_string()),
        "context_handoff_pct" => Ok(config.context_handoff_pct.to_string()),
        "context_handoff_escalate_pct" => Ok(config.context_handoff_escalate_pct.to_string()),
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
    fn set_uses_disk_base_preserves_concurrent_key_audit2_012() {
        let dir = std::env::temp_dir().join(format!(
            "agend-test-runtime-config-2012-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("runtime-config.json");

        // Simulate ANOTHER process (e.g. the daemon) having written
        // dev_idle_threshold_secs=9999 to disk while THIS process's in-memory
        // global is stale. (#2549: was `progress_mode` before that field was
        // retired — any field works here, the test is about the disk-base
        // mechanism, not this specific key's semantics.)
        let on_disk = RuntimeConfig {
            dev_idle_threshold_secs: 9999,
            ..RuntimeConfig::default()
        };
        std::fs::write(&path, serde_json::to_string_pretty(&on_disk).unwrap()).unwrap();

        // This process sets a DIFFERENT key. With the disk-base read under the
        // lock, the concurrently-written key must survive (not revert to default).
        set(&dir, "copy_on_select", "off").unwrap();

        let after: RuntimeConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            after.dev_idle_threshold_secs, 9999,
            "AUDIT2-012: a concurrently-written key must be preserved, not clobbered"
        );
        assert!(!after.copy_on_select, "the just-set key must be applied");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #2549: `progress_mode` was retired (ProgressBackstop/ProgressMirror
    /// deleted). An on-disk `runtime-config.json` from BEFORE the retirement
    /// may still carry the now-unknown `progress_mode` key — `RuntimeConfig`
    /// has no `#[serde(deny_unknown_fields)]`, so serde silently drops unknown
    /// keys on deserialize, but this pins that contract explicitly rather than
    /// relying on it staying true by accident (a future `deny_unknown_fields`
    /// addition elsewhere in the struct would silently break old configs).
    #[test]
    #[serial(runtime_config)]
    fn reload_tolerates_retired_progress_mode_key_2549() {
        let dir = std::env::temp_dir().join(format!(
            "agend-test-runtime-config-2549-retired-key-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("runtime-config.json");
        std::fs::write(
            &path,
            r#"{"dev_idle_threshold_secs": 1234, "progress_mode": 1, "schema_version": 1}"#,
        )
        .unwrap();

        reload(&dir);

        let config = get();
        assert_eq!(
            config.dev_idle_threshold_secs, 1234,
            "fields that still exist must load normally"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial(runtime_config)]
    fn set_fails_closed_when_lock_unavailable_audit2_012() {
        // `home` is a regular FILE, so `home.join("runtime-config.json.lock")`
        // can't be created (ENOTDIR) and the lock cannot be acquired. set() must
        // FAIL CLOSED (return Err), never proceed to write unlocked.
        let home_file = std::env::temp_dir().join(format!(
            "agend-test-runtime-config-2012-isfile-{}",
            std::process::id()
        ));
        std::fs::write(&home_file, b"x").unwrap();
        let res = set(&home_file, "copy_on_select", "on");
        assert!(
            res.is_err(),
            "set() must fail closed when the lock is unavailable, got: {res:?}"
        );
        std::fs::remove_file(&home_file).ok();
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

    /// #2325: copy-on-select (Mode B) is the DEFAULT, and the flag round-trips
    /// through set/reload/get_key — including the operator-facing `on`/`off`
    /// vocabulary, not just `true`/`false`.
    #[test]
    #[serial(runtime_config)]
    fn copy_on_select_default_on_and_toggleable_via_on_off() {
        assert!(
            RuntimeConfig::default().copy_on_select,
            "copy_on_select must default ON (Mode B = copy-on-select)"
        );
        let dir = std::env::temp_dir().join("agend-test-runtime-config-copysel");
        std::fs::create_dir_all(&dir).ok();
        set(&dir, "copy_on_select", "off").unwrap();
        reload(&dir);
        assert_eq!(get_key("copy_on_select").unwrap(), "false");
        set(&dir, "copy_on_select", "on").unwrap();
        reload(&dir);
        assert_eq!(get_key("copy_on_select").unwrap(), "true");
        // Garbage value rejected (not silently coerced).
        assert!(set(&dir, "copy_on_select", "maybe").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #2413 (A): `observed_badge` defaults ON (the operator sees observer-corrected
    /// badges by default) and toggles via on/off (and true/false/1/0), persisting
    /// across reload. Garbage rejected.
    #[test]
    #[serial(runtime_config)]
    fn observed_badge_default_on_and_toggleable_via_on_off() {
        assert!(
            RuntimeConfig::default().observed_badge,
            "observed_badge must default ON"
        );
        let dir = std::env::temp_dir().join("agend-test-runtime-config-obsbadge");
        std::fs::create_dir_all(&dir).ok();
        set(&dir, "observed_badge", "off").unwrap();
        reload(&dir);
        assert_eq!(get_key("observed_badge").unwrap(), "false");
        set(&dir, "observed_badge", "on").unwrap();
        reload(&dir);
        assert_eq!(get_key("observed_badge").unwrap(), "true");
        assert!(set(&dir, "observed_badge", "maybe").is_err());
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

    #[test]
    #[serial(runtime_config)]
    fn context_pcts_default_and_toggleable() {
        let c = RuntimeConfig::default();
        assert_eq!(c.context_alert_pct, 80.0);
        assert_eq!(c.context_handoff_pct, 85.0);
        assert_eq!(c.context_handoff_escalate_pct, 92.0);

        let dir = std::env::temp_dir().join("agend-test-runtime-config-ctxpct");
        std::fs::create_dir_all(&dir).ok();

        // Test valid setting
        set(&dir, "context_alert_pct", "60").unwrap();
        set(&dir, "context_handoff_pct", "65.5").unwrap();
        set(&dir, "context_handoff_escalate_pct", "70.2").unwrap();
        reload(&dir);
        assert_eq!(get_key("context_alert_pct").unwrap(), "60");
        assert_eq!(get_key("context_handoff_pct").unwrap(), "65.5");
        assert_eq!(get_key("context_handoff_escalate_pct").unwrap(), "70.2");

        // Test invalid setting (setting a value that breaks order or bounds must return Err)
        assert!(set(&dir, "context_alert_pct", "80.0").is_err()); // alert 80.0 >= handoff 65.5
        assert!(set(&dir, "context_handoff_pct", "4.0").is_err()); // handoff <= 5.0 (hysteresis)
        assert!(set(&dir, "context_handoff_escalate_pct", "NaN").is_err()); // NaN is not finite

        // Test invalid disk configuration reload keeps last known good
        // Write invalid reload file directly
        std::fs::write(
            dir.join("runtime-config.json"),
            r#"{"schema_version": 1, "context_alert_pct": 95.0, "context_handoff_pct": 70.0, "context_handoff_escalate_pct": 80.0}"#,
        )
        .unwrap();
        reload(&dir);
        // Kept last good (60.0 / 65.5 / 70.2)
        assert_eq!(get_key("context_alert_pct").unwrap(), "60");
        assert_eq!(get_key("context_handoff_pct").unwrap(), "65.5");
        assert_eq!(get_key("context_handoff_escalate_pct").unwrap(), "70.2");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Pins the single-source-of-truth coupling: `validate_thresholds`' lower
    /// bound IS `HYSTERESIS_PCT` and is exclusive. A threshold exactly at the
    /// floor is rejected (the re-arm condition `pct < threshold - HYSTERESIS_PCT`
    /// would be impossible); a value just above it (with valid ordered
    /// handoff/escalate) is accepted. If someone re-tunes `HYSTERESIS_PCT`, the
    /// validate bound moves with it — they cannot drift apart.
    #[test]
    fn validate_lower_bound_is_hysteresis_pct_single_source() {
        // Exactly at the floor is out of the exclusive lower bound → Err.
        assert!(
            validate_thresholds(
                HYSTERESIS_PCT,
                HYSTERESIS_PCT + 10.0,
                HYSTERESIS_PCT + 20.0
            )
            .is_err(),
            "a threshold == HYSTERESIS_PCT must be rejected (exclusive lower bound)"
        );
        // Just above the floor, ordered handoff/escalate → Ok.
        assert!(
            validate_thresholds(
                HYSTERESIS_PCT + 0.1,
                HYSTERESIS_PCT + 10.0,
                HYSTERESIS_PCT + 20.0
            )
            .is_ok(),
            "a threshold just above HYSTERESIS_PCT (ordered) must be accepted"
        );
    }
}
