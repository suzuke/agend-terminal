//! Menu-bar / system-tray resident app.
//!
//! Gated behind the `tray` Cargo feature. Scaffold only — this module
//! currently provides the directory layout, the `Autostart` and
//! `OpenInTerminal` platform traits, and the `agend-terminal tray`
//! subcommand entry point. The event loop, icon loading, and daemon
//! lifecycle land in PLAN task #4 (see `docs/PLAN-tray-resident.md`).

// Scaffold: most items won't be called until later PLAN tasks land.
#![allow(dead_code)]

pub mod autostart;
pub mod config;
pub mod icon;
pub mod terminal;

use std::path::Path;

/// Entry point for `agend-terminal tray`.
///
/// Scaffold stub: emits a banner and exits 0 so the subcommand is wired
/// end-to-end. The real event loop (tray icon + menu + daemon lifecycle)
/// lands with PLAN task #4.
pub fn run(_home: &Path) -> anyhow::Result<()> {
    eprintln!(
        "agend-terminal tray: scaffold only — event loop lands with \
         PLAN task #4 (docs/PLAN-tray-resident.md)"
    );
    Ok(())
}
