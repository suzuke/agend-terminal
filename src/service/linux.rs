//! Sprint 57 Wave 3 PR-3 (#548 Phase 3) — Linux systemd user integration.
//!
//! User-level systemd unit at
//! `~/.config/systemd/user/agend-terminal-daemon.service`.
//! No root / sudo required.

use std::path::{Path, PathBuf};

use super::{apply_substitutions, ServiceState, UninstallOutcome, SYSTEMD_TEMPLATE, SYSTEMD_UNIT};

/// `~/.config/systemd/user/agend-terminal-daemon.service`.
/// Honors `XDG_CONFIG_HOME` if set, otherwise `~/.config`.
pub(super) fn unit_path() -> Result<PathBuf, String> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        let home = std::env::var("HOME").map_err(|_| {
            "HOME env var not set; cannot resolve systemd user unit path".to_string()
        })?;
        PathBuf::from(home).join(".config")
    };
    Ok(base.join("systemd").join("user").join(SYSTEMD_UNIT))
}

pub(super) fn install(home: &Path, exe: &Path) -> Result<PathBuf, String> {
    let unit = unit_path()?;
    let resolved = apply_substitutions(
        SYSTEMD_TEMPLATE,
        &[
            ("__EXECUTABLE__", &exe.display().to_string()),
            ("__HOME__", &home.display().to_string()),
        ],
    );
    if let Some(parent) = unit.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create_dir_all {parent:?}: {e}"))?;
    }
    std::fs::write(&unit, resolved.as_bytes())
        .map_err(|e| format!("write systemd unit {unit:?}: {e}"))?;

    // Idempotent register: daemon-reload first (picks up our write),
    // then enable --now (which is itself idempotent on already-enabled).
    let reload = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();
    if let Err(e) = reload {
        // Non-fatal: systemctl may not be available in the runner;
        // surface the failure but keep going so callers in test
        // environments without systemd see the file written.
        eprintln!("systemctl --user daemon-reload failed: {e}");
    }
    let enable = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", SYSTEMD_UNIT])
        .output();
    match enable {
        Ok(o) if o.status.success() => Ok(unit),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            // Test environments without a systemd session bus return
            // a non-zero exit but still write the unit file. Keep
            // the install successful (file present + on-disk schema
            // valid) and surface the activation issue via warning.
            eprintln!("systemctl --user enable --now {SYSTEMD_UNIT} failed (non-fatal): {stderr}");
            Ok(unit)
        }
        Err(e) => {
            eprintln!("systemctl --user enable: {e}");
            Ok(unit)
        }
    }
}

pub(super) fn uninstall(_home: &Path) -> Result<UninstallOutcome, String> {
    let unit = unit_path()?;
    if !unit.exists() {
        return Ok(UninstallOutcome {
            was_installed: false,
        });
    }
    // Best-effort disable + stop (no-op if not active).
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", SYSTEMD_UNIT])
        .output();
    std::fs::remove_file(&unit).map_err(|e| format!("remove systemd unit {unit:?}: {e}"))?;
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();
    Ok(UninstallOutcome {
        was_installed: true,
    })
}

pub(super) fn status(_home: &Path) -> Result<ServiceState, String> {
    let unit = unit_path()?;
    if !unit.exists() {
        return Ok(ServiceState::NotInstalled);
    }
    // `is-active` exits 0 if active, non-zero otherwise.
    let active = std::process::Command::new("systemctl")
        .args(["--user", "is-active", SYSTEMD_UNIT])
        .output()
        .map_err(|e| format!("systemctl --user is-active: {e}"))?;
    if active.status.success() {
        Ok(ServiceState::Running)
    } else {
        Ok(ServiceState::Stopped)
    }
}
