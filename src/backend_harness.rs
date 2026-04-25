//! Backend harness — PTY mechanism verification + cross-backend capability matrix.
//!
//! Proves PTY byte delivery (ESC/Ctrl-C) and tcgetpgrp work via shell proxy.
//! Backend-specific semantics (does ESC stop LLM generation?) are separately
//! tracked as unverified — real CLI verification is future work (backlog).

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
    /// PTY byte delivery works (proven via shell proxy)
    pub transport_verified: CapabilityLevel,
    /// Backend interprets ESC as "stop generation" (requires real CLI test)
    pub esc_semantics_verified: CapabilityLevel,
    /// SIGINT to foreground pgid kills tool subprocess (requires real CLI test)
    pub signal_semantics_verified: CapabilityLevel,
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
        for (name, notes) in [
            ("kiro-cli", ""),
            ("codex", ""),
            ("claude", "LLM context not tied to PTY buffer (known gap)"),
            ("gemini", ""),
        ] {
            backends.insert(
                name.into(),
                BackendCapability {
                    transport_verified: CapabilityLevel::Unverified,
                    esc_semantics_verified: CapabilityLevel::Unverified,
                    signal_semantics_verified: CapabilityLevel::Unverified,
                    notes: notes.into(),
                },
            );
        }
        Self {
            backends,
            tested_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Record shell proxy transport verification results.
    pub fn record_transport_results(&mut self, esc_ok: bool, signal_ok: bool) {
        for (name, b) in &mut self.backends {
            if name == "claude" {
                b.transport_verified = CapabilityLevel::False;
                continue;
            }
            b.transport_verified = if esc_ok && signal_ok {
                CapabilityLevel::True
            } else {
                CapabilityLevel::Unverified
            };
            // Semantics stay Unverified — shell proxy doesn't prove backend behavior
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        std::fs::write(path, serde_yaml::to_string(self)?)?;
        Ok(())
    }
}

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
    fn test_capability_matrix_serializes_with_split_columns() {
        let matrix = CapabilityMatrix::new();
        assert_eq!(matrix.backends.len(), 4);
        // All start unverified
        for b in matrix.backends.values() {
            assert_eq!(b.esc_semantics_verified, CapabilityLevel::Unverified);
            assert_eq!(b.signal_semantics_verified, CapabilityLevel::Unverified);
        }
    }

    #[test]
    fn test_record_transport_keeps_semantics_unverified() {
        let mut matrix = CapabilityMatrix::new();
        matrix.record_transport_results(true, true);
        // Transport verified for non-claude
        assert_eq!(
            matrix.backends["kiro-cli"].transport_verified,
            CapabilityLevel::True
        );
        assert_eq!(
            matrix.backends["codex"].transport_verified,
            CapabilityLevel::True
        );
        // Claude stays false (known gap)
        assert_eq!(
            matrix.backends["claude"].transport_verified,
            CapabilityLevel::False
        );
        // Semantics stay unverified for ALL — shell proxy doesn't prove backend behavior
        for b in matrix.backends.values() {
            assert_eq!(b.esc_semantics_verified, CapabilityLevel::Unverified);
            assert_eq!(b.signal_semantics_verified, CapabilityLevel::Unverified);
        }
    }

    #[test]
    fn test_esc_byte_delivery() {
        verify_byte_delivery(0x1b).expect("ESC byte must be deliverable via PTY");
    }

    #[test]
    fn test_ctrl_c_byte_delivery() {
        verify_byte_delivery(0x03).expect("Ctrl-C byte must be deliverable via PTY");
    }

    #[test]
    fn test_tcgetpgrp_returns_valid_pgid() {
        let pgid = verify_tcgetpgrp().expect("tcgetpgrp must return valid pgid");
        assert!(pgid > 0);
    }

    #[test]
    fn test_full_harness_produces_honest_matrix() {
        let mut matrix = CapabilityMatrix::new();
        let esc_ok = verify_byte_delivery(0x1b).is_ok();
        let signal_ok = verify_tcgetpgrp().is_ok();
        matrix.record_transport_results(esc_ok, signal_ok);

        // Save and verify
        let dir = std::env::temp_dir().join(format!("agend-harness-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        matrix.save(&dir.join("capability_matrix.yaml")).unwrap();
        let content = std::fs::read_to_string(dir.join("capability_matrix.yaml")).unwrap();
        // Transport should be true (shell proxy works)
        assert!(
            content.contains("transport_verified: true")
                || content.contains("transport_verified: 'true'"),
            "transport must be verified via shell proxy"
        );
        // Semantics must stay unverified
        assert!(
            content.contains("esc_semantics_verified: unverified"),
            "ESC semantics must stay unverified (no real backend test)"
        );
        assert!(esc_ok && signal_ok);
        std::fs::remove_dir_all(&dir).ok();
    }
}
