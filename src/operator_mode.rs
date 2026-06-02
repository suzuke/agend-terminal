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
use std::sync::atomic::{AtomicBool, Ordering};
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

/// #1576: false until the first `reload()`, to tell startup (no last-known-good
/// yet → an untrusted file means LOCK DOWN) apart from a running daemon (has a
/// last-good in `global()` → an untrusted file means KEEP it).
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// #1576: de-dupes the tamper alert so a persistently-untrusted file doesn't log
/// every 10s tick. Cleared on the next clean (trusted) load.
static TAMPER_ALERTED: AtomicBool = AtomicBool::new(false);

fn global() -> &'static RwLock<OperatorModeState> {
    OPERATOR_MODE.get_or_init(|| RwLock::new(OperatorModeState::default()))
}

fn path(home: &Path) -> PathBuf {
    home.join("operator-mode.json")
}

/// #1576: hex HMAC sidecar next to `operator-mode.json`.
fn sidecar_path(home: &Path) -> PathBuf {
    home.join("operator-mode.json.hmac")
}

/// #1576: the safe-restrictive lockdown state for a present-but-untrusted file
/// at startup — gate ENABLED, NO delegation. The opposite of the old
/// fail-open→Active default, so a tampered authority file locks down, not open.
fn lockdown() -> OperatorModeState {
    OperatorModeState {
        mode: OperatorMode::Away,
        delegate_to: None,
        delegate_scope: Vec::new(),
    }
}

/// Snapshot the current operator-mode state (lock-free clone).
pub fn get() -> OperatorModeState {
    global().read().clone()
}

/// Reload from disk. Called each daemon tick (reload-coherent).
///
/// #1576 — single-user injection-containment (see [`crate::config_integrity`]).
/// The file is trusted only if it carries a valid HMAC sidecar. Disposition:
/// - trusted (valid HMAC, or a legacy pre-#1576 file that we migrate-sign) → load it;
/// - fresh install (no file, never signed) → today's default (`Active`), zero-migration;
/// - untrusted (corrupt / bad-or-missing HMAC / deleted after signing) → FAIL CLOSED,
///   never fail-open→`Active`: at startup lock down to restrictive (`Away`); at
///   runtime keep the last-known-good already in `global()`. Either way, alert.
pub fn reload(home: &Path) {
    let is_startup = !INITIALIZED.swap(true, Ordering::SeqCst);
    match load_verified(home) {
        Loaded::Trusted(state) => {
            *global().write() = state;
            TAMPER_ALERTED.store(false, Ordering::Relaxed);
        }
        Loaded::FreshInstall => {
            *global().write() = OperatorModeState::default();
        }
        Loaded::Untrusted(reason) => {
            if is_startup {
                *global().write() = lockdown();
            }
            // runtime: leave global() untouched — keep last-known-good.
            alert_tamper(reason, is_startup);
        }
    }
}

enum Loaded {
    Trusted(OperatorModeState),
    FreshInstall,
    Untrusted(&'static str),
}

fn load_verified(home: &Path) -> Loaded {
    let content = match std::fs::read(path(home)) {
        Ok(c) => c,
        Err(_) => {
            // Missing. A fresh install (never signed) is fine; a missing file
            // AFTER we've signed before means it was deleted to revert the gate.
            return if crate::config_integrity::key_exists(home) {
                Loaded::Untrusted(
                    "operator-mode.json missing but an integrity key exists (deleted?)",
                )
            } else {
                Loaded::FreshInstall
            };
        }
    };
    let state: OperatorModeState = match serde_json::from_slice(&content) {
        Ok(s) => s,
        Err(_) => return Loaded::Untrusted("operator-mode.json is not valid JSON"),
    };
    match std::fs::read_to_string(sidecar_path(home)) {
        Ok(tag) => {
            if crate::config_integrity::verify(home, &content, &tag) {
                Loaded::Trusted(state)
            } else {
                Loaded::Untrusted("operator-mode.json HMAC mismatch")
            }
        }
        Err(_) => {
            // No sidecar. Never-signed (no key) ⇒ legacy pre-#1576 file: accept
            // once and migrate-sign. Key present ⇒ the sidecar was removed ⇒ tamper.
            if crate::config_integrity::key_exists(home) {
                Loaded::Untrusted("operator-mode.json HMAC sidecar missing")
            } else {
                let _ = write_sidecar(home, &content);
                Loaded::Trusted(state)
            }
        }
    }
}

/// Sign `content` and write the hex HMAC sidecar. Best-effort: a failed sidecar
/// write surfaces on the next reload as "missing sidecar" (fail-closed), so a
/// silent drop here can never open the gate.
fn write_sidecar(home: &Path, content: &[u8]) -> std::io::Result<()> {
    let tag = crate::config_integrity::sign(home, content)?;
    std::fs::write(sidecar_path(home), tag)
}

fn alert_tamper(reason: &str, is_startup: bool) {
    if TAMPER_ALERTED.swap(true, Ordering::Relaxed) {
        return; // already alerted this episode — don't spam every 10s tick.
    }
    let disposition = if is_startup {
        "locked down to restrictive (Away) at startup"
    } else {
        "kept last-known-good mode"
    };
    tracing::error!(
        reason,
        disposition,
        "#1576: operator-mode.json failed its integrity check — a prompt-injected \
         agent may have tampered with the authority gate. Re-set the mode via the \
         operator CLI to clear."
    );
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
    std::fs::write(path(home), &json).map_err(|e| e.to_string())?;
    // #1576: sign so the next reload trusts only operator-written content.
    write_sidecar(home, json.as_bytes()).map_err(|e| e.to_string())?;
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

    /// Simulate a fresh daemon process: reset the process-global statics so the
    /// next `reload()` is treated as startup (no last-known-good yet). The
    /// per-`home` files (key + sidecar) are isolated per test by `tmp`.
    fn reset_fresh_process() {
        *global().write() = OperatorModeState::default();
        INITIALIZED.store(false, Ordering::SeqCst);
        TAMPER_ALERTED.store(false, Ordering::SeqCst);
    }

    #[test]
    #[serial]
    fn omitted_file_defaults_to_active_zero_migration() {
        let home = tmp("default");
        reset_fresh_process();
        reload(&home); // no operator-mode.json, never signed → fresh install
        assert_eq!(get().mode, OperatorMode::Active);
        assert!(get().delegate_to.is_none());
        assert!(get().delegate_scope.is_empty());
    }

    // #1576: REWRITTEN from the old `..falls_back_to_active_not_denyall` test,
    // which encoded the fail-OPEN footgun (corrupt authority file → Active =
    // gate disabled). The contract is now fail-CLOSED: a present-but-untrusted
    // file at startup must LOCK DOWN (restrictive), never open the gate.
    #[test]
    #[serial]
    fn corrupt_file_at_startup_locks_down_not_active() {
        let home = tmp("garbage");
        std::fs::write(path(&home), b"not json {{{").unwrap();
        reset_fresh_process();
        reload(&home);
        assert_eq!(
            get().mode,
            OperatorMode::Away,
            "corrupt authority file at startup must fail CLOSED (Away), not open (Active)"
        );
        assert!(get().delegate_to.is_none(), "lockdown grants no delegation");
    }

    #[test]
    #[serial]
    fn tampered_content_bad_hmac_at_startup_locks_down() {
        let home = tmp("tamper-startup");
        // Operator legitimately sets Away (writes a valid HMAC sidecar).
        set_mode(&home, OperatorMode::Away, None, vec![]).expect("set_mode");
        // Injected agent blind-overwrites the JSON to disable the gate, but
        // can't forge the HMAC (doesn't know the key) → sidecar now stale.
        std::fs::write(path(&home), br#"{"mode":"active"}"#).unwrap();
        reset_fresh_process();
        reload(&home);
        assert_eq!(
            get().mode,
            OperatorMode::Away,
            "blind-overwritten (bad-HMAC) file must be rejected → lockdown, not Active"
        );
    }

    #[test]
    #[serial]
    fn tampered_content_at_runtime_keeps_last_known_good() {
        let home = tmp("tamper-runtime");
        set_mode(
            &home,
            OperatorMode::Sleep,
            Some("lead".into()),
            vec!["send".into()],
        )
        .expect("set_mode");
        reset_fresh_process();
        reload(&home); // startup: loads the trusted Sleep state; INITIALIZED=true
        assert_eq!(get().mode, OperatorMode::Sleep);
        // Now a running daemon: agent blind-overwrites the file.
        std::fs::write(path(&home), br#"{"mode":"active"}"#).unwrap();
        reload(&home); // runtime tamper → KEEP last-known-good
        assert_eq!(
            get().mode,
            OperatorMode::Sleep,
            "runtime tamper must keep the last-known-good mode, not adopt the forged one"
        );
    }

    #[test]
    #[serial]
    fn valid_signed_file_loads_across_restart() {
        let home = tmp("signed-restart");
        set_mode(
            &home,
            OperatorMode::Sleep,
            Some("fixup-lead".into()),
            vec!["task_dispatch".into(), "pr_merge".into()],
        )
        .expect("set_mode");
        reset_fresh_process(); // simulate daemon restart reading the persisted file
        reload(&home);
        let s = get();
        assert_eq!(s.mode, OperatorMode::Sleep);
        assert_eq!(s.delegate_to.as_deref(), Some("fixup-lead"));
        assert_eq!(s.delegate_scope, vec!["task_dispatch", "pr_merge"]);
    }

    #[test]
    #[serial]
    fn legacy_unsigned_file_migrates_and_loads() {
        let home = tmp("legacy");
        // A pre-#1576 file: valid JSON, NO sidecar, NO key ever created.
        std::fs::write(path(&home), br#"{"mode":"sleep","delegate_to":"lead"}"#).unwrap();
        assert!(!crate::config_integrity::key_exists(&home));
        reset_fresh_process();
        reload(&home);
        assert_eq!(
            get().mode,
            OperatorMode::Sleep,
            "a legacy unsigned file is accepted once (upgrade must not lock out)"
        );
        assert!(
            sidecar_path(&home).exists(),
            "legacy file is migrate-signed so subsequent tamper is caught"
        );
    }

    #[test]
    #[serial]
    fn deleted_file_after_signing_locks_down() {
        let home = tmp("deleted");
        set_mode(&home, OperatorMode::Sleep, None, vec![]).expect("set_mode");
        // Agent deletes the file to revert the gate to Active. The key remains.
        std::fs::remove_file(path(&home)).unwrap();
        assert!(crate::config_integrity::key_exists(&home));
        reset_fresh_process();
        reload(&home);
        assert_eq!(
            get().mode,
            OperatorMode::Away,
            "deleting a previously-signed file must lock down, not revert to Active"
        );
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
