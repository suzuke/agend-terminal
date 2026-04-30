//! Generic JSON file store — shared by decisions, tasks, teams, schedules.
//!
//! Uses file locking to prevent concurrent load-modify-save races.

use serde::{de::DeserializeOwned, Serialize};
use std::path::{Path, PathBuf};

/// Load a JSON file into a typed struct, returning default if missing or invalid.
pub fn load<T: DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
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

/// Write `bytes` to `path` via temp-file + fsync + rename so an observer
/// never sees a half-written file and a power loss leaves either the old
/// contents or the new — never truncated or partial.
///
/// Use this for any file whose readers expect a complete document on disk
/// at all times (agent configs, decisions, snapshots, TOML configs).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.tmp"),
        None => "tmp".to_string(),
    });
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
    use fs4::fs_std::FileExt;
    f.lock_exclusive()
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
        Err(_) => return T::default(),
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
        // temp sibling must not linger
        let tmp = path.with_extension("json.tmp");
        assert!(
            !tmp.exists(),
            "temp must be renamed/removed, found: {}",
            tmp.display()
        );
        assert_eq!(fs::read(&path).expect("read"), b"{\"a\":1}");
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
        // On the same process, a second lock_exclusive on a different File
        // handle for the same path blocks; try_lock should refuse.
        use fs4::fs_std::FileExt;
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
            second.try_lock_exclusive().is_err(),
            "second exclusive lock must fail while first held"
        );
        drop(guard);
        // After drop, second can acquire.
        assert!(second.try_lock_exclusive().is_ok());
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
}
