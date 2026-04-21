//! Windows: `wt.exe` with `conhost` fallback. PLAN task #3.
//!
//! The exact `wt.exe` flag form differs between cold-start and
//! already-running sessions — prototype on Windows before pinning the
//! command string (PLAN gotcha).

use super::OpenInTerminal;

pub struct WindowsTerminal;

impl OpenInTerminal for WindowsTerminal {
    fn open(&self, _cmd: &[&str]) -> anyhow::Result<()> {
        anyhow::bail!("tray OpenInTerminal (Windows): not yet implemented — PLAN task #3")
    }
}
