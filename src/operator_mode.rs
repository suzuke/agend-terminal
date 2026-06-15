//! #1339: Operator Mode — fleet-global runtime authority state.
//!
//! `~/.agend-terminal/operator-mode.json` answers *"is the human operator
//! available, and what authority is delegated?"* — consumed by the single API
//! ingress gate (`api::check_operation_allowed`). Reloaded each daemon tick
//! (like [`crate::runtime_config`]), so a mode change propagates fleet-wide
//! without a restart.
//!
//! Distinct from #1563 `idle_expectation` (per-agent fleet.yaml *static* config,
//! "is THIS agent expected to be quiet?").
//!
//! #1576/#1654 — FAIL-CLOSED authority gate (single-user injection-containment;
//! see [`crate::config_integrity`]). The file is trusted only if it carries a
//! valid HMAC, established solely by an explicit operator `set_mode`. Any
//! unsigned/tampered/missing state resolves to restrictive ([`OperatorMode::Away`])
//! at startup (keep-last-good at runtime), NEVER fail-open to `Active`. So a
//! fresh install — and a post-upgrade legacy file — start restrictive until the
//! operator activates a mode once: the correct default for an authority gate.

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
/// FAIL-CLOSED authority gate: the on-disk file is trusted ONLY if it carries a
/// valid HMAC signature, which is established ONLY by an explicit operator
/// activation (`set_mode` through the daemon). Anything else — missing file,
/// corrupt JSON, missing/bad sidecar, missing key, or an unsigned legacy file —
/// is untrusted and NEVER migrate-trusted into `Active`.
///
/// This closes the #1654/codex bypass: because the agent runs same-uid it can
/// delete the key + sidecar, so "a key exists" can't be a trust discriminator
/// and "no-key ⇒ fresh ⇒ trust Active" was a hole. Now an agent that `rm`s the
/// key + sidecar and writes `{"mode":"active"}` lands in the untrusted path →
/// the daemon refuses to bless it.
///
/// Disposition for untrusted: at STARTUP (no last-known-good yet) lock down to
/// restrictive (`Away`); at RUNTIME keep the last-known-good already in
/// `global()`. Either way, alert. CONSEQUENCE (documented): a genuine fresh
/// install AND a post-upgrade legacy install both start restrictive until the
/// operator activates operator-mode once — the correct fail-closed default for
/// an authority gate.
pub fn reload(home: &Path) {
    let is_startup = !INITIALIZED.swap(true, Ordering::SeqCst);
    match load_verified(home) {
        Loaded::Trusted(state) => {
            *global().write() = state;
            TAMPER_ALERTED.store(false, Ordering::Relaxed);
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
    Untrusted(&'static str),
}

/// Trust the file ONLY on valid-JSON + valid-HMAC. Every other case is
/// untrusted — no migrate-signing, no "key exists" discriminator (the agent can
/// delete the key), no fail-open. The key + sidecar are created solely by
/// `set_mode` (operator activation), so this is the only path to a trusted
/// non-restrictive mode.
fn load_verified(home: &Path) -> Loaded {
    let content = match std::fs::read(path(home)) {
        Ok(c) => c,
        Err(_) => return Loaded::Untrusted("operator-mode.json missing (not yet activated?)"),
    };
    if serde_json::from_slice::<OperatorModeState>(&content).is_err() {
        return Loaded::Untrusted("operator-mode.json is not valid JSON");
    }
    let Ok(tag) = std::fs::read_to_string(sidecar_path(home)) else {
        return Loaded::Untrusted("operator-mode.json has no HMAC sidecar (unsigned/tampered)");
    };
    if !crate::config_integrity::verify(home, &content, &tag) {
        return Loaded::Untrusted("operator-mode.json HMAC mismatch");
    }
    // Re-parse the now-verified bytes for the state to return.
    match serde_json::from_slice(&content) {
        Ok(state) => Loaded::Trusted(state),
        Err(_) => Loaded::Untrusted("operator-mode.json is not valid JSON"),
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
    crate::store::atomic_write(&path(home), json.as_bytes()).map_err(|e| e.to_string())?;
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

    // #1576/#1654: fail-closed authority default. A fresh install (no signed
    // file) starts RESTRICTIVE (Away) until the operator activates a mode once
    // — NOT the old fail-open Active. This is the correct default for an
    // authority gate and closes the delete-key+sidecar bypass (no-signature can
    // never mean "trust Active").
    #[test]
    #[serial]
    fn fresh_install_locks_down_until_activated() {
        let home = tmp("default");
        reset_fresh_process();
        reload(&home); // no operator-mode.json / no signature yet
        assert_eq!(
            get().mode,
            OperatorMode::Away,
            "an unsigned fresh install must fail CLOSED (Away), not open (Active)"
        );
        assert!(get().delegate_to.is_none());
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
        // Operator legitimately activates Sleep (writes a valid HMAC sidecar).
        set_mode(
            &home,
            OperatorMode::Sleep,
            Some("lead".into()),
            vec!["send".into()],
        )
        .expect("set_mode");
        // Injected agent blind-overwrites the JSON to disable the gate, but
        // can't forge the HMAC (doesn't know the key) → sidecar now stale.
        std::fs::write(path(&home), br#"{"mode":"active"}"#).unwrap();
        reset_fresh_process();
        reload(&home);
        assert_eq!(
            get().mode,
            OperatorMode::Away,
            "blind-overwritten (bad-HMAC) file must be rejected → lockdown (Away), not Active or the prior Sleep"
        );
    }

    // #1654 (codex bypass): an injected agent runs same-uid, so it can DELETE
    // both the key and the sidecar, then write `{"mode":"active"}`. The old code
    // read no-key+no-sidecar as "legacy/fresh → migrate-trust", blessing the
    // forgery. Fail-closed: no valid signature → Away, never the forged Active.
    #[test]
    #[serial]
    fn delete_key_and_sidecar_then_forge_active_locks_down() {
        let home = tmp("codex-bypass");
        // Operator had activated Sleep (key + sidecar exist).
        set_mode(&home, OperatorMode::Sleep, None, vec![]).expect("set_mode");
        // Attack: rm the key + sidecar, write a forged Active with no signature.
        std::fs::remove_file(home.join(".config-integrity-key")).unwrap();
        std::fs::remove_file(sidecar_path(&home)).unwrap();
        std::fs::write(path(&home), br#"{"mode":"active"}"#).unwrap();
        reset_fresh_process();
        reload(&home);
        assert_eq!(
            get().mode,
            OperatorMode::Away,
            "delete-key+sidecar+forge-Active must NOT be migrate-trusted — fail closed to Away"
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

    // #1654: a legacy pre-#1576 file (valid JSON, NO sidecar) is NOT
    // migrate-trusted (that was the bypass). It locks down until the operator
    // re-activates once — accepted upgrade cost for an authority gate.
    #[test]
    #[serial]
    fn legacy_unsigned_file_locks_down() {
        let home = tmp("legacy");
        std::fs::write(path(&home), br#"{"mode":"sleep","delegate_to":"lead"}"#).unwrap();
        reset_fresh_process();
        reload(&home);
        assert_eq!(
            get().mode,
            OperatorMode::Away,
            "an unsigned legacy file must lock down, not be blessed into its on-disk mode"
        );
    }

    #[test]
    #[serial]
    fn deleted_file_after_signing_locks_down() {
        let home = tmp("deleted");
        set_mode(&home, OperatorMode::Sleep, None, vec![]).expect("set_mode");
        // Agent deletes the file to revert the gate. (Key may or may not remain
        // — irrelevant now: missing file = no signature = untrusted.)
        std::fs::remove_file(path(&home)).unwrap();
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
