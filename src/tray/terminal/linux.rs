//! Linux: `$TERMINAL` → `x-terminal-emulator` → PATH fallback chain.
//! PLAN task #3.

use super::OpenInTerminal;

pub struct LinuxTerminal;

impl OpenInTerminal for LinuxTerminal {
    fn open(&self, _cmd: &[&str]) -> anyhow::Result<()> {
        anyhow::bail!("tray OpenInTerminal (Linux): not yet implemented — PLAN task #3")
    }
}
