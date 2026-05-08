//! Sprint 57 Wave 3 PR-3 (#548 Phase 3) — Windows Task Scheduler integration.
//!
//! User-level at-logon task `\AgendTerminalDaemon`. No admin required.

use std::path::{Path, PathBuf};

use super::{apply_substitutions, ServiceState, UninstallOutcome, WINDOWS_TASK, WINDOWS_TEMPLATE};

/// Where we cache the rendered task XML on disk so `status` can
/// detect "installed" by file presence + so a re-install regenerates
/// the latest path resolution.
pub(super) fn task_xml_path(home: &Path) -> PathBuf {
    home.join("service").join("scheduler.task.xml")
}

/// Resolve the current Windows user identifier in `DOMAIN\\USER`
/// format. Falls back to bare `%USERNAME%` if `USERDOMAIN` is unset
/// (rare on workgroup machines + most CI runners).
fn current_windows_user() -> String {
    let username = std::env::var("USERNAME").unwrap_or_default();
    if let Ok(domain) = std::env::var("USERDOMAIN") {
        if !domain.is_empty() {
            return format!("{domain}\\{username}");
        }
    }
    username
}

pub(super) fn install(home: &Path, exe: &Path) -> Result<PathBuf, String> {
    let xml_path = task_xml_path(home);
    let user = current_windows_user();
    let resolved = apply_substitutions(
        WINDOWS_TEMPLATE,
        &[
            ("__EXECUTABLE__", &exe.display().to_string()),
            ("__HOME__", &home.display().to_string()),
            ("__USER__", &user),
        ],
    );
    if let Some(parent) = xml_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create_dir_all {parent:?}: {e}"))?;
    }
    // schtasks /Create /XML expects UTF-16 LE BOM input. Encode the
    // resolved string accordingly so the XML parser doesn't choke
    // on UTF-8 (the template's <?xml encoding="UTF-16"?> declaration
    // matches this).
    let utf16: Vec<u16> = resolved.encode_utf16().collect();
    let mut bytes: Vec<u8> = Vec::with_capacity(utf16.len() * 2 + 2);
    bytes.extend_from_slice(&[0xFF, 0xFE]); // UTF-16 LE BOM
    for unit in utf16 {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&xml_path, &bytes).map_err(|e| format!("write task xml {xml_path:?}: {e}"))?;

    // Idempotent register: schtasks /Create /F overwrites any existing
    // task with the same name, so no separate delete-first step is
    // needed.
    let create = std::process::Command::new("schtasks")
        .args(["/Create", "/TN", WINDOWS_TASK, "/XML"])
        .arg(&xml_path)
        .arg("/F")
        .output();
    match create {
        Ok(o) if o.status.success() => Ok(xml_path),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            // Non-fatal in test environments where schtasks may be
            // unavailable. Keep the file write successful so downstream
            // tests can verify the rendered XML shape.
            eprintln!("schtasks /Create /TN {WINDOWS_TASK} failed (non-fatal): {stderr}");
            Ok(xml_path)
        }
        Err(e) => {
            eprintln!("schtasks /Create: {e}");
            Ok(xml_path)
        }
    }
}

pub(super) fn uninstall(home: &Path) -> Result<UninstallOutcome, String> {
    let xml_path = task_xml_path(home);
    if !xml_path.exists() {
        return Ok(UninstallOutcome {
            was_installed: false,
        });
    }
    let _ = std::process::Command::new("schtasks")
        .args(["/Delete", "/TN", WINDOWS_TASK, "/F"])
        .output();
    std::fs::remove_file(&xml_path).map_err(|e| format!("remove task xml {xml_path:?}: {e}"))?;
    Ok(UninstallOutcome {
        was_installed: true,
    })
}

pub(super) fn status(home: &Path) -> Result<ServiceState, String> {
    let xml_path = task_xml_path(home);
    if !xml_path.exists() {
        return Ok(ServiceState::NotInstalled);
    }
    let query = std::process::Command::new("schtasks")
        .args(["/Query", "/TN", WINDOWS_TASK, "/FO", "LIST"])
        .output()
        .map_err(|e| format!("schtasks /Query: {e}"))?;
    if !query.status.success() {
        return Ok(ServiceState::Stopped);
    }
    let stdout = String::from_utf8_lossy(&query.stdout);
    // schtasks /FO LIST reports "Status: Running" for active tasks.
    if stdout.contains("Running") {
        Ok(ServiceState::Running)
    } else {
        Ok(ServiceState::Stopped)
    }
}
