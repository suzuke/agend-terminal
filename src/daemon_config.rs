//! Process-wide daemon configuration — replaces env var reads with explicit config.
//!
//! Sprint 45 G3: `std::env::set_var` is unsafe in multi-threaded contexts
//! (Rust std docs, recent nightly marks it `unsafe`). This module provides
//! a thread-safe config singleton that callers read instead of env vars.
//!
//! Lifecycle: `init()` once at daemon startup (before spawning threads),
//! then `get()` from anywhere. Env var fallback preserved for backward compat.

use std::sync::OnceLock;

/// Process-wide daemon configuration.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Feature flag: pointer-only inbox injection (replaces AGEND_POINTER_ONLY_INJECT env var).
    pub pointer_only_inject: bool,
    /// #1487: operator-facing IANA timezone for the `now=` timestamp injected
    /// into agent message headers. Loaded from fleet.yaml `display_timezone:` at
    /// boot (see `init_daemon_services`); reuses the same operator-tz concept as
    /// `display_time::format_local_short`. `None` → system local time.
    pub display_timezone: Option<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            pointer_only_inject: std::env::var("AGEND_POINTER_ONLY_INJECT")
                .ok()
                .map(|v| v == "1")
                .unwrap_or(false),
            display_timezone: None,
        }
    }
}

static CONFIG: OnceLock<DaemonConfig> = OnceLock::new();

/// Initialize the global config. Call once at daemon startup.
/// If not called, `get()` returns env-var-derived defaults.
pub fn init(config: DaemonConfig) {
    let _ = CONFIG.set(config);
}

/// Get the current config. Returns defaults if `init()` was never called.
pub fn get() -> DaemonConfig {
    CONFIG.get().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_config_default_returns_expected_values() {
        let cfg = DaemonConfig::default();
        // `display_timezone` defaults to None unconditionally.
        assert_eq!(
            cfg.display_timezone, None,
            "display_timezone default is None"
        );
        // `pointer_only_inject` is derived from AGEND_POINTER_ONLY_INJECT
        // (true only when the var is exactly "1"). Compute the expectation the
        // same way so the assertion is deterministic regardless of the ambient
        // env, while still pinning that `default()` actually honors the var —
        // the previous body asserted nothing at all.
        let expect_pointer = std::env::var("AGEND_POINTER_ONLY_INJECT")
            .map(|v| v == "1")
            .unwrap_or(false);
        assert_eq!(
            cfg.pointer_only_inject, expect_pointer,
            "pointer_only_inject default must reflect AGEND_POINTER_ONLY_INJECT"
        );
    }

    #[test]
    fn daemon_config_with_overrides() {
        let cfg = DaemonConfig {
            pointer_only_inject: true,
            display_timezone: Some("Europe/Paris".to_string()),
        };
        assert!(cfg.pointer_only_inject);
        assert_eq!(cfg.display_timezone.as_deref(), Some("Europe/Paris"));
    }

    /// #1487: display_timezone defaults to None (→ system local) when unset.
    #[test]
    fn daemon_config_display_timezone_defaults_to_none() {
        assert_eq!(DaemonConfig::default().display_timezone, None);
    }
}
