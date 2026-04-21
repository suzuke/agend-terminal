//! `$AGEND_HOME/tray.toml` schema.
//!
//! MVP: single `terminal` key. Missing file → defaults. Malformed →
//! warn-and-default (the tray must never crash on parse). See
//! `docs/PLAN-tray-resident.md` §"tray.toml (MVP schema)".

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
