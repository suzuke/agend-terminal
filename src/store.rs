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
