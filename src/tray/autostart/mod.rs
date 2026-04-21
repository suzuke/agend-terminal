//! Launch-at-login persistence.
//!
//! Each platform writes its own entry pointing at
//! `std::env::current_exe()?.canonicalize()?` so `cargo install --force`
//! upgrades are picked up on next login with no re-configuration.
//! Implementations land with PLAN task #2.

pub trait Autostart {
    /// Install the platform-specific autostart entry (idempotent).
    fn enable(&self) -> anyhow::Result<()>;
    /// Remove the entry (idempotent — missing is success).
    fn disable(&self) -> anyhow::Result<()>;
    /// Query current state by inspecting the on-disk entry.
    fn is_enabled(&self) -> anyhow::Result<bool>;
}

// `Platform` is re-exported but not yet consumed — the tray event loop
// (PLAN task #4) is what calls `Platform::enable()` etc. Suppress the
// scaffold-window warning rather than gating the re-export itself.
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
#[allow(unused_imports)]
pub use macos::MacAutostart as Platform;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
#[allow(unused_imports)]
pub use linux::LinuxAutostart as Platform;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
#[allow(unused_imports)]
pub use windows::WindowsAutostart as Platform;
