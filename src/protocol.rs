//! Fleet protocol extraction and resolution.
//!
//! Two-layer fallback: binary-embedded default (always overwritten on startup)
//! lives in `AGEND_HOME/protocol/.default/`. User overrides go in the parent
//! `AGEND_HOME/protocol/` directory and are never touched by the daemon.

use std::path::{Path, PathBuf};

const FILENAME: &str = "FLEET-DEV-PROTOCOL-v1.md";

/// Embedded default protocol (compile-time).
const DEFAULT_PROTOCOL: &str = include_str!("../docs/FLEET-DEV-PROTOCOL-v1.md");

/// Extract embedded protocol to `AGEND_HOME/protocol/.default/`.
/// Always overwrites — `.default/` is daemon-owned.
pub fn extract_default(home: &Path) {
    let dir = home.join("protocol").join(".default");
    std::fs::create_dir_all(&dir).ok();
    let _ = std::fs::write(dir.join(FILENAME), DEFAULT_PROTOCOL);
}

/// Return the best available protocol file path.
/// Priority: override > extracted default. Extracts if neither exists.
pub fn protocol_path(home: &Path) -> PathBuf {
    let override_path = home.join("protocol").join(FILENAME);
    if override_path.exists() {
        return override_path;
    }
    let default_path = home.join("protocol").join(".default").join(FILENAME);
    if default_path.exists() {
        return default_path;
    }
    extract_default(home);
    default_path
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
        extract_default(&home);
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
        extract_default(&home);
        let override_dir = home.join("protocol");
        std::fs::write(override_dir.join(FILENAME), "custom protocol").expect("write override");
        let path = protocol_path(&home);
        assert_eq!(path, override_dir.join(FILENAME));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn missing_override_falls_back_to_default() {
        let home = tmp_home("fallback");
        extract_default(&home);
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
}
