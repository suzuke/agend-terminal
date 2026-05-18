//! #914 daemon log rotation.
//!
//! Provides:
//! - [`setup_daemon_tracing`] — install panic hook + rolling-file tracing
//!   subscriber. Called once from `main` when entering the daemon child path
//!   (`start --foreground`).
//! - [`migrate_existing_daemon_log`] — idempotent rename of any pre-rotation
//!   `daemon.log` left by old binaries into `daemon.log.migration.<epoch>`
//!   so the new rolling appender starts on a clean slate without losing
//!   operator history.
//! - [`cleanup_oversize_logs`] — hard backstop. Pruning the oldest
//!   `daemon.log.*` files until the directory's total log footprint is
//!   under `AGEND_LOG_MAX_BYTES`. Wired into the per-tick handler so it
//!   runs hourly regardless of `max_log_files`.
//! - [`update_daemon_log_symlink_unix`] — points `daemon.log` symlink at
//!   the newest rotated file so `tail -F daemon.log` keeps working post-rotation.
//!
//! Skeleton commit — function bodies are stubs that fail the RED tests in
//! the companion mod. The next commit (#914 C2) replaces them with the
//! real implementations.

// Each item is `dead_code` in this RED-tests commit; the wiring lands
// in C1/C2/C3. The blanket allow here keeps the test commit clippy-clean
// without inserting per-item `#[allow]` noise that the impl commits
// would then have to remove.
#![allow(dead_code)]

use std::path::Path;

/// Default daily retention when `AGEND_LOG_RETAIN_DAYS` is unset.
/// 3 files × ~800 MB worst-case per file ≈ 2.4 GB ceiling; the
/// `AGEND_LOG_MAX_BYTES` hard backstop catches the heavy-traffic case.
pub const DEFAULT_RETAIN_DAYS: usize = 3;

/// Hard directory-size backstop in bytes. Hourly cleanup tick prunes
/// oldest `daemon.log.*` until total under this cap.
pub const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// File-name prefix for the rolling appender. Suffix is `<date>` (and
/// `.migration.<epoch>` for one-shot migration rename).
pub const LOG_FILENAME_PREFIX: &str = "daemon";
/// File-name extension. Matches existing operator scripts that tail
/// `daemon.log*`.
pub const LOG_FILENAME_SUFFIX: &str = "log";

/// Migration-rename suffix prefix. Deliberately distinct from rotating
/// `<date>` suffix so cleanup-tick can track both patterns without
/// collision when same-day boots happen.
pub const MIGRATION_SUFFIX_PREFIX: &str = "migration.";

/// Parse `AGEND_LOG_MAX_BYTES` string into bytes. Accepts plain integers
/// (`2147483648`) and `K`/`M`/`G` suffixes (case-insensitive, e.g.
/// `2G`, `500M`, `1024K`). Returns `None` for malformed input so callers
/// can fall back to [`DEFAULT_MAX_BYTES`].
pub fn parse_size(_s: &str) -> Option<u64> {
    // STUB: real impl in next commit.
    None
}

/// Rename any existing `<home>/daemon.log` to
/// `<home>/daemon.log.migration.<unix-epoch-seconds>` so the rolling
/// appender starts on a clean slate without losing operator history.
/// No-op when `daemon.log` is absent. Idempotent: if a previous
/// migration left a `daemon.log.migration.*` file behind AND `daemon.log`
/// also exists (e.g., old binary started again post-fix), leaves
/// `daemon.log` alone rather than double-rotating.
///
/// Returns the destination path on success. Caller policy on rename
/// failure: per lead synth, log via stderr passthrough + continue
/// daemon startup with rolling appender on the fresh path.
pub fn migrate_existing_daemon_log(_home: &Path) -> std::io::Result<Option<std::path::PathBuf>> {
    // STUB: real impl in next commit.
    Ok(None)
}

/// Hourly cleanup tick. Sums sizes of every `daemon.log.*` file in
/// `home` (rotated dates AND migration suffixes both count toward the
/// total); when total > `max_bytes`, deletes oldest by mtime until
/// total is under the cap. Returns the number of files removed for
/// telemetry.
pub fn cleanup_oversize_logs(_home: &Path, _max_bytes: u64) -> usize {
    // STUB: real impl in next commit.
    0
}

/// Maintain `<home>/daemon.log` symlink → newest `daemon.log.<date>`
/// rotated file so operator's `tail -F daemon.log` keeps tracking the
/// active file across rotation boundaries. Unix-only. Windows operator
/// must `glob daemon.log.*` per the lead-synthed BC note.
#[cfg(unix)]
pub fn update_daemon_log_symlink_unix(_home: &Path) {
    // STUB: real impl in next commit.
}

#[cfg(not(unix))]
pub fn update_daemon_log_symlink_unix(_home: &Path) {
    // BC note: Windows operators glob `daemon.log.*` instead of relying
    // on symlink. See PR #914 description + CLAUDE.md.
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agend-914-{}-{}-{}", tag, std::process::id(), id));
        std::fs::create_dir_all(&dir).expect("create tmp home");
        dir
    }

    // ----- parse_size -----

    #[test]
    fn parse_size_accepts_plain_integer() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("0"), Some(0));
    }

    #[test]
    fn parse_size_accepts_suffixes() {
        assert_eq!(parse_size("2K"), Some(2 * 1024));
        assert_eq!(parse_size("500M"), Some(500 * 1024 * 1024));
        assert_eq!(parse_size("2G"), Some(2 * 1024 * 1024 * 1024));
        // Case-insensitive
        assert_eq!(parse_size("1g"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("1m"), Some(1024 * 1024));
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size("12.5G"), None);
        assert_eq!(parse_size("G500"), None);
    }

    // ----- migrate_existing_daemon_log -----

    #[test]
    fn migrate_renames_existing_daemon_log_to_epoch_suffix() {
        let home = tmp_home("mig-basic");
        std::fs::write(home.join("daemon.log"), b"legacy content").unwrap();

        let result = migrate_existing_daemon_log(&home).expect("migrate ok");
        let target = result.expect("must return rename target when daemon.log existed");

        assert!(
            !home.join("daemon.log").exists(),
            "original daemon.log must be renamed away"
        );
        assert!(target.exists(), "renamed target must exist: {target:?}");
        let target_name = target.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            target_name.starts_with("daemon.log.migration."),
            "target must use migration.<epoch> suffix, got: {target_name}"
        );
        // Suffix after `daemon.log.migration.` must be all-digits unix epoch.
        let epoch_part = &target_name["daemon.log.migration.".len()..];
        assert!(
            epoch_part.chars().all(|c| c.is_ascii_digit()) && !epoch_part.is_empty(),
            "epoch suffix must be digits, got: {epoch_part}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn migrate_is_noop_when_no_daemon_log() {
        let home = tmp_home("mig-none");

        let result = migrate_existing_daemon_log(&home).expect("ok on empty");
        assert!(
            result.is_none(),
            "no-op must return None when daemon.log absent"
        );

        // No new files should be created.
        let entries: Vec<_> = std::fs::read_dir(&home).unwrap().flatten().collect();
        assert!(
            entries.is_empty(),
            "no-op migrate must not create files, got: {entries:?}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn migrate_is_idempotent_when_prior_migration_present() {
        let home = tmp_home("mig-idemp");
        // Pre-seed a prior migration result + a fresh daemon.log (e.g.,
        // old binary restarted after fix landed).
        std::fs::write(home.join("daemon.log.migration.1000000000"), b"prior").unwrap();
        std::fs::write(home.join("daemon.log"), b"fresh from old binary").unwrap();

        let result = migrate_existing_daemon_log(&home).expect("idempotent ok");
        assert!(
            result.is_none(),
            "idempotent path must NOT rename when prior migration exists"
        );
        assert!(
            home.join("daemon.log").exists(),
            "daemon.log must be left in place when skipping"
        );
        assert!(
            home.join("daemon.log.migration.1000000000").exists(),
            "prior migration file must not be touched"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // ----- cleanup_oversize_logs -----

    #[test]
    fn cleanup_prunes_oldest_until_under_cap() {
        let home = tmp_home("cleanup-prune");
        // Seed 5 rotated files at 1 KB each. Total 5 KB. Cap = 3 KB
        // → 2 oldest must be removed.
        for i in 0..5u32 {
            let path = home.join(format!("daemon.log.2026-05-{:02}", 10 + i));
            std::fs::write(&path, vec![0u8; 1024]).unwrap();
            // Force mtime to be increasing so "oldest" is well-defined.
            // (Most filesystems give us this for free on sequential writes,
            // but be explicit.)
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let removed = cleanup_oversize_logs(&home, 3 * 1024);
        assert!(
            removed >= 2,
            "must remove >= 2 files to fit 5KB under 3KB cap, got removed={removed}"
        );

        // Total remaining size must be <= cap.
        let total: u64 = std::fs::read_dir(&home)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("daemon.log."))
            .filter_map(|e| e.metadata().ok().map(|m| m.len()))
            .sum();
        assert!(
            total <= 3 * 1024,
            "total size after cleanup must be <= cap (3 KB), got: {total}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_noop_when_under_cap() {
        let home = tmp_home("cleanup-noop");
        for i in 0..3u32 {
            let path = home.join(format!("daemon.log.2026-05-{:02}", 10 + i));
            std::fs::write(&path, vec![0u8; 1024]).unwrap();
        }

        let removed = cleanup_oversize_logs(&home, 100 * 1024 * 1024);
        assert_eq!(
            removed, 0,
            "must not remove anything when total well under cap, got removed={removed}"
        );
        let remaining: usize = std::fs::read_dir(&home).unwrap().flatten().count();
        assert_eq!(remaining, 3, "all files must remain");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_counts_migration_files_toward_budget() {
        let home = tmp_home("cleanup-mig");
        // 1 KB rotated + 4 KB migration => 5 KB total, cap 3 KB.
        std::fs::write(home.join("daemon.log.2026-05-15"), vec![0u8; 1024]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(
            home.join("daemon.log.migration.1700000000"),
            vec![0u8; 4 * 1024],
        )
        .unwrap();

        let removed = cleanup_oversize_logs(&home, 3 * 1024);
        assert!(
            removed >= 1,
            "must prune to fit under cap, got removed={removed}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // ----- update_daemon_log_symlink_unix -----

    #[cfg(unix)]
    #[test]
    fn symlink_points_at_newest_rotated_file() {
        let home = tmp_home("symlink-newest");
        // Three rotated files at increasing dates.
        std::fs::write(home.join("daemon.log.2026-05-10"), b"oldest").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(home.join("daemon.log.2026-05-11"), b"middle").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(home.join("daemon.log.2026-05-12"), b"newest").unwrap();

        update_daemon_log_symlink_unix(&home);

        let link = home.join("daemon.log");
        assert!(link.exists(), "symlink must be created");
        let target = std::fs::read_link(&link).expect("must be a symlink");
        // Target is stored as the file name (relative) or as a full path —
        // either way it must reference the newest.
        let target_str = target.to_string_lossy();
        assert!(
            target_str.ends_with("daemon.log.2026-05-12"),
            "symlink must point at newest (daemon.log.2026-05-12), got: {target_str}"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
