//! #914 daemon log rotation + #927 PR-A app log parametrization.
//!
//! Provides:
//! - [`setup_rolling_tracing`] — install panic hook + rolling-file tracing
//!   subscriber. Parametrized over filename prefix + default filter +
//!   migration policy so daemon AND app paths share one implementation.
//!   Called from `main` (daemon child) and `app::run` (TUI process).
//! - [`migrate_existing_log`] — idempotent rename of any pre-rotation
//!   `<prefix>.log` left by old binaries. Behaviour gated by
//!   [`MigrationPolicy`]: daemon path keeps history via
//!   `<prefix>.log.migration.<epoch>` rename; app path drops the tiny
//!   pre-rotation file outright (operator chose drop in synthesis —
//!   `app.log` was ~12KB, no rescue value).
//! - [`cleanup_oversize_logs`] — hard backstop. Pruning the oldest
//!   `daemon.log.*` files until the directory's total log footprint is
//!   under `AGEND_LOG_MAX_BYTES`. Wired into the per-tick handler so it
//!   runs hourly regardless of `max_log_files`. Daemon-only today;
//!   app's tighter `max_log_files=3` retention bounds disk use without
//!   an app-side tick.
//! - [`update_daemon_log_symlink_unix`] — points `daemon.log` symlink at
//!   the newest rotated file so `tail -F daemon.log` keeps working post-rotation.
//!
//! See `[setup_rolling_tracing]` for the canonical entry. CLI
//! commands route through `[setup_cli_tracing]` and keep stderr.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// Default daily retention when `AGEND_LOG_RETAIN_DAYS` is unset.
/// 3 files × ~800 MB worst-case per file ≈ 2.4 GB ceiling; the
/// `AGEND_LOG_MAX_BYTES` hard backstop catches the heavy-traffic case.
pub const DEFAULT_RETAIN_DAYS: usize = 3;

/// Hard directory-size backstop in bytes. Hourly cleanup tick prunes
/// oldest `daemon.log.*` until total under this cap.
pub const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Legacy file-name prefix constant — kept for callers (per-tick
/// cleanup, symlink update) that operate exclusively on the daemon
/// `daemon.log.*` namespace. New callers should pass an explicit
/// prefix string to [`setup_rolling_tracing`]; this constant is
/// reserved for the daemon-side helpers.
#[allow(dead_code)] // reserved for cleanup_oversize_logs / symlink helpers
pub const LOG_FILENAME_PREFIX: &str = "daemon";
/// File-name extension. Matches existing operator scripts that tail
/// `daemon.log*`.
pub const LOG_FILENAME_SUFFIX: &str = "log";

/// Migration-rename suffix prefix. Deliberately distinct from rotating
/// `<date>` suffix so cleanup-tick can track both patterns without
/// collision when same-day boots happen.
pub const MIGRATION_SUFFIX_PREFIX: &str = "migration.";

/// Read `AGEND_LOG_RETAIN_DAYS` env var, fall back to
/// [`DEFAULT_RETAIN_DAYS`]. Used as `max_log_files` on the rolling
/// appender so the daemon never accumulates more than this many
/// rotated files (orthogonal to the `AGEND_LOG_MAX_BYTES` hard cap
/// enforced by the per-tick cleanup).
fn retain_days_from_env() -> usize {
    std::env::var("AGEND_LOG_RETAIN_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_RETAIN_DAYS)
}

/// Stderr-tracing init for CLI commands (`inject`, `list`, `kill`,
/// `start` (parent of detach), every non-daemon command). Identical
/// to the pre-#914 init — extracted here so `main` has one symmetric
/// helper for the daemon vs CLI fork.
pub fn setup_cli_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("AGEND_LOG")
                .unwrap_or_else(|_| EnvFilter::new("agend_terminal=info")),
        )
        .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339())
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

/// Per-process pre-rotation file migration policy. #927 PR-A added the
/// app path which historically used `truncate(true)` on a tiny
/// `app.log`; the synthesis decided to DROP that file on first boot of
/// the rolling appender rather than preserve trivial history. The
/// daemon path keeps the [`MigrationPolicy::Migrate`] behaviour from
/// #914 so pre-rotation `daemon.log` (potentially MBs of operator
/// history) is rescued via `.migration.<epoch>` rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationPolicy {
    /// Rename pre-existing `<prefix>.log` → `<prefix>.log.migration.<epoch>`.
    /// Idempotent: skipped when a prior migration marker already exists.
    /// Used by the daemon path.
    Migrate,
    /// Delete pre-existing `<prefix>.log` outright. Used by the app
    /// path where the pre-rotation file is tiny and not worth
    /// preserving.
    Drop,
}

/// Parametrized rolling-tracing init for both daemon and app paths.
/// `filename_prefix` is the rotating filename stem (e.g. `"daemon"`
/// produces `daemon.log.<YYYY-MM-DD>`). `default_filter` is the
/// `EnvFilter` directive applied when `AGEND_LOG` is unset.
/// `migration_policy` controls how pre-rotation `<prefix>.log` files
/// from older binaries are handled (see [`MigrationPolicy`]).
///
/// Caller MUST hold the returned [`WorkerGuard`] for the full process
/// lifetime — dropping it shuts the background writer thread down and
/// may drop the last batch of pending log records.
///
/// `with_ansi(false)`: per #914 lead synth, intentional observable
/// change. Pre-#914 daemon stderr was redirected to a file and tracing
/// wrote ANSI codes into it (visible as `\x1b[...]` noise in
/// `cat`/`grep`); now rotated log files contain plain text.
///
/// Also installs a panic hook so panics route through `tracing::error!`
/// into the rolling file. Without this, panics print to stderr — which
/// the post-#914 `spawn_detached` sends to `/dev/null` — and would be
/// invisible to the operator.
///
/// #927 PR-A: previously named `setup_daemon_tracing`; parametrized to
/// let `app::run` (TUI process) share the rolling-appender + panic-hook
/// machinery instead of bypassing it with a raw `OpenOptions::truncate`
/// call on `app.log`.
pub fn setup_rolling_tracing(
    home: &Path,
    filename_prefix: &str,
    default_filter: &str,
    migration_policy: MigrationPolicy,
) -> anyhow::Result<WorkerGuard> {
    let log_name = format!("{filename_prefix}.{LOG_FILENAME_SUFFIX}");
    if let Err(e) = migrate_existing_log(home, &log_name, migration_policy) {
        eprintln!(
            "agend-terminal: {log_name} migration failed: {e} \
             (continuing with fresh rolling appender)"
        );
    }

    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .max_log_files(retain_days_from_env())
        .filename_prefix(filename_prefix)
        .filename_suffix(LOG_FILENAME_SUFFIX)
        .build(home)
        .map_err(|e| {
            anyhow::anyhow!("{filename_prefix} log: RollingFileAppender::build failed: {e}")
        })?;
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("AGEND_LOG").unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339())
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(false)
        .try_init()
        .ok(); // try_init: app::run may share a process with already-init subscriber in tests

    install_panic_to_tracing_hook();

    Ok(guard)
}

/// Install a `panic::set_hook` that forwards panics to `tracing::error!`
/// so the rolling-file appender captures them. Chains the previous
/// (default) hook after so panic messages still reach stderr-if-attached
/// during foreground daemon runs (`agend-terminal start --foreground` in
/// a terminal, not via `spawn_detached`'s `/dev/null` redirect).
fn install_panic_to_tracing_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let location = panic_info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let payload = panic_info.payload();
        let msg = payload
            .downcast_ref::<&str>()
            .copied()
            .map(str::to_string)
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        tracing::error!(location = %location, message = %msg, "panic");
        previous(panic_info);
    }));
}

/// Parse `AGEND_LOG_MAX_BYTES` string into bytes. Accepts plain integers
/// (`2147483648`) and `K`/`M`/`G` suffixes (case-insensitive, e.g.
/// `2G`, `500M`, `1024K`). Returns `None` for malformed input so callers
/// can fall back to [`DEFAULT_MAX_BYTES`].
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_part, mult): (&str, u64) = if let Some(stripped) = s.strip_suffix(['G', 'g']) {
        (stripped, 1024 * 1024 * 1024)
    } else if let Some(stripped) = s.strip_suffix(['M', 'm']) {
        (stripped, 1024 * 1024)
    } else if let Some(stripped) = s.strip_suffix(['K', 'k']) {
        (stripped, 1024)
    } else {
        (s, 1)
    };
    let num_part = num_part.trim();
    if num_part.is_empty() || !num_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    num_part
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
}

/// Idempotent pre-rotation file migration. Behavior gated by
/// [`MigrationPolicy`]:
///
/// - [`MigrationPolicy::Migrate`]: rename `<home>/<filename>` to
///   `<home>/<filename>.migration.<unix-epoch-seconds>` (preserves
///   operator history). No-op when absent. Idempotent: if a prior
///   `<filename>.migration.*` already exists AND `<filename>` also
///   exists (e.g., old binary restarted post-fix), leaves the new
///   `<filename>` alone rather than double-rotating.
/// - [`MigrationPolicy::Drop`]: delete `<home>/<filename>` outright.
///   No-op when absent. Used by the app path per synthesis (file is
///   tiny, no rescue value).
///
/// Returns the destination path on `Migrate` success, `None` for noop /
/// Drop. Caller policy on failure: per lead synth, stderr passthrough
/// + continue startup with fresh rolling appender on the new path.
///
/// `symlink_metadata` (not `exists`) so we observe a post-#914
/// `daemon.log` symlink as such and leave it alone instead of treating
/// it as a legacy file to migrate.
pub fn migrate_existing_log(
    home: &Path,
    filename: &str,
    policy: MigrationPolicy,
) -> std::io::Result<Option<std::path::PathBuf>> {
    let src = home.join(filename);
    let meta = match std::fs::symlink_metadata(&src) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        // Post-fix symlink — leave alone.
        return Ok(None);
    }
    match policy {
        MigrationPolicy::Drop => {
            std::fs::remove_file(&src)?;
            Ok(None)
        }
        MigrationPolicy::Migrate => {
            // Idempotence: any pre-existing migration marker means a
            // prior boot already migrated; leave the new file untouched.
            let migration_prefix = format!("{filename}.{MIGRATION_SUFFIX_PREFIX}");
            let already_migrated = std::fs::read_dir(home)
                .ok()
                .map(|entries| {
                    entries.flatten().any(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .starts_with(&migration_prefix)
                    })
                })
                .unwrap_or(false);
            if already_migrated {
                return Ok(None);
            }
            let epoch = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let target = home.join(format!("{filename}.{MIGRATION_SUFFIX_PREFIX}{epoch}"));
            std::fs::rename(&src, &target)?;
            Ok(Some(target))
        }
    }
}

/// Hourly cleanup tick. Sums sizes of every `daemon.log.*` file in
/// `home` (rotated dates AND migration suffixes both count toward the
/// total); when total > `max_bytes`, deletes oldest by mtime until
/// total is under the cap. Returns the number of files removed for
/// telemetry.
pub fn cleanup_oversize_logs(home: &Path, max_bytes: u64) -> usize {
    let entries_iter = match std::fs::read_dir(home) {
        Ok(d) => d,
        Err(_) => return 0,
    };
    let mut entries: Vec<(std::path::PathBuf, u64, std::time::SystemTime)> = entries_iter
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Match rotated (`daemon.log.<date>`) AND migration
            // (`daemon.log.migration.<epoch>`) — both count toward budget.
            // Exclude the symlink itself (we want the underlying files in
            // the budget, not the link byte-count).
            if !name_str.starts_with("daemon.log.") {
                return None;
            }
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let mtime = meta.modified().ok()?;
            Some((entry.path(), meta.len(), mtime))
        })
        .collect();
    let total: u64 = entries.iter().map(|(_, s, _)| *s).sum();
    if total <= max_bytes {
        return 0;
    }
    // Oldest first — preserves the most recent N days of logs the
    // operator is most likely investigating.
    entries.sort_by_key(|(_, _, m)| *m);
    let mut current = total;
    let mut removed = 0;
    for (path, size, _) in &entries {
        if current <= max_bytes {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            current = current.saturating_sub(*size);
            removed += 1;
        }
    }
    removed
}

/// Maintain `<home>/daemon.log` symlink → newest `daemon.log.<date>`
/// rotated file so operator's `tail -F daemon.log` keeps tracking the
/// active file across rotation boundaries. Unix-only. Windows operator
/// must `glob daemon.log.*` per the lead-synthed BC note.
#[cfg(unix)]
pub fn update_daemon_log_symlink_unix(home: &Path) {
    use std::os::unix::fs as unix_fs;
    let entries_iter = match std::fs::read_dir(home) {
        Ok(d) => d,
        Err(_) => return,
    };
    // Newest rotated file (`daemon.log.<date>`), excluding migration
    // markers (those are static historic snapshots — never the active
    // write target) and the symlink itself.
    let newest = entries_iter
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name_str = name.to_string_lossy().to_string();
            if !name_str.starts_with("daemon.log.") || name_str.contains(MIGRATION_SUFFIX_PREFIX) {
                return None;
            }
            let ft = entry.file_type().ok()?;
            if ft.is_symlink() {
                return None;
            }
            let mtime = entry.metadata().ok().and_then(|m| m.modified().ok())?;
            Some((entry.path(), mtime))
        })
        .max_by_key(|(_, mtime)| *mtime);
    let Some((newest_path, _)) = newest else {
        return;
    };
    let link = home.join("daemon.log");
    // Remove existing link/file before re-creating; symlink() refuses an
    // existing path. `remove_file` works for both regular files and
    // symlinks (the symlink itself, not the target, on Unix).
    if std::fs::symlink_metadata(&link).is_ok() {
        let _ = std::fs::remove_file(&link);
    }
    // Relative target so operator can `mv $AGEND_HOME` without dangling.
    let target_name = match newest_path.file_name() {
        Some(n) => n.to_owned(),
        None => return,
    };
    let _ = unix_fs::symlink(&target_name, &link);
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

    // ----- migrate_existing_log -----

    #[test]
    fn migrate_renames_existing_daemon_log_to_epoch_suffix() {
        let home = tmp_home("mig-basic");
        std::fs::write(home.join("daemon.log"), b"legacy content").unwrap();

        let result = migrate_existing_log(&home, "daemon.log", MigrationPolicy::Migrate)
            .expect("migrate ok");
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

        let result = migrate_existing_log(&home, "daemon.log", MigrationPolicy::Migrate)
            .expect("ok on empty");
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

        let result = migrate_existing_log(&home, "daemon.log", MigrationPolicy::Migrate)
            .expect("idempotent ok");
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

    // ----- #927 PR-A: app-path migration policy + filename prefix -----

    /// `MigrationPolicy::Drop` for app first-boot: pre-seed `app.log`,
    /// invoke `migrate_existing_log`, assert the file is gone + no
    /// migration sidecar created. Daemon path's `Migrate` policy is
    /// unaffected and still covered by the daemon-path tests above.
    #[test]
    fn app_log_migration_drop_policy_removes_file() {
        let home = tmp_home("app-mig-drop");
        std::fs::write(home.join("app.log"), b"tiny app history (12KB)").unwrap();

        let result =
            migrate_existing_log(&home, "app.log", MigrationPolicy::Drop).expect("drop ok");

        assert!(
            result.is_none(),
            "Drop policy MUST return None (no rename target)"
        );
        assert!(
            !home.join("app.log").exists(),
            "Drop policy MUST remove the pre-existing app.log"
        );
        // No migration sidecar should be created for Drop policy.
        let any_migration = std::fs::read_dir(&home).unwrap().flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("app.log.migration.")
        });
        assert!(
            !any_migration,
            "Drop policy MUST NOT create any app.log.migration.* sidecar"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// `app.log` prefix must produce `app.log.*` rotated files —
    /// distinct namespace from `daemon.log.*`. This test runs both
    /// migrate calls + sanity-checks namespacing.
    #[test]
    fn app_log_migration_uses_app_prefix_namespace() {
        let home = tmp_home("app-mig-prefix");
        // Seed both pre-rotation files (daemon AND app) so we can verify
        // the daemon path is unaffected by the app migration.
        std::fs::write(home.join("daemon.log"), b"daemon legacy").unwrap();
        std::fs::write(home.join("app.log"), b"app legacy").unwrap();

        // App migration: Drop → app.log removed, no sidecar.
        let app_result =
            migrate_existing_log(&home, "app.log", MigrationPolicy::Drop).expect("app drop ok");
        assert!(app_result.is_none());
        assert!(!home.join("app.log").exists(), "app.log dropped");

        // Daemon migration: Migrate → daemon.log renamed to sidecar.
        let daemon_result = migrate_existing_log(&home, "daemon.log", MigrationPolicy::Migrate)
            .expect("daemon migrate ok");
        let daemon_target = daemon_result.expect("Migrate produces target");
        assert!(daemon_target.exists());
        let target_name = daemon_target
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(
            target_name.starts_with("daemon.log.migration."),
            "daemon path uses daemon.log.migration.* namespace, got {target_name}"
        );
        assert!(
            !target_name.starts_with("app.log."),
            "app prefix namespace MUST NOT leak into daemon migration target"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Migration policy is policy-orthogonal across prefixes: Drop on a
    /// `daemon.log` (hypothetical fresh-install scenario or operator-
    /// driven cleanup) MUST work the same as Drop on app.log. Locks
    /// the policy enum's behavior as filename-independent.
    #[test]
    fn migrate_drop_policy_is_filename_independent() {
        let home = tmp_home("policy-orth");
        std::fs::write(home.join("daemon.log"), b"unwanted").unwrap();
        std::fs::write(home.join("app.log"), b"unwanted").unwrap();

        for name in &["daemon.log", "app.log"] {
            let r = migrate_existing_log(&home, name, MigrationPolicy::Drop).expect("ok");
            assert!(r.is_none(), "{name}: Drop returns None");
            assert!(!home.join(name).exists(), "{name}: removed");
        }

        std::fs::remove_dir_all(&home).ok();
    }
}
