//! Menu-bar / system-tray resident app.
//!
//! Gated behind the `tray` Cargo feature. See
//! `docs/archived/PLAN-tray-resident.md` for the full design.

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
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};

use crate::api;

use self::autostart::{Autostart, Platform as AutostartPlatform};
use self::terminal::{OpenInTerminal, Platform as TerminalPlatform};

/// Status polling cadence. PLAN §"Runtime flow" pins 2s; slower feels
/// stale, faster is just wasted IPC.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Events forwarded from tray-icon's global callbacks and the status
/// poller into the tao event loop so the loop wakes on every input.
#[derive(Debug)]
enum UserEvent {
    Menu(MenuEvent),
    /// Distilled daemon status from the worker thread. Drained on the
    /// main thread (tray-icon / muda require main-thread mutation on
    /// macOS); the handler derives label text + icon color from the
    /// same variant so they can never desync.
    Status(StatusKind),
}

/// The three dial positions the tray icon can show. PLAN §"Layout"
/// calls for `active / idle / error`; we reuse the same split but
/// rename `error` → `offline` because the only actual failure mode
/// today is an unreachable daemon. Real unhealthy-agent signal would
/// need a sick state on the agent handle, which the current registry
/// doesn't carry.
#[derive(Debug, Clone, Copy)]
enum StatusKind {
    /// Daemon probe failed — no tray ↔ daemon IPC at all.
    Offline,
    /// Daemon up, zero agents registered. Rare outside a fresh
    /// `$AGEND_HOME`.
    Idle,
    /// Daemon up with `N >= 1` agents in the registry.
    Active(usize),
}

impl StatusKind {
    /// Classify a LIST response. Missing / non-array `result.agents`
    /// means the response shape drifted — treat as offline rather
    /// than silently showing `Idle` with a misleading green icon.
    fn from_response(resp: &Value) -> Self {
        match resp
            .get("result")
            .and_then(|r| r.get("agents"))
            .and_then(|a| a.as_array())
        {
            None => Self::Offline,
            Some(arr) if arr.is_empty() => Self::Idle,
            Some(arr) => Self::Active(arr.len()),
        }
    }

    /// Disabled menu-item text for the status row.
    fn label(&self) -> String {
        match *self {
            Self::Offline => "daemon offline".into(),
            Self::Idle => "no agents".into(),
            Self::Active(1) => "1 agent".into(),
            Self::Active(n) => format!("{n} agents"),
        }
    }

    /// 32x32 solid-color icon. Placeholder until designed PNG assets
    /// are bundled — still useful for dogfooding because the color
    /// alone is a glanceable daemon-up/down signal without opening
    /// the menu. Colors picked for visibility in both light and dark
    /// menu bars.
    fn icon(&self) -> Icon {
        let color = match *self {
            Self::Offline => [0x88, 0x88, 0x88, 0xFF], // neutral gray
            Self::Idle => [0xD8, 0xAA, 0x3A, 0xFF],    // amber
            Self::Active(_) => [0x3A, 0xA8, 0x55, 0xFF], // brand green
        };
        solid_icon(color)
    }
}

/// Fill an RGBA buffer with one color and wrap it in an `Icon`.
/// Factored out so `StatusKind::icon` stays a table of colors.
fn solid_icon(rgba: [u8; 4]) -> Icon {
    const W: u32 = 32;
    const H: u32 = 32;
    let mut buf = Vec::with_capacity((W * H * 4) as usize);
    for _ in 0..(W * H) {
        buf.extend_from_slice(&rgba);
    }
    Icon::from_rgba(buf, W, H).expect("32x32 RGBA buffer is always valid")
}

/// Sprint 57 Wave 3 PR-2 (#548 Q7) check_daemon_state: probe the
/// daemon via `api::call(LIST)` and report Online vs Offline. The
/// tray no longer spawns the daemon directly — daemon spawn is
/// consolidated to CLI `start` per Q7. When the tray detects
/// Offline, the menu surfaces a "Start daemon" item that shells
/// out to `agend-terminal start` (which itself owns the
/// detached-default semantics from Q1).
fn check_daemon_state(home: &Path) -> StatusKind {
    match api::call(home, &json!({"method": api::method::LIST})) {
        Ok(resp) => StatusKind::from_response(&resp),
        Err(_) => StatusKind::Offline,
    }
}

/// Sprint 57 Wave 3 PR-2 (#548 Q7) tray "Start daemon" menu action:
/// shell out to `agend-terminal start` instead of spawning the
/// daemon directly. The CLI `start` command's default is detached
/// service mode (Q1 default-flip), so the spawned process becomes
/// a free-standing daemon and the tray's launch shell exits
/// immediately. Failures surface to stderr — the tray stays usable
/// so the operator can retry / inspect / Quit.
fn start_daemon_via_cli() {
    // #548 Q7: tray shells out via `current_exe()`-resolved binary path
    // to invoke `agend-terminal start`. The separation contract pinned by
    // `tests/issue_548_phase2_invariants` — tray must NOT import the
    // daemon-spawn module's helpers; it must build the Command itself and
    // invoke `start` via CLI. Preserved exactly below.
    //
    // #879v3 supersession at the SPEC level (NOT call-site level): tray
    // sources args + env from the canonical recursion-guard module so the
    // AGEND_SPAWN_DEPTH increment lands on the child env and the args
    // shape stays in lock-step with the CLI Start + app auto-spawn paths.
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("tray: failed to resolve current_exe for daemon start: {e}");
            return;
        }
    };
    let spec = match crate::bootstrap::spawn_depth::canonical_spawn_args(None) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tray: AGEND_SPAWN_DEPTH guard refused tray-side daemon spawn: {e}");
            return;
        }
    };
    let mut cmd = std::process::Command::new(&exe);
    // Apply the canonical spec: `start --foreground` args + AGEND_SPAWN_DEPTH
    // env. The literal `.arg("start")` shape is pinned by #548 Q7's
    // `tray_menu_start_command_shells_out_to_cli` invariant; the canonical
    // spec asserts the same string via `canonical_spawn_args_includes_start_and_foreground`.
    cmd.arg("start");
    cmd.args(&spec.args[1..]);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    crate::bootstrap::spawn_depth::apply_detach_flags(&mut cmd);
    match cmd.spawn() {
        Ok(_child) => {
            tracing::info!(
                exe = %exe.display(),
                "tray: shelled out to `agend-terminal start` for daemon launch"
            );
        }
        Err(e) => {
            eprintln!("tray: `{} start` spawn failed: {e}", exe.display());
        }
    }
}

/// Tell the daemon to shut down. Best-effort — if it's already gone,
/// the RPC fails silently. Called from the Quit menu handler.
fn shutdown_daemon(home: &Path) {
    let _ = api::call(home, &json!({"method": api::method::SHUTDOWN}));
}

/// Entry point for `agend-terminal tray`.
///
/// Probes/spawns the daemon, brings up the tray icon with status
/// label / Open App / Launch-at-login / Quit, and runs the event
/// loop. Quit sends SHUTDOWN before exiting. Status is refreshed
/// every `POLL_INTERVAL` from a worker thread.
///
/// The `unused_*` allows cover the `tray_icon` ownership slot inside
/// the event loop: dropping a `TrayIcon` removes the icon from the
/// system bar, so the slot must live for the lifetime of the loop —
/// but nothing ever reads it back.
#[allow(unused_assignments, unused_variables)]
pub fn run(home: &Path) -> anyhow::Result<()> {
    // Sprint 57 Wave 3 PR-2 (#548 Q7): tray no longer spawns the
    // daemon on startup. Probe via `check_daemon_state`; if Offline,
    // surface a "Start daemon" menu item that shells out to CLI
    // `start` (consolidates spawn entry to CLI per Q7).
    let initial_state = check_daemon_state(home);
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

    // Autostart platform instance + current on-disk state. Probe
    // failures (e.g. launchctl not in PATH on a minimal runner) are
    // non-fatal: treat as disabled and keep the tray usable.
    let autostart = AutostartPlatform::new(home.clone());
    let autostart_on = autostart.is_enabled().unwrap_or_else(|e| {
        eprintln!("tray: failed to probe autostart state: {e}");
        false
    });

    // Menu: disabled status label, separator, Start daemon (Wave 3
    // PR-2 #548 Q7), Open App, Launch-at-login toggle, Quit.
    //
    // The "Start daemon" item is always present; it shells out to
    // `agend-terminal start` and lets that command's detached-default
    // semantics (Q1) own the actual lifecycle. When the daemon is
    // already up the click is harmless — the CLI's own discovery
    // prevents double-start. Tray's role is reduced to status widget +
    // GUI launcher per Q7.
    let menu = Menu::new();
    let status_item = MenuItem::new(initial_state.label(), false, None);
    let start_daemon_item = MenuItem::new("Start daemon", true, None);
    let open_app_item = MenuItem::new("Open App", true, None);
    let autostart_item = CheckMenuItem::new("Launch at login", true, autostart_on, None);
    let quit_item = MenuItem::new("Quit agend-terminal", true, None);
    menu.append(&status_item)?;
    menu.append(&tray_icon::menu::PredefinedMenuItem::separator())?;
    menu.append(&start_daemon_item)?;
    menu.append(&open_app_item)?;
    menu.append(&autostart_item)?;
    menu.append(&quit_item)?;
    let start_daemon_id = start_daemon_item.id().clone();
    let open_app_id = open_app_item.id().clone();
    let autostart_id = autostart_item.id().clone();
    let quit_id = quit_item.id().clone();

    // Status poller: every POLL_INTERVAL, probe the daemon and push a
    // pre-formatted label through the event loop. Runs on a worker
    // thread because tray-icon / muda menu mutation must stay on the
    // main thread on macOS. The loop exits when `send_event` fails —
    // which happens once the event loop has shut down (`Quit` clicked).
    let poll_proxy = event_loop.create_proxy();
    let poll_home = home.clone();
    // fire-and-forget: tray status poller; loop exits on `send_event` Err
    // when the tao event loop has been closed (Quit clicked → process exit
    // path). No graceful join because tao itself drives shutdown ordering.
    thread::spawn(move || loop {
        let kind = match api::call(&poll_home, &json!({"method": api::method::LIST})) {
            Ok(resp) => StatusKind::from_response(&resp),
            Err(_) => StatusKind::Offline,
        };
        if poll_proxy.send_event(UserEvent::Status(kind)).is_err() {
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
                    // Boot color is `Offline`'s gray. The poller replaces
                    // it within `POLL_INTERVAL`; this just avoids leaking
                    // an always-green look during the first 2s when the
                    // daemon may or may not be up.
                    match TrayIconBuilder::new()
                        .with_tooltip("agend-terminal")
                        .with_icon(StatusKind::Offline.icon())
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
                } else if ev.id == start_daemon_id {
                    // Sprint 57 Wave 3 PR-2 (#548 Q7): shell out to
                    // CLI `start`. The CLI's detached-default
                    // semantics (Q1) own the actual lifecycle —
                    // tray's job here ends as soon as the spawn
                    // returns. The status poller picks up the new
                    // daemon on the next POLL_INTERVAL tick.
                    start_daemon_via_cli();
                } else if ev.id == open_app_id {
                    // Best-effort: surface PATH / spawn errors on stderr but
                    // keep the tray alive so the user can retry or Quit.
                    if let Err(e) = opener.open(&["agend-terminal", "app"]) {
                        eprintln!("tray: open app failed: {e}");
                    }
                } else if ev.id == autostart_id {
                    // CheckMenuItem flips its own check state before firing
                    // the event; read that to learn the user's intent, then
                    // persist via the Autostart trait. On failure, revert the
                    // visual to the real on-disk state so the menu never
                    // shows a lie.
                    let desired = autostart_item.is_checked();
                    let result = if desired {
                        autostart.enable()
                    } else {
                        autostart.disable()
                    };
                    if let Err(e) = result {
                        eprintln!("tray: autostart toggle failed: {e}");
                        let actual = autostart.is_enabled().unwrap_or(!desired);
                        autostart_item.set_checked(actual);
                    }
                }
            }
            Event::UserEvent(UserEvent::Status(kind)) => {
                status_item.set_text(kind.label());
                if let Some(tray) = tray_icon.as_ref() {
                    // `set_icon(None)` would remove the tray slot entirely
                    // on Linux; always pass `Some(...)` when swapping.
                    if let Err(e) = tray.set_icon(Some(kind.icon())) {
                        eprintln!("tray: failed to swap icon: {e}");
                    }
                }
            }
            _ => {}
        }
    });
}
