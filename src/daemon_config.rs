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
    /// Daemon PID (replaces AGEND_DAEMON_PID env var).
    pub daemon_pid: u32,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            pointer_only_inject: std::env::var("AGEND_POINTER_ONLY_INJECT")
                .ok()
                .map(|v| v == "1")
                .unwrap_or(false),
            daemon_pid: std::process::id(),
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
        assert_eq!(cfg.daemon_pid, std::process::id());
        // pointer_only_inject depends on env var; in test context should be false
        // unless explicitly set
    }

    #[test]
    fn daemon_config_with_overrides() {
        let cfg = DaemonConfig {
            pointer_only_inject: true,
            daemon_pid: 42,
        };
        assert!(cfg.pointer_only_inject);
        assert_eq!(cfg.daemon_pid, 42);
    }
}
