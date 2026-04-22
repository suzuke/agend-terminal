//! "Open App" terminal dispatcher.
//!
//! Given a command (`["agend-terminal", "app"]`), open it in the user's
//! configured terminal emulator. Resolution rules live in
//! `docs/archived/PLAN-tray-resident.md` §"OpenInTerminal per platform".
//!
//! Construct via `Platform::new(terminal)` where `terminal` is the
//! `terminal` field from `tray.toml` (`"default"` if unset).

pub trait OpenInTerminal {
    /// Launch `cmd` in a new terminal window/tab. `cmd[0]` is the
    /// executable, rest are args.
    fn open(&self, cmd: &[&str]) -> anyhow::Result<()>;
}

/// Spawn a launcher process and return immediately, detaching stdio.
///
/// Every platform impl funnels through this. Critical for Linux, where
/// `xterm` / `kitty` / `alacritty` / many `konsole` setups stay in the
/// foreground — calling `Command::status()` there would block the tray
/// event loop for the entire lifetime of the user's terminal window.
/// Windows bare-exe dispatch has the same shape and the same fix;
/// macOS's `open(1)` and `osascript` return quickly regardless, but
/// routing them through the same helper avoids future drift.
///
/// Post-fork failures (bad iTerm script, unknown `--args` to `open`,
/// etc.) are deliberately not surfaced — there is no error UI on the
/// tray yet. Pre-fork failures (binary not on PATH) still propagate
/// because `spawn()?` catches them at the execve boundary.
///
/// The returned `Child` is dropped, which on Unix leaves a zombie
/// until the tray process exits (or task #4 installs a SIGCHLD
/// reaper). For a menu click cadence that's a non-issue.
pub(super) fn spawn_detached(mut cmd: std::process::Command) -> anyhow::Result<()> {
    use std::process::Stdio;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

// `Platform` is re-exported but not yet consumed — the tray event loop
// (PLAN task #4) is what calls `Platform::open()`. Suppress the
// scaffold-window warning rather than gating the re-export itself.
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
#[allow(unused_imports)]
pub use macos::MacTerminal as Platform;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
#[allow(unused_imports)]
pub use linux::LinuxTerminal as Platform;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
#[allow(unused_imports)]
pub use windows::WindowsTerminal as Platform;
