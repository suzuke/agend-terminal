//! Centralized path helpers — single source of truth for daemon directory layout.

use std::path::{Path, PathBuf};

/// `<home>/workspace/` — per-agent working directories.
pub fn workspace_dir(home: &Path) -> PathBuf {
    home.join("workspace")
}

/// `<home>/runtime/` — per-agent runtime state (binding.json, metadata).
pub fn runtime_dir(home: &Path) -> PathBuf {
    home.join("runtime")
}

/// `<home>/runtime/<agent>/binding.json`
#[allow(dead_code)]
pub fn binding_path(home: &Path, agent: &str) -> PathBuf {
    runtime_dir(home).join(agent).join(BINDING_FILENAME)
}

/// Binding state filename.
#[allow(dead_code)]
pub const BINDING_FILENAME: &str = "binding.json";
