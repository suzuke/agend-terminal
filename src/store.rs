//! Generic JSON file store — shared by decisions, tasks, teams, schedules.
//!
//! Uses file locking to prevent concurrent load-modify-save races.

use serde::{de::DeserializeOwned, Serialize};
use std::path::{Path, PathBuf};

/// Load a JSON file into a typed struct, returning default if missing or invalid.
pub fn load<T: DeserializeOwned + Default>(path: &Path) -> T {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return T::default(),
    };
    if content.trim().is_empty() {
        return T::default();
    }
    match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "store load: corrupt JSON, returning default — backing up corrupt file"
            );
            // M1: backup corrupt file so operator can inspect/recover
            let backup = path.with_extension(format!(
                "corrupt.{}",
                chrono::Utc::now().format("%Y%m%d%H%M%S")
            ));
            let _ = std::fs::copy(path, &backup);
            T::default()
        }
    }
}

/// Save a typed struct to a JSON file. Returns error on failure.
#[allow(dead_code)] // Last caller (deployments::save) migrated to save_atomic; kept for future use
pub fn save<T: Serialize>(path: &Path, data: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(data)?)?;
    Ok(())
}

/// #965 process-wide unique tmp suffix counter. Combined with `process::id`
/// so cross-process concurrent atomic_write calls (CLI + daemon, or
/// multiple agend-terminal CLIs) also receive distinct tmp paths.
static ATOMIC_WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// #965 RAII guard that unlinks the temp file if [`atomic_write`] fails
/// before the rename succeeds. Without this, every failed atomic_write
/// (disk full, fsync error, permission denied between create and rename)
/// would leak a unique-named tmp file in the destination directory.
/// `disarm()` is called immediately before the rename: if the rename
/// succeeds the tmp path no longer exists, so a Drop-side `remove_file`
/// would be a no-op; if the rename fails we WANT the cleanup, which is
/// exactly what staying armed achieves.
struct TmpGuard<'a> {
    path: &'a Path,
    armed: bool,
}

impl<'a> TmpGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TmpGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

/// Write `bytes` to `path` via temp-file + fsync + rename so an observer
/// never sees a half-written file and a power loss leaves either the old
/// contents or the new — never truncated or partial.
///
/// #965: per-call unique tmp filename (`<path>.<pid>.<seq>.tmp`) eliminates
/// the shared-tmp-inode race. Pre-#965 every caller wrote to the same
/// `<path>.tmp`; concurrent invocations on the same destination raced on
/// the shared inode (truncate-truncate-interleaved-writes-rename) and
/// published corrupted bytes. With unique names each call owns its own
/// tmp inode end-to-end; the final rename is the only contention point
/// and POSIX `rename(2)` is atomic per destination directory entry.
///
/// Failure paths (Err between create and rename) are covered by a Drop
/// guard that unlinks the orphan tmp file.
///
/// Use this for any file whose readers expect a complete document on disk
/// at all times (agent configs, decisions, snapshots, TOML configs).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::sync::atomic::Ordering;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let seq = ATOMIC_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let tmp_suffix = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.{pid}.{seq}.tmp"),
        None => format!("{pid}.{seq}.tmp"),
    };
    let tmp = path.with_extension(tmp_suffix);
    let mut guard = TmpGuard::new(&tmp);
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    guard.disarm();
    Ok(())
}

/// Serialize `data` as pretty JSON and [`atomic_write`] it to `path`.
pub fn save_atomic<T: Serialize>(path: &Path, data: &T) -> anyhow::Result<()> {
    let body = serde_json::to_string_pretty(data)?;
    atomic_write(path, body.as_bytes())
}

/// Acquire an exclusive advisory lock tied to `lock_path`.
///
/// Released when the returned handle is dropped. Do NOT open the lock file
/// with `truncate(true)` — truncation is unnecessary (the file contents are
/// meaningless) and invites confusion in crash paths where a partially
/// initialised lock file might be observed empty by another opener while
/// the flock itself is still held on the inode.
pub fn acquire_file_lock(lock_path: &Path) -> anyhow::Result<std::fs::File> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    // Trait method explicit because Rust 1.89 stabilized inherent
    // `File::lock` with the same name; without explicit trait syntax,
    // the inherent method would be selected and clippy MSRV gate fires
    // (current MSRV is 1.87 per `rust-version`).
    fs4::FileExt::lock(&f)
        .map_err(|e| anyhow::anyhow!("flock failed on {}: {e}", lock_path.display()))?;
    Ok(f)
}

/// Helper: build a store path from home + filename.
pub fn store_path(home: &Path, filename: &str) -> PathBuf {
    home.join(filename)
}

/// Stores that carry a `schema_version` field so a future binary can migrate
/// or reject forward-incompatible on-disk data (P2-8, review 2026-04-18).
///
/// Without this, an older daemon downgraded onto data written by a newer
/// daemon would silently `unwrap_or_default` (in [`load`]) and wipe
/// everything — the serde shape might be compatible for old fields but drop
/// new ones. By stamping a version and refusing futures, we turn that silent
/// data loss into a loud startup error.
pub trait SchemaVersioned {
    /// Latest version this binary understands how to read *and* write.
    const CURRENT: u32;
    /// Mutable access to the stored version so the mutate helper can stamp
    /// it before every save.
    fn version_mut(&mut self) -> &mut u32;
}

/// Load like [`load`], but reject files whose `schema_version` exceeds
/// `current_version`. Newer files surface as default + an error log rather
/// than being silently downgraded.
pub fn load_versioned<T: DeserializeOwned + Default>(path: &Path, current_version: u32) -> T {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return T::default(),
    };
    // Peek the raw JSON first so we can inspect schema_version without
    // committing to a full deserialize (which could fail on unknown required
    // fields and mask the version check). Then hand off to [`load`] for the
    // real decode, keeping a single source of truth for missing-file / empty
    // / corrupt fallbacks.
    let peek: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "load_versioned: corrupt JSON, returning default — backing up corrupt file"
            );
            let backup = path.with_extension(format!(
                "corrupt.{}",
                chrono::Utc::now().format("%Y%m%d%H%M%S")
            ));
            let _ = std::fs::copy(path, &backup);
            return T::default();
        }
    };
    let version = peek
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    if version > current_version {
        tracing::error!(
            path = %path.display(),
            found = version,
            supported = current_version,
            "refusing to load store written by a newer schema version"
        );
        return T::default();
    }
    load(path)
}

/// Versioned variant of [`mutate`]. Rejects future-versioned files on load
/// and stamps [`SchemaVersioned::CURRENT`] on every successful save.
pub fn mutate_versioned<T, R, F>(path: &Path, f: F) -> anyhow::Result<R>
where
    T: DeserializeOwned + Default + Serialize + SchemaVersioned,
    F: FnOnce(&mut T) -> anyhow::Result<R>,
{
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension("lock");
    let _lock = acquire_file_lock(&lock_path)?;

    let mut data: T = load_versioned(path, T::CURRENT);
    let result = f(&mut data)?;
    *data.version_mut() = T::CURRENT;
    save_atomic(path, &data)?;
    Ok(result)
}

/// Generic flock-guarded read-modify-write for any JSON state file.
///
/// Returns `Ok(None)` when the file does not exist (no mutation performed).
/// The lock file is `<path>.lock`; caller does not manage it.
pub fn with_json_state<T, R, F>(path: &Path, mutate: F) -> anyhow::Result<Option<R>>
where
    T: DeserializeOwned + Serialize,
    F: FnOnce(&mut T) -> R,
{
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension("lock");
    let _lock = acquire_file_lock(&lock_path)?;
    let Some(mut state) = std::fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str::<T>(&c).ok())
    else {
        return Ok(None);
    };
    let result = mutate(&mut state);
    let body = serde_json::to_string_pretty(&state)?;
    atomic_write(path, body.as_bytes())?;
    Ok(Some(result))
}

/// Like [`with_json_state`] but creates the file from `default_fn` when missing.
pub fn with_json_state_or_create<T, D, R, F>(
    path: &Path,
    default_fn: D,
    mutate: F,
) -> anyhow::Result<R>
where
    T: DeserializeOwned + Serialize,
    D: FnOnce() -> T,
    F: FnOnce(&mut T) -> R,
{
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension("lock");
    let _lock = acquire_file_lock(&lock_path)?;
    let mut state = std::fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str::<T>(&c).ok())
        .unwrap_or_else(default_fn);
    let result = mutate(&mut state);
    let body = serde_json::to_string_pretty(&state)?;
    atomic_write(path, body.as_bytes())?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::fs;

    #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
    struct TestData {
        items: Vec<String>,
        count: u32,
    }

    fn tmp_dir(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-store-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn test_roundtrip() {
        let dir = tmp_dir("roundtrip");
        let path = dir.join("roundtrip.json");
        let data = TestData {
            items: vec!["a".into(), "b".into()],
            count: 42,
        };
        save(&path, &data).expect("save");
        let loaded: TestData = load(&path);
        assert_eq!(loaded, data);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_missing_file_returns_default() {
        let path = PathBuf::from("/tmp/agend-store-test-nonexistent.json");
        let loaded: TestData = load(&path);
        assert_eq!(loaded, TestData::default());
    }

    #[test]
    fn test_corrupt_json_returns_default() {
        let dir = tmp_dir("corrupt");
        let path = dir.join("corrupt.json");
        fs::write(&path, "not valid json {{{").expect("write");
        let loaded: TestData = load(&path);
        assert_eq!(loaded, TestData::default());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_empty_file_returns_default() {
        let dir = tmp_dir("empty");
        let path = dir.join("empty.json");
        fs::write(&path, "").expect("write");
        let loaded: TestData = load(&path);
        assert_eq!(loaded, TestData::default());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_save_creates_parent_dirs() {
        let dir = tmp_dir("parent_dirs");
        let path = dir.join("nested/deep/data.json");
        let data = TestData {
            items: vec!["x".into()],
            count: 1,
        };
        save(&path, &data).expect("save with nested dirs");
        let loaded: TestData = load(&path);
        assert_eq!(loaded, data);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_overwrite() {
        let dir = tmp_dir("overwrite");
        let path = dir.join("overwrite.json");
        let v1 = TestData {
            items: vec!["old".into()],
            count: 1,
        };
        let v2 = TestData {
            items: vec!["new".into()],
            count: 2,
        };
        save(&path, &v1).expect("save v1");
        save(&path, &v2).expect("save v2");
        let loaded: TestData = load(&path);
        assert_eq!(loaded, v2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_atomic_write_leaves_no_tmp_on_success() {
        let dir = tmp_dir("atomic_ok");
        let path = dir.join("atomic.json");
        atomic_write(&path, b"{\"a\":1}").expect("write");
        assert!(path.exists());
        // #965: post-success, NO *.tmp sibling may linger. Pre-#965 we
        // only checked `<path>.tmp`; with unique tmp names this glob
        // covers all per-call tmp paths.
        let entries: Vec<_> = fs::read_dir(&dir)
            .expect("read_dir")
            .flatten()
            .filter(|e| {
                e.path()
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|n| n.contains(".tmp"))
            })
            .collect();
        assert!(
            entries.is_empty(),
            "no *.tmp sibling may linger post-success, found: {entries:?}"
        );
        assert_eq!(fs::read(&path).expect("read"), b"{\"a\":1}");
        fs::remove_dir_all(&dir).ok();
    }

    /// #965 T2 — Drop guard unlinks the tmp file when atomic_write fails
    /// between tmp creation and rename. Simulated via a non-writable
    /// destination directory (rename fails with EACCES on Unix) or a
    /// destination path that's actually a directory (rename fails with
    /// EISDIR). The latter is more portable.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_atomic_write_drop_guard_cleans_orphan_tmp_on_rename_failure() {
        let dir = tmp_dir("atomic_drop_guard");
        // Make `path` an existing DIRECTORY so rename(tmp, path) fails with
        // EISDIR / EEXIST. The tmp file is created and written successfully
        // first; the rename is what fails. Drop guard must clean up.
        let path = dir.join("dest");
        fs::create_dir_all(&path).unwrap();

        let err = atomic_write(&path, b"hello").expect_err("rename onto a directory must fail");
        // Don't assert error shape (OS-specific); just verify Err.
        let _ = err;

        // Sweep for orphan *.tmp siblings in the parent dir.
        let entries: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.path()
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|n| n.contains(".tmp"))
            })
            .collect();
        assert!(
            entries.is_empty(),
            "#965 Drop guard must unlink orphan tmp on rename failure, \
             found: {entries:?}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_save_atomic_roundtrip() {
        let dir = tmp_dir("save_atomic");
        let path = dir.join("sa.json");
        let data = TestData {
            items: vec!["z".into()],
            count: 7,
        };
        save_atomic(&path, &data).expect("save");
        let loaded: TestData = load(&path);
        assert_eq!(loaded, data);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_atomic_write_no_extension() {
        // Files without an extension must still get a distinct temp path
        // rather than colliding on the original path.
        let dir = tmp_dir("atomic_noext");
        let path = dir.join("LOCK");
        atomic_write(&path, b"x").expect("write");
        assert_eq!(fs::read(&path).expect("read"), b"x");
        let tmp = dir.join("LOCK.tmp");
        assert!(!tmp.exists(), "temp must not linger");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_acquire_file_lock_is_exclusive_same_process() {
        // On the same process, a second lock on a different File
        // handle for the same path blocks; try_lock should refuse.
        // Explicit trait method (see comment in acquire_file_lock).
        let dir = tmp_dir("flock");
        let lock_path = dir.join("my.lock");
        let guard = acquire_file_lock(&lock_path).expect("first lock");

        let second = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .expect("open");
        assert!(
            fs4::FileExt::try_lock(&second).is_err(),
            "second exclusive lock must fail while first held"
        );
        drop(guard);
        // After drop, second can acquire.
        assert!(fs4::FileExt::try_lock(&second).is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
    struct VersionedTestStore {
        #[serde(default)]
        schema_version: u32,
        payload: Vec<String>,
    }

    impl SchemaVersioned for VersionedTestStore {
        const CURRENT: u32 = 2;
        fn version_mut(&mut self) -> &mut u32 {
            &mut self.schema_version
        }
    }

    #[test]
    fn test_load_versioned_accepts_equal_or_older_version() {
        let dir = tmp_dir("versioned_ok");
        let path = dir.join("v.json");
        // Version == CURRENT: accepted.
        fs::write(&path, r#"{"schema_version": 2, "payload": ["keep"]}"#).expect("w");
        let got: VersionedTestStore = load_versioned(&path, VersionedTestStore::CURRENT);
        assert_eq!(got.payload, vec!["keep".to_string()]);
        assert_eq!(got.schema_version, 2);

        // Version == 0 (missing field): accepted as default-0.
        fs::write(&path, r#"{"payload": ["legacy"]}"#).expect("w");
        let got: VersionedTestStore = load_versioned(&path, VersionedTestStore::CURRENT);
        assert_eq!(got.payload, vec!["legacy".to_string()]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_versioned_refuses_future_version() {
        let dir = tmp_dir("versioned_future");
        let path = dir.join("v.json");
        // schema_version > CURRENT: refused, default returned.
        fs::write(&path, r#"{"schema_version": 99, "payload": ["alien"]}"#).expect("w");
        let got: VersionedTestStore = load_versioned(&path, VersionedTestStore::CURRENT);
        assert_eq!(got, VersionedTestStore::default());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_mutate_versioned_stamps_current_on_save() {
        let dir = tmp_dir("versioned_stamp");
        let path = dir.join("v.json");
        // Start from a legacy (unversioned) file.
        fs::write(&path, r#"{"payload": ["legacy"]}"#).expect("w");
        mutate_versioned(&path, |s: &mut VersionedTestStore| {
            s.payload.push("appended".into());
            Ok(())
        })
        .expect("mutate");
        let content = fs::read_to_string(&path).expect("r");
        // After save, schema_version must equal CURRENT (2).
        assert!(
            content.contains("\"schema_version\": 2"),
            "save must stamp CURRENT; got: {content}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_acquire_file_lock_no_truncate_preserves_lock_contents() {
        // Write a sentinel into the lock file, then re-acquire. The new
        // opener must NOT wipe the contents (we removed truncate(true)).
        // Release the lock before reading: on Windows, `LockFileEx` is a
        // byte-range lock that blocks reads from *any* handle, so the
        // assertion-time read would otherwise fail there with ERROR_LOCK_VIOLATION.
        // The semantic we care about — "open-with-truncate=false did not wipe
        // content" — is decided during acquisition and survives the drop.
        let dir = tmp_dir("flock_no_trunc");
        let lock_path = dir.join("my.lock");
        fs::write(&lock_path, "sentinel").expect("pre-write");
        let guard = acquire_file_lock(&lock_path).expect("lock");
        drop(guard);
        let after = fs::read_to_string(&lock_path).expect("read");
        assert_eq!(after, "sentinel", "lock acquisition must not truncate");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_corrupt_creates_backup() {
        let dir = tmp_dir("corrupt_backup");
        let path = dir.join("data.json");
        fs::write(&path, "not valid json {{{").expect("write");
        let loaded: TestData = load(&path);
        assert_eq!(loaded, TestData::default(), "corrupt should return default");
        // M1: backup file should exist
        let backups: Vec<_> = fs::read_dir(&dir)
            .expect("read dir")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("data.corrupt."))
            .collect();
        assert!(
            !backups.is_empty(),
            "corrupt file should be backed up: {:?}",
            fs::read_dir(&dir)
                .expect("read dir")
                .flatten()
                .map(|e| e.file_name())
                .collect::<Vec<_>>()
        );
        fs::remove_dir_all(&dir).ok();
    }
}
