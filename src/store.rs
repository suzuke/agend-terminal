//! Generic JSON file store — shared by decisions, tasks, teams, schedules.

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
pub fn save<T: Serialize>(path: &Path, data: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(data)?)?;
    Ok(())
}

/// Helper: build a store path from home + filename.
pub fn store_path(home: &Path, filename: &str) -> PathBuf {
    home.join(filename)
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
        let dir = std::env::temp_dir().join(format!("agend-store-test-{}-{}-{}", std::process::id(), name, id));
        fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn test_roundtrip() {
        let dir = tmp_dir("roundtrip");
        let path = dir.join("roundtrip.json");
        let data = TestData { items: vec!["a".into(), "b".into()], count: 42 };
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
        let data = TestData { items: vec!["x".into()], count: 1 };
        save(&path, &data).expect("save with nested dirs");
        let loaded: TestData = load(&path);
        assert_eq!(loaded, data);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_overwrite() {
        let dir = tmp_dir("overwrite");
        let path = dir.join("overwrite.json");
        let v1 = TestData { items: vec!["old".into()], count: 1 };
        let v2 = TestData { items: vec!["new".into()], count: 2 };
        save(&path, &v1).expect("save v1");
        save(&path, &v2).expect("save v2");
        let loaded: TestData = load(&path);
        assert_eq!(loaded, v2);
        fs::remove_dir_all(&dir).ok();
    }
}
