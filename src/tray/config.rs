//! `$AGEND_HOME/tray.toml` schema.
//!
//! MVP: single `terminal` key. Missing file → defaults. Malformed →
//! warn-and-default (the tray must never crash on parse). See
//! `docs/archived/PLAN-tray-resident.md` §"tray.toml (MVP schema)".

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrayConfig {
    /// `"default"` auto-detects per platform. Any other value names a
    /// terminal emulator — either a known handler (e.g. `"iTerm"`,
    /// `"wt"`, `"gnome-terminal"`) or an executable in `PATH`.
    #[serde(default = "default_terminal")]
    pub terminal: String,
}

fn default_terminal() -> String {
    "default".to_string()
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            terminal: default_terminal(),
        }
    }
}

/// Read `$AGEND_HOME/tray.toml`. PLAN locks this to warn-and-default
/// on every failure mode — missing file (normal, user never edited),
/// unreadable file (permissions), or malformed TOML (user typo). The
/// tray never crashes on parse; a broken config just behaves like
/// `default()`.
pub fn load(home: &Path) -> TrayConfig {
    let path = home.join("tray.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return TrayConfig::default(),
        Err(e) => {
            eprintln!("tray: failed to read {}: {e}", path.display());
            return TrayConfig::default();
        }
    };
    match toml::from_str(&raw) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("tray: malformed {}: {e}", path.display());
            TrayConfig::default()
        }
    }
}
