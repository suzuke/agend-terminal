//! Windows autostart: HKCU\Software\Microsoft\Windows\CurrentVersion\Run
//! value `AgendTerminal` via `windows-sys`. PLAN task #2.
//!
//! When wiring this up: add `Win32_System_Registry` to the **existing**
//! `windows-sys` features list in `Cargo.toml` — do not declare a second
//! entry (PLAN gotcha).

use super::Autostart;

pub struct WindowsAutostart;

impl Autostart for WindowsAutostart {
    fn enable(&self) -> anyhow::Result<()> {
        anyhow::bail!("tray autostart (Windows): not yet implemented — PLAN task #2")
    }

    fn disable(&self) -> anyhow::Result<()> {
        anyhow::bail!("tray autostart (Windows): not yet implemented — PLAN task #2")
    }

    fn is_enabled(&self) -> anyhow::Result<bool> {
        Ok(false)
    }
}
