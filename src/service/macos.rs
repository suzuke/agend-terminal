//! Sprint 57 Wave 3 PR-3 (#548 Phase 3) — macOS launchd integration.
//!
//! User-level LaunchAgent at
//! `~/Library/LaunchAgents/com.agend-terminal.daemon.plist`.
//! No admin required.

use std::path::{Path, PathBuf};

use super::{
    apply_substitutions, xml_escape, ServiceState, UninstallOutcome, LAUNCHD_TEMPLATE,
    SERVICE_LABEL,
};

/// Resolve the absolute path to the LaunchAgent plist for the
/// current user. `~/Library/LaunchAgents/com.agend-terminal.daemon.plist`.
pub(super) fn plist_path() -> Result<PathBuf, String> {
    let home_dir = std::env::var("HOME")
        .map_err(|_| "HOME env var not set; cannot resolve LaunchAgents path".to_string())?;
    Ok(PathBuf::from(home_dir)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{SERVICE_LABEL}.plist")))
}

pub(super) fn install(home: &Path, exe: &Path) -> Result<PathBuf, String> {
    let plist = plist_path()?;
    let log_path = home.join("daemon.log");
    // Sprint 57 Wave 3 PR-3 r2 (Tier-2 Pass 2 fixup): XML-escape every
    // substituted value before splicing into the plist template.
    // Paths containing `&` (e.g. macOS network shares) or `<`, `>`,
    // `"`, `'` (rare but legal in user paths) would otherwise produce
    // malformed plist that `launchctl load -w` rejects with cryptic
    // errors. Pure entity-escape; safe even when no special chars.
    let exe_escaped = xml_escape(&exe.display().to_string());
    let home_escaped = xml_escape(&home.display().to_string());
    let log_escaped = xml_escape(&log_path.display().to_string());
    let label_escaped = xml_escape(SERVICE_LABEL);
    let resolved = apply_substitutions(
        LAUNCHD_TEMPLATE,
        &[
            ("__LABEL__", label_escaped.as_str()),
            ("__EXECUTABLE__", exe_escaped.as_str()),
            ("__HOME__", home_escaped.as_str()),
            ("__LOG__", log_escaped.as_str()),
        ],
    );
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create_dir_all {parent:?}: {e}"))?;
    }
    std::fs::write(&plist, resolved.as_bytes())
        .map_err(|e| format!("write plist {plist:?}: {e}"))?;

    // Idempotent register: unload first (no-op if not loaded), then load.
    let _ = std::process::Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist)
        .output();
    let load = std::process::Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&plist)
        .output()
        .map_err(|e| format!("launchctl load: {e}"))?;
    if !load.status.success() {
        let stderr = String::from_utf8_lossy(&load.stderr);
        return Err(format!(
            "launchctl load failed: {} (stderr: {})",
            load.status,
            stderr.trim()
        ));
    }
    Ok(plist)
}

pub(super) fn uninstall(_home: &Path) -> Result<UninstallOutcome, String> {
    let plist = plist_path()?;
    if !plist.exists() {
        return Ok(UninstallOutcome {
            was_installed: false,
        });
    }
    // Best-effort unload (no-op if already unloaded).
    let _ = std::process::Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist)
        .output();
    std::fs::remove_file(&plist).map_err(|e| format!("remove plist {plist:?}: {e}"))?;
    Ok(UninstallOutcome {
        was_installed: true,
    })
}

pub(super) fn status(_home: &Path) -> Result<ServiceState, String> {
    let plist = plist_path()?;
    if !plist.exists() {
        return Ok(ServiceState::NotInstalled);
    }
    // `launchctl list <label>` returns 0 + dict-style output if the
    // service is loaded (whether or not the underlying process is
    // alive — launchd holds the slot). The "PID" key indicates the
    // running PID; absence means loaded-but-stopped.
    let out = std::process::Command::new("launchctl")
        .args(["list", SERVICE_LABEL])
        .output()
        .map_err(|e| format!("launchctl list: {e}"))?;
    if !out.status.success() {
        // Not loaded — but plist file exists. Treat as Stopped.
        return Ok(ServiceState::Stopped);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.contains("\"PID\" =") || stdout.contains("\"PID\"=") {
        Ok(ServiceState::Running)
    } else {
        Ok(ServiceState::Stopped)
    }
}
