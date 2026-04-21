//! Menu-bar / system-tray resident app.
//!
//! Gated behind the `tray` Cargo feature. See
//! `docs/PLAN-tray-resident.md` for the full design.

// Autostart / terminal modules are wired up incrementally across follow-on
// tasks; keep the scaffold exports alive without warnings in the meantime.
#![allow(dead_code)]

pub mod autostart;
pub mod config;
pub mod icon;
pub mod terminal;

use std::path::{Path, PathBuf};

use serde_json::json;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
#[cfg(target_os = "macos")]
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};

use crate::{api, bootstrap::daemon_spawn};

/// Events forwarded from tray-icon's global callbacks into the tao
/// event loop so the loop wakes on every menu click.
#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
}

/// Build a 32x32 solid-color placeholder icon so the spike runs without
/// bundled PNG assets. Real icon variants (active / idle / error) land
/// with follow-on polish — `Icon::from_rgba` accepts any RGBA buffer
/// that satisfies `width * height * 4 == bytes.len()`.
fn placeholder_icon() -> Icon {
    const W: u32 = 32;
    const H: u32 = 32;
    let mut rgba = Vec::with_capacity((W * H * 4) as usize);
    for _ in 0..(W * H) {
        // Brand-ish green. Swap for real asset later.
        rgba.extend_from_slice(&[0x3A, 0xA8, 0x55, 0xFF]);
    }
    Icon::from_rgba(rgba, W, H).expect("32x32 RGBA buffer is always valid")
}

/// Probe the daemon via `api::call(LIST)`; if it's not up, spawn a
/// detached one (blocks up to 5s for readiness). Tray stays usable
/// even if spawn fails — the user can still Quit.
fn bootstrap_daemon(home: &Path) {
    if api::call(home, &json!({"method": api::method::LIST})).is_ok() {
        // Adopted running daemon.
        return;
    }
    if let Err(e) = daemon_spawn::spawn_detached(home, None) {
        // Non-fatal: tray still starts so the user can Quit / inspect.
        // Without a status menu yet (lands in follow-on), surface the
        // failure on stderr — `agend-terminal tray` is usually run in a
        // terminal during the MVP phase.
        eprintln!("tray: daemon spawn failed: {e}");
    }
}

/// Tell the daemon to shut down. Best-effort — if it's already gone,
/// the RPC fails silently. Called from the Quit menu handler.
fn shutdown_daemon(home: &Path) {
    let _ = api::call(home, &json!({"method": api::method::SHUTDOWN}));
}

/// Entry point for `agend-terminal tray`.
///
/// Probes/spawns the daemon, brings up the tray icon, and runs the
/// event loop. Quit sends SHUTDOWN before exiting. Status polling,
/// Open App, and Launch-at-login toggle land in follow-on commits.
///
/// The `unused_*` allows cover the `tray_icon` ownership slot inside
/// the event loop: dropping a `TrayIcon` removes the icon from the
/// system bar, so the slot must live for the lifetime of the loop —
/// but nothing ever reads it back.
#[allow(unused_assignments, unused_variables)]
pub fn run(home: &Path) -> anyhow::Result<()> {
    bootstrap_daemon(home);
    let home: PathBuf = home.to_path_buf();

    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    event_loop.set_activation_policy(ActivationPolicy::Accessory);

    // Forward menu events into the event loop so it wakes on clicks.
    // tray-icon uses a global crossbeam channel internally; without
    // this bridge the loop would sleep forever in `ControlFlow::Wait`.
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(UserEvent::Menu(event));
    }));

    // Menu: just "Quit" for the spike.
    let menu = Menu::new();
    let quit_item = MenuItem::new("Quit agend-terminal", true, None);
    menu.append(&quit_item)?;
    let quit_id = quit_item.id().clone();

    // tray-icon requires creation AFTER the event loop starts on macOS
    // (prevents fullscreen-app issues, per crate docs). Build inside
    // `StartCause::Init` rather than before `run()`.
    let mut tray_icon: Option<TrayIcon> = None;
    let mut menu_slot: Option<Menu> = Some(menu);

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                if let Some(menu) = menu_slot.take() {
                    match TrayIconBuilder::new()
                        .with_tooltip("agend-terminal")
                        .with_icon(placeholder_icon())
                        .with_menu(Box::new(menu))
                        .build()
                    {
                        Ok(t) => tray_icon = Some(t),
                        Err(e) => {
                            eprintln!("tray: failed to build icon: {e}");
                            *control_flow = ControlFlow::Exit;
                        }
                    }
                }
            }
            Event::UserEvent(UserEvent::Menu(ev)) if ev.id == quit_id => {
                shutdown_daemon(&home);
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}
