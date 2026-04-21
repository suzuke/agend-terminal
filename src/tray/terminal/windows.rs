//! Windows: `wt.exe` for the default/`wt` case, `cmd /c start` for
//! `conhost`, bare-executable otherwise.
//!
//! PLAN flags the cold-start vs running-instance `wt.exe` flag ambiguity
//! as a prototype-on-Windows task — the shape below matches the
//! cold-start form (`wt.exe <cmd> <args...>`). If an already-running wt
//! eats these differently we'll adjust in task #4 smoke testing.

use std::process::Command;

use super::OpenInTerminal;

pub struct WindowsTerminal {
    terminal: String,
}

impl WindowsTerminal {
    pub fn new(terminal: String) -> Self {
        Self { terminal }
    }
}

impl OpenInTerminal for WindowsTerminal {
    fn open(&self, cmd: &[&str]) -> anyhow::Result<()> {
        if cmd.is_empty() {
            anyhow::bail!("OpenInTerminal::open: empty cmd");
        }
        match self.terminal.as_str() {
            "default" | "wt" => run_wt(cmd),
            "conhost" => run_conhost(cmd),
            other => run_other(other, cmd),
        }
    }
}

fn run_wt(cmd: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("wt.exe").args(cmd).status()?;
    if !status.success() {
        anyhow::bail!("wt.exe failed with {status}");
    }
    Ok(())
}

fn run_conhost(cmd: &[&str]) -> anyhow::Result<()> {
    // `cmd /c start "<title>" prog [args...]` detaches prog in a new
    // conhost window. The quoted "title" is consumed by `start` as the
    // new window title — skipping it would make start treat prog as a
    // title instead. (Documented gotcha in cmd.exe.)
    let status = Command::new("cmd")
        .arg("/c")
        .arg("start")
        .arg("agend-terminal")
        .args(cmd)
        .status()?;
    if !status.success() {
        anyhow::bail!("cmd /c start failed with {status}");
    }
    Ok(())
}

/// PLAN contract: "other | treated as executable, invoked with `app` as
/// arg". The caller hands us `["agend-terminal", "app"]`; we invoke the
/// configured emulator with `cmd[1..]` (everything after the binary
/// name) as its argv. Power users who configure `alacritty.exe` etc.
/// will probably want `-e` style dispatch — revisit in task #4 after
/// real usage signal.
fn run_other(exe: &str, cmd: &[&str]) -> anyhow::Result<()> {
    let status = Command::new(exe).args(&cmd[1..]).status()?;
    if !status.success() {
        anyhow::bail!("{exe} failed with {status}");
    }
    Ok(())
}
