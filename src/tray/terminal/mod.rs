//! "Open App" terminal dispatcher.
//!
//! Given a command (`["agend-terminal", "app"]`), open it in the user's
//! configured terminal emulator. Resolution rules live in
//! `docs/PLAN-tray-resident.md` §"OpenInTerminal per platform".
//! Implementations land with PLAN task #3.

pub trait OpenInTerminal {
    /// Launch `cmd` in a new terminal window/tab. `cmd[0]` is the
    /// executable, rest are args.
    fn open(&self, cmd: &[&str]) -> anyhow::Result<()>;
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
