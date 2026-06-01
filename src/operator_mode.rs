//! #1339: Operator Mode — fleet-global runtime authority state.
//!
//! `~/.agend-terminal/operator-mode.json` answers *"is the human operator
//! available, and what authority is delegated?"* — consumed by the single API
//! ingress gate (`api::check_operation_allowed`). Reloaded each daemon tick
//! (like [`crate::runtime_config`]), so a mode change propagates fleet-wide
//! without a restart.
//!
//! Distinct from #1563 `idle_expectation` (per-agent fleet.yaml *static* config,
//! "is THIS agent expected to be quiet?"). They share only the
//! `#[serde(default)]` zero-migration discipline: an absent/empty file →
//! [`OperatorMode::Active`] = today's all-allowed behavior, so deployments that
//! never set a mode are unaffected.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Operator availability / authority mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperatorMode {
    /// Operator at the TUI — full authority; today's behavior (the default).
    #[default]
    Active,
    /// Operator reachable via Telegram but not at the TUI — structural ops
    /// blocked, NO delegation (the operator still decides, via TG).
    Away,
    /// Operator unreachable — a named delegate may proxy operations within
    /// `delegate_scope`; the never-delegate set stays blocked regardless.
    Sleep,
}

/// Fleet-global operator-mode state (mode + delegation).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorModeState {
    #[serde(default)]
    pub mode: OperatorMode,
    /// In `Sleep`, the instance granted proxy authority. `None` ⇒ no delegate
    /// (so every operator-requiring op is denied/queued, i.e. `Away`-like).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegate_to: Option<String>,
    /// Operation classes the delegate may proxy. **Deny-by-default**: anything
    /// not listed here is denied even in `Sleep`. The never-delegate set is
    /// blocked even if listed here (the gate hard-codes that, not this field).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delegate_scope: Vec<String>,
}

static OPERATOR_MODE: OnceLock<RwLock<OperatorModeState>> = OnceLock::new();

fn global() -> &'static RwLock<OperatorModeState> {
    OPERATOR_MODE.get_or_init(|| RwLock::new(OperatorModeState::default()))
}

fn path(home: &Path) -> PathBuf {
    home.join("operator-mode.json")
}

/// Snapshot the current operator-mode state (lock-free clone).
pub fn get() -> OperatorModeState {
    global().read().clone()
}

/// Reload from disk. Called each daemon tick (reload-coherent). A missing or
/// unparseable file → default (`Active`) — fail-safe to today's behavior, never
/// a deny-all that could lock the fleet.
pub fn reload(home: &Path) {
    let state = std::fs::read_to_string(path(home))
        .ok()
        .and_then(|c| serde_json::from_str::<OperatorModeState>(&c).ok())
        .unwrap_or_default();
    *global().write() = state;
}

/// Set the mode (+ optional delegate) and persist atomically (disk + memory).
/// Typed — unlike the flat `runtime_config` string setter — because
/// `delegate_scope` is a structured list a `set(key, value)` API can't carry.
pub fn set_mode(
    home: &Path,
    mode: OperatorMode,
    delegate_to: Option<String>,
    delegate_scope: Vec<String>,
) -> Result<OperatorModeState, String> {
    let state = OperatorModeState {
        mode,
        delegate_to,
        delegate_scope,
    };
    let json = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
    std::fs::write(path(home), json).map_err(|e| e.to_string())?;
    *global().write() = state.clone();
    Ok(state)
}

/// Parse a mode string (for the MCP `mode` tool). Case-insensitive.
pub fn parse_mode(s: &str) -> Result<OperatorMode, String> {
    match s.to_ascii_lowercase().as_str() {
        "active" => Ok(OperatorMode::Active),
        "away" => Ok(OperatorMode::Away),
        "sleep" => Ok(OperatorMode::Sleep),
        other => Err(format!(
            "unknown mode: {other} (expected active|away|sleep)"
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("agend-opmode-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    #[serial]
    fn omitted_file_defaults_to_active_zero_migration() {
        let home = tmp("default");
        reload(&home); // no operator-mode.json present
        assert_eq!(get().mode, OperatorMode::Active);
        assert!(get().delegate_to.is_none());
        assert!(get().delegate_scope.is_empty());
    }

    #[test]
    #[serial]
    fn unparseable_file_falls_back_to_active_not_denyall() {
        let home = tmp("garbage");
        std::fs::write(path(&home), b"not json {{{").unwrap();
        reload(&home);
        assert_eq!(get().mode, OperatorMode::Active, "fail-safe to Active");
    }

    #[test]
    #[serial]
    fn set_mode_then_reload_is_coherent() {
        let home = tmp("reload-coherence");
        set_mode(
            &home,
            OperatorMode::Sleep,
            Some("fixup-lead".into()),
            vec!["task_dispatch".into(), "pr_merge".into()],
        )
        .expect("set_mode");
        // Simulate a fresh process / next-tick reload reading the persisted file.
        *global().write() = OperatorModeState::default();
        reload(&home);
        let s = get();
        assert_eq!(s.mode, OperatorMode::Sleep);
        assert_eq!(s.delegate_to.as_deref(), Some("fixup-lead"));
        assert_eq!(s.delegate_scope, vec!["task_dispatch", "pr_merge"]);
    }

    #[test]
    fn mode_serializes_lowercase() {
        let s = OperatorModeState {
            mode: OperatorMode::Sleep,
            delegate_to: Some("lead".into()),
            delegate_scope: vec!["send".into()],
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"sleep\""), "mode is lowercase: {json}");
        // Round-trips.
        let back: OperatorModeState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn parse_mode_case_insensitive_and_rejects_unknown() {
        assert_eq!(parse_mode("Active").unwrap(), OperatorMode::Active);
        assert_eq!(parse_mode("SLEEP").unwrap(), OperatorMode::Sleep);
        assert!(parse_mode("dnd").is_err(), "dnd is excluded from MVP");
    }
}
