//! Embedded PNG icon assets for the tray.
//!
//! Placeholder — real icons (active / idle / error variants) are bundled
//! via `include_bytes!` from `assets/tray/` when PLAN task #4 lands.
//! `tray-icon` loads PNGs natively via `Icon::from_rgba`; do not pull the
//! `image` crate.
