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
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
#[cfg(target_os = "macos")]
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};

use crate::{api, bootstrap::daemon_spawn};

use self::terminal::{OpenInTerminal, Platform as TerminalPlatform};

/// Status polling cadence. PLAN §"Runtime flow" pins 2s; slower feels
/// stale, faster is just wasted IPC.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Events forwarded from tray-icon's global callbacks and the status
/// poller into the tao event loop so the loop wakes on every input.
#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
    /// Pre-formatted label text from the worker thread. Drained on the
    /// main thread (tray-icon / muda require main-thread mutation on
    /// macOS).
    Status(String),
}

/// Render a LIST response into the menu label. First line is what the
/// user scans: how many agents the daemon has, or "daemon offline" if
/// the probe errors.
fn format_status(resp: &Value) -> String {
    let Some(agents) = resp
        .get("result")
        .and_then(|r| r.get("agents"))
        .and_then(|a| a.as_array())
    else {
        return "daemon offline".to_string();
    };
    match agents.len() {
        0 => "no agents".to_string(),
        1 => "1 agent".to_string(),
        n => format!("{n} agents"),
    }
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

    // tray.toml is best-effort — absence / parse errors fall back to
    // defaults (warn-and-default per PLAN). Build the terminal
    // dispatcher from its `terminal` field so "Open App" clicks route
    // through the user's chosen emulator.
    let cfg = config::load(&home);
    let opener = TerminalPlatform::new(cfg.terminal);

    // Menu: disabled status label, separator, Open App, Quit.
    // Launch-at-login toggle lands in the follow-on commit.
    let menu = Menu::new();
    let status_item = MenuItem::new("starting…", false, None);
    let open_app_item = MenuItem::new("Open App", true, None);
    let quit_item = MenuItem::new("Quit agend-terminal", true, None);
    menu.append(&status_item)?;
    menu.append(&tray_icon::menu::PredefinedMenuItem::separator())?;
    menu.append(&open_app_item)?;
    menu.append(&quit_item)?;
    let open_app_id = open_app_item.id().clone();
    let quit_id = quit_item.id().clone();

    // Status poller: every POLL_INTERVAL, probe the daemon and push a
    // pre-formatted label through the event loop. Runs on a worker
    // thread because tray-icon / muda menu mutation must stay on the
    // main thread on macOS. The loop exits when `send_event` fails —
    // which happens once the event loop has shut down (`Quit` clicked).
    let poll_proxy = event_loop.create_proxy();
    let poll_home = home.clone();
    thread::spawn(move || loop {
        let text = match api::call(&poll_home, &json!({"method": api::method::LIST})) {
            Ok(resp) => format_status(&resp),
            Err(_) => "daemon offline".to_string(),
        };
        if poll_proxy.send_event(UserEvent::Status(text)).is_err() {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    });

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
            Event::UserEvent(UserEvent::Menu(ev)) => {
                if ev.id == quit_id {
                    shutdown_daemon(&home);
                    *control_flow = ControlFlow::Exit;
                } else if ev.id == open_app_id {
                    // Best-effort: surface PATH / spawn errors on stderr but
                    // keep the tray alive so the user can retry or Quit.
                    if let Err(e) = opener.open(&["agend-terminal", "app"]) {
                        eprintln!("tray: open app failed: {e}");
                    }
                }
            }
            Event::UserEvent(UserEvent::Status(text)) => {
                status_item.set_text(text);
            }
            _ => {}
        }
    });
}
