//! Linux autostart: XDG `.desktop` at
//! `~/.config/autostart/agend-terminal.desktop`. PLAN task #2.

use super::Autostart;

pub struct LinuxAutostart;

impl Autostart for LinuxAutostart {
    fn enable(&self) -> anyhow::Result<()> {
        anyhow::bail!("tray autostart (Linux): not yet implemented — PLAN task #2")
    }

    fn disable(&self) -> anyhow::Result<()> {
        anyhow::bail!("tray autostart (Linux): not yet implemented — PLAN task #2")
    }

    fn is_enabled(&self) -> anyhow::Result<bool> {
        Ok(false)
    }
}
