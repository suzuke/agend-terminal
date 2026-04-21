//! macOS: `open -na <Terminal|iTerm|Ghostty> --args ...`. PLAN task #3.

use super::OpenInTerminal;

pub struct MacTerminal;

impl OpenInTerminal for MacTerminal {
    fn open(&self, _cmd: &[&str]) -> anyhow::Result<()> {
        anyhow::bail!("tray OpenInTerminal (macOS): not yet implemented — PLAN task #3")
    }
}
