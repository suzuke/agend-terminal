//! Fleet protocol extraction and resolution.
//!
//! Two-layer fallback: binary-embedded default (always overwritten on startup)
//! lives in `AGEND_HOME/protocol/.default/`. User overrides go in the parent
//! `AGEND_HOME/protocol/` directory and are never touched by the daemon.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const FILENAME: &str = "FLEET-DEV-PROTOCOL.md";

/// Embedded default protocol (compile-time).
const DEFAULT_PROTOCOL: &str = include_str!("../docs/FLEET-DEV-PROTOCOL.md");

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProtocolIdentity {
    pub source_kind: String,
    pub path: PathBuf,
    pub content_sha256: String,
    pub embedded_sha256: String,
    pub build_sha: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProtocolStatus {
    pub state: String,
    pub source_kind: Option<String>,
    pub path: Option<PathBuf>,
    pub content_sha256: Option<String>,
    pub embedded_sha256: String,
    pub build_sha: String,
    pub error: Option<String>,
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn embedded_sha256() -> String {
    digest(DEFAULT_PROTOCOL.as_bytes())
}

fn build_sha() -> String {
    option_env!("AGEND_BUILD_SHA")
        .unwrap_or("unknown")
        .to_owned()
}

fn identity_from_bytes(source_kind: &str, path: &Path, bytes: &[u8]) -> Result<ProtocolIdentity> {
    if bytes.is_empty() {
        bail!("protocol artifact {} is empty", path.display());
    }
    std::str::from_utf8(bytes)
        .with_context(|| format!("protocol artifact {} is not valid UTF-8", path.display()))?;
    Ok(ProtocolIdentity {
        source_kind: source_kind.to_owned(),
        path: path.to_path_buf(),
        content_sha256: digest(bytes),
        embedded_sha256: embedded_sha256(),
        build_sha: build_sha(),
    })
}

fn read_identity(path: &Path, source_kind: &str) -> Result<ProtocolIdentity> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("stat protocol artifact {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "protocol artifact {} is not a regular file (symlinks and directories are refused)",
            path.display()
        );
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("read protocol artifact {}", path.display()))?;
    identity_from_bytes(source_kind, path, &bytes)
}

fn default_path(home: &Path) -> PathBuf {
    home.join("protocol").join(".default").join(FILENAME)
}

/// Extract embedded protocol to `AGEND_HOME/protocol/.default/`.
/// Always overwrites — `.default/` is daemon-owned. Replacement is atomic and
/// fallible, so a failed write cannot leave a partial protocol artifact.
pub fn extract_default(home: &Path) -> anyhow::Result<ProtocolIdentity> {
    let dir = home.join("protocol").join(".default");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create protocol directory {}", dir.display()))?;
    let path = dir.join(FILENAME);
    crate::store::atomic_write(&path, DEFAULT_PROTOCOL.as_bytes())
        .with_context(|| format!("atomically extract protocol to {}", path.display()))?;
    identity_from_bytes("default", &path, DEFAULT_PROTOCOL.as_bytes())
}

/// Resolve the artifact that may be relied on by a provisioned agent.
/// Explicit overrides are authoritative but must be structurally valid. The
/// daemon-owned default is repaired to the exact embedded bytes before it is
/// returned, including when an older complete artifact is present.
pub fn resolve_protocol(home: &Path) -> Result<ProtocolIdentity> {
    let override_path = home.join("protocol").join(FILENAME);
    match std::fs::symlink_metadata(&override_path) {
        Ok(_) => return read_identity(&override_path, "override"),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).with_context(|| format!("stat {}", override_path.display()));
        }
        Err(_) => {}
    }

    let default = default_path(home);
    match std::fs::symlink_metadata(&default) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => extract_default(home),
        Err(error) => Err(error).with_context(|| format!("stat {}", default.display())),
        Ok(metadata) if !metadata.file_type().is_file() => bail!(
            "protocol default {} is not a regular file (directories and symlinks are refused)",
            default.display()
        ),
        Ok(_) => {
            let bytes = std::fs::read(&default)
                .with_context(|| format!("read protocol default {}", default.display()))?;
            if bytes == DEFAULT_PROTOCOL.as_bytes() {
                identity_from_bytes("default", &default, &bytes)
            } else {
                extract_default(home)
            }
        }
    }
}

/// Return the best available protocol file path for legacy callers. New
/// reliance-sensitive callers use [`resolve_protocol`] so they can surface
/// the error rather than silently proceeding.
#[allow(dead_code)]
pub fn protocol_path(home: &Path) -> PathBuf {
    resolve_protocol(home)
        .map(|identity| identity.path)
        .unwrap_or_else(|_| default_path(home))
}

fn status_for_identity(identity: ProtocolIdentity, state: &str) -> ProtocolStatus {
    ProtocolStatus {
        state: state.to_owned(),
        source_kind: Some(identity.source_kind),
        path: Some(identity.path),
        content_sha256: Some(identity.content_sha256),
        embedded_sha256: identity.embedded_sha256,
        build_sha: identity.build_sha,
        error: None,
    }
}

fn invalid_status(state: &str, path: Option<PathBuf>, error: impl Into<String>) -> ProtocolStatus {
    ProtocolStatus {
        state: state.to_owned(),
        source_kind: None,
        path,
        content_sha256: None,
        embedded_sha256: embedded_sha256(),
        build_sha: build_sha(),
        error: Some(error.into()),
    }
}

/// Return human- and machine-readable protocol delivery state without
/// mutating the workspace. This deliberately ignores temporary siblings and
/// reports the exact named override/default artifact only.
pub fn status(home: &Path) -> ProtocolStatus {
    let override_path = home.join("protocol").join(FILENAME);
    match std::fs::symlink_metadata(&override_path) {
        Ok(_) => {
            return match read_identity(&override_path, "override") {
                Ok(identity) => status_for_identity(identity, "ready"),
                Err(error) => {
                    invalid_status("invalid_override", Some(override_path), error.to_string())
                }
            };
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return invalid_status("invalid_override", Some(override_path), error.to_string());
        }
        Err(_) => {}
    }

    let default = default_path(home);
    let metadata = match std::fs::symlink_metadata(&default) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ProtocolStatus {
                state: "absent".into(),
                source_kind: None,
                path: Some(default),
                content_sha256: None,
                embedded_sha256: embedded_sha256(),
                build_sha: build_sha(),
                error: None,
            };
        }
        Err(error) => {
            return invalid_status("invalid_default", Some(default), error.to_string());
        }
    };
    if !metadata.file_type().is_file() {
        return invalid_status(
            "invalid_default",
            Some(default),
            "artifact is not a regular file (directories and symlinks are refused)",
        );
    }
    match std::fs::read(&default) {
        Ok(bytes) if bytes.is_empty() => {
            invalid_status("invalid_default", Some(default), "artifact is empty")
        }
        Ok(bytes) => match identity_from_bytes("default", &default, &bytes) {
            Ok(identity) if identity.content_sha256 == identity.embedded_sha256 => {
                status_for_identity(identity, "ready")
            }
            Ok(identity) => status_for_identity(identity, "degraded_serviceable"),
            Err(error) => invalid_status("invalid_default", Some(default), error.to_string()),
        },
        Err(error) => invalid_status("invalid_default", Some(default), error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-protocol-test-{}-{}-{}",
            std::process::id(),
            tag,
            id,
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn extract_default_creates_file() {
        let home = tmp_home("extract");
        extract_default(&home).expect("extract");
        let path = home.join("protocol/.default").join(FILENAME);
        assert!(path.exists(), ".default/ file must exist after extract");
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(
            content.contains("Fleet Development Protocol"),
            "extracted content must match embedded protocol"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn override_wins_over_default() {
        let home = tmp_home("override");
        extract_default(&home).expect("extract");
        let override_dir = home.join("protocol");
        std::fs::write(override_dir.join(FILENAME), "custom protocol").expect("write override");
        let path = protocol_path(&home);
        assert_eq!(path, override_dir.join(FILENAME));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn missing_override_falls_back_to_default() {
        let home = tmp_home("fallback");
        extract_default(&home).expect("extract");
        let path = protocol_path(&home);
        assert_eq!(
            path,
            home.join("protocol/.default").join(FILENAME),
            "must fall back to .default/ when no override"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn missing_both_extracts_and_returns() {
        let home = tmp_home("empty");
        // Neither exists — protocol_path should extract and return .default/
        let path = protocol_path(&home);
        assert_eq!(path, home.join("protocol/.default").join(FILENAME));
        assert!(path.exists(), "must auto-extract when both missing");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn stale_default_is_healed_before_selection() {
        let home = tmp_home("stale");
        let default_path = home.join("protocol/.default").join(FILENAME);
        std::fs::create_dir_all(default_path.parent().expect("default parent")).unwrap();
        std::fs::write(&default_path, "historically stale protocol").unwrap();

        let selected = protocol_path(&home);

        assert_eq!(selected, default_path);
        assert_eq!(
            std::fs::read_to_string(&selected).unwrap(),
            DEFAULT_PROTOCOL,
            "a stale daemon-owned default must self-heal before reliance"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn structural_override_artifacts_are_not_selected() {
        let home = tmp_home("override-structural");
        extract_default(&home).expect("extract");
        let override_path = home.join("protocol").join(FILENAME);
        std::fs::create_dir(&override_path).unwrap();

        let selected = protocol_path(&home);

        assert_ne!(
            selected, override_path,
            "a directory must never become the effective protocol artifact"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_override_is_not_selected() {
        let home = tmp_home("override-symlink");
        extract_default(&home).expect("extract");
        let override_path = home.join("protocol").join(FILENAME);
        let target = home.join("outside-protocol.md");
        std::fs::write(&target, "outside").unwrap();
        std::os::unix::fs::symlink(&target, &override_path).unwrap();

        let selected = protocol_path(&home);

        assert_ne!(
            selected, override_path,
            "a symlink must not provide ambiguous override provenance"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn failed_atomic_replacement_preserves_complete_default() {
        let home = tmp_home("atomic-failure");
        extract_default(&home).expect("extract");
        let default_path = home.join("protocol/.default").join(FILENAME);
        let before = std::fs::read(&default_path).unwrap();
        crate::store::fail_next_atomic_write_for_test(&default_path);

        let _ = extract_default(&home);

        assert_eq!(
            std::fs::read(&default_path).unwrap(),
            before,
            "failed replacement must preserve the prior complete artifact"
        );
        assert!(
            crate::store::atomic_write(&default_path, b"probe").is_ok(),
            "extract_default must consume the deterministic failure seam"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
