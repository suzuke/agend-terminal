//! macOS autostart: LaunchAgent plist at
//! `~/Library/LaunchAgents/io.github.suzuke.agend-terminal.plist` +
//! `launchctl bootstrap gui/$UID`. PLAN task #2.

use super::Autostart;

pub struct MacAutostart;

impl Autostart for MacAutostart {
    fn enable(&self) -> anyhow::Result<()> {
        anyhow::bail!("tray autostart (macOS): not yet implemented — PLAN task #2")
    }

    fn disable(&self) -> anyhow::Result<()> {
        anyhow::bail!("tray autostart (macOS): not yet implemented — PLAN task #2")
    }

    fn is_enabled(&self) -> anyhow::Result<bool> {
        Ok(false)
    }
}
