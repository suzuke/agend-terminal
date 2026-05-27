//! Legacy dedup-state directory management.
//!
//! #1316 removed the per-agent dedup ledger (fingerprint / dedup_count /
//! last_inject_at / input_text persistence). The `dedup-state/` directory
//! may still contain leftover `.json` and `.tmp` files from pre-#1316
//! daemon runs. This module retains:
//!
//! - `DEDUP_STATE_DIR` constant (referenced by GC path in `daemon/mod.rs`)
//! - `cleanup_tmp_orphans` — GC stale `*.tmp` orphans at daemon startup
//! - `DedupStateGcReport` — GC outcome struct

/// Sub-directory of `$AGEND_HOME` that holds legacy per-agent JSON files.
pub(crate) const DEDUP_STATE_DIR: &str = "dedup-state";

/// GC report — what `cleanup_tmp_orphans` found and acted on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DedupStateGcReport {
    pub candidates: usize,
    pub deleted: usize,
    pub preserved_recent: usize,
}

/// GC stale `*.tmp` / `*.json.tmp` orphans under `<home>/dedup-state/`.
/// Returns counts so callers can log/test the GC outcome.
pub fn cleanup_tmp_orphans(home: &std::path::Path, retention_secs: u64) -> DedupStateGcReport {
    let dedup_root = home.join(DEDUP_STATE_DIR);
    let mut report = DedupStateGcReport::default();
    let entries = match std::fs::read_dir(&dedup_root) {
        Ok(it) => it,
        Err(_) => return report,
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !name.ends_with(".tmp") {
            continue;
        }
        report.candidates += 1;
        let mtime = entry.metadata().and_then(|m| m.modified()).ok();
        let elapsed = mtime
            .and_then(|t| now.duration_since(t).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if elapsed < retention_secs {
            report.preserved_recent += 1;
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                report.deleted += 1;
                tracing::info!(file = %name, elapsed_secs = elapsed,
                    "dedup-state GC: removed orphan tmp file");
            }
            Err(e) => {
                tracing::warn!(file = %name, error = %e,
                    "dedup-state GC: removal failed, skipping");
            }
        }
    }
    report
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-dedup-state-{}-{}-{}",
            std::process::id(),
            tag,
            id,
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn cleanup_tmp_orphans_removes_stale_and_preserves_recent() {
        let home = tmp_home("gc-tmp");
        let dedup_dir = home.join(DEDUP_STATE_DIR);
        std::fs::create_dir_all(&dedup_dir).unwrap();

        // Create a stale .tmp file (mtime = now, but retention = 0 → everything stale)
        std::fs::write(dedup_dir.join("dev.json.tmp"), b"stale").unwrap();
        let report = cleanup_tmp_orphans(&home, 0);
        assert_eq!(report.candidates, 1);
        assert_eq!(report.deleted, 1);
        assert_eq!(report.preserved_recent, 0);

        // Non-.tmp files are ignored
        std::fs::write(dedup_dir.join("dev.json"), b"real").unwrap();
        let report2 = cleanup_tmp_orphans(&home, 0);
        assert_eq!(report2.candidates, 0);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_tmp_orphans_preserves_recent() {
        let home = tmp_home("gc-recent");
        let dedup_dir = home.join(DEDUP_STATE_DIR);
        std::fs::create_dir_all(&dedup_dir).unwrap();

        std::fs::write(dedup_dir.join("dev.json.tmp"), b"fresh").unwrap();
        // retention = 9999s → file is fresh
        let report = cleanup_tmp_orphans(&home, 9999);
        assert_eq!(report.candidates, 1);
        assert_eq!(report.preserved_recent, 1);
        assert_eq!(report.deleted, 0);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_tmp_orphans_missing_dir_returns_empty() {
        let home = tmp_home("gc-missing");
        let report = cleanup_tmp_orphans(&home, 0);
        assert_eq!(report, DedupStateGcReport::default());
    }
}
