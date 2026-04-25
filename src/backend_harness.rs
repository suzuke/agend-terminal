//! Backend harness — cross-backend capability matrix for ESC interrupt + PTY signal.
//!
//! Verifies byte delivery mechanism works per backend and documents
//! per-backend capability flags. Shell survival already proven in PR-S
//! spike (Linux/macOS); this harness captures cross-backend differences.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum CapabilityLevel {
    True,
    False,
    Partial,
    Unverified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct BackendCapability {
    pub supports_esc_interrupt: CapabilityLevel,
    pub supports_pty_signal_tool_kill: CapabilityLevel,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct CapabilityMatrix {
    pub backends: HashMap<String, BackendCapability>,
    pub tested_at: String,
}

#[allow(dead_code)]
impl CapabilityMatrix {
    pub fn new() -> Self {
        let mut backends = HashMap::new();
        for (name, esc, sig, notes) in [
            ("kiro-cli", CapabilityLevel::Unverified, CapabilityLevel::Unverified, ""),
            ("codex", CapabilityLevel::Unverified, CapabilityLevel::Unverified, ""),
            ("claude", CapabilityLevel::False, CapabilityLevel::False,
             "Claude Code LLM context not tied to PTY buffer (known gap t-20260424011906930464-7)"),
            ("gemini", CapabilityLevel::Unverified, CapabilityLevel::Unverified, ""),
        ] {
            backends.insert(name.into(), BackendCapability {
                supports_esc_interrupt: esc,
                supports_pty_signal_tool_kill: sig,
                notes: notes.into(),
            });
        }
        Self {
            backends,
            tested_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        std::fs::write(path, serde_yaml::to_string(self)?)?;
        Ok(())
    }
}

/// Verify a single byte can be written to a PTY without error.
/// This proves the delivery mechanism works — the byte reaches the terminal.
#[cfg(unix)]
#[allow(dead_code)]
pub fn verify_byte_delivery(byte: u8) -> anyhow::Result<()> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::Write;

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut child = pair.slave.spawn_command(CommandBuilder::new("/bin/sh"))?;
    drop(pair.slave);
    let mut writer = pair.master.take_writer()?;
    writer.write_all(&[byte])?;
    writer.flush()?;
    child.kill().ok();
    Ok(())
}

/// Verify tcgetpgrp returns a valid pgid from a PTY master.
#[cfg(unix)]
#[allow(dead_code)]
pub fn verify_tcgetpgrp() -> anyhow::Result<i32> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut child = pair.slave.spawn_command(CommandBuilder::new("/bin/sh"))?;
    drop(pair.slave);
    std::thread::sleep(std::time::Duration::from_millis(200));
    let fd = pair
        .master
        .as_raw_fd()
        .ok_or_else(|| anyhow::anyhow!("no raw fd"))?;
    let pgid = unsafe { libc::tcgetpgrp(fd) };
    child.kill().ok();
    if pgid > 0 {
        Ok(pgid)
    } else {
        Err(anyhow::anyhow!("tcgetpgrp returned {pgid}"))
    }
}

#[cfg(test)]
#[cfg(unix)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_capability_matrix_serializes() {
        let matrix = CapabilityMatrix::new();
        assert_eq!(matrix.backends.len(), 4);
        assert_eq!(
            matrix.backends["claude"].supports_esc_interrupt,
            CapabilityLevel::False
        );
        let yaml = serde_yaml::to_string(&matrix).unwrap();
        assert!(yaml.contains("kiro-cli"));
        assert!(yaml.contains("claude"));
    }

    #[test]
    fn test_esc_byte_delivery() {
        // ESC (0x1b) — used by CLI backends to stop LLM generation
        verify_byte_delivery(0x1b).expect("ESC byte must be deliverable via PTY");
    }

    #[test]
    fn test_ctrl_c_byte_delivery() {
        // Ctrl-C (0x03) — terminal interrupt signal
        verify_byte_delivery(0x03).expect("Ctrl-C byte must be deliverable via PTY");
    }

    #[test]
    fn test_tcgetpgrp_returns_valid_pgid() {
        let pgid = verify_tcgetpgrp().expect("tcgetpgrp must return valid pgid");
        assert!(pgid > 0);
    }

    #[test]
    fn test_capability_matrix_save_load() {
        let dir = std::env::temp_dir().join(format!("agend-harness-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("capability_matrix.yaml");

        let mut matrix = CapabilityMatrix::new();
        matrix
            .backends
            .get_mut("kiro-cli")
            .unwrap()
            .supports_esc_interrupt = CapabilityLevel::True;
        matrix.backends.get_mut("kiro-cli").unwrap().notes =
            "Verified: ESC stops generation".into();
        matrix.save(&path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("kiro-cli"));
        assert!(content.contains("true"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_full_harness_produces_matrix() {
        let mut matrix = CapabilityMatrix::new();

        let esc_ok = verify_byte_delivery(0x1b).is_ok();
        let ctrl_c_ok = verify_byte_delivery(0x03).is_ok();
        let pgid_ok = verify_tcgetpgrp().is_ok();

        // Record results — shell proxy proves PTY mechanism works
        // Real backend behavior inferred (same PTY write path)
        for name in ["kiro-cli", "codex", "gemini"] {
            if let Some(b) = matrix.backends.get_mut(name) {
                b.supports_esc_interrupt = if esc_ok {
                    CapabilityLevel::Partial
                } else {
                    CapabilityLevel::Unverified
                };
                b.supports_pty_signal_tool_kill = if pgid_ok {
                    CapabilityLevel::Partial
                } else {
                    CapabilityLevel::Unverified
                };
                b.notes = format!(
                    "Shell proxy: ESC={esc_ok}, Ctrl-C={ctrl_c_ok}, tcgetpgrp={pgid_ok}. \
                     Real CLI verification requires #[ignore] test."
                );
            }
        }

        let dir = std::env::temp_dir().join(format!("agend-harness-matrix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        matrix.save(&dir.join("capability_matrix.yaml")).unwrap();

        assert!(esc_ok, "ESC delivery must work");
        assert!(ctrl_c_ok, "Ctrl-C delivery must work");
        assert!(pgid_ok, "tcgetpgrp must work");
        std::fs::remove_dir_all(&dir).ok();
    }
}
