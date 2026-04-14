//! Terminal application — multi-tab/pane TUI for agent management.

use crate::backend::Backend;
use crate::keybinds::{Action, KeyHandler};
use crate::layout::{Layout, Pane, SplitDir, Tab};
use crate::render;
use crate::state::StateTracker;
use crate::vterm::VTerm;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use ratatui::DefaultTerminal;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// An item in the new-tab selection menu.
pub struct MenuItem {
    pub label: String,
    pub kind: MenuItemKind,
}

pub enum MenuItemKind {
    Shell,
    Backend(Backend),
    FleetInstance(String),
}

/// UI overlay mode.
enum Overlay {
    None,
    NewTabMenu { items: Vec<MenuItem>, selected: usize },
    RenameTab { input: String },
    TabList { selected: usize },
    Help,
    /// Scroll mode: pane_id of the pane being scrolled, scroll offset (lines from bottom).
    Scroll { offset: usize },
}

/// Run the terminal application.
pub fn run() -> Result<()> {
    // Redirect tracing to a log file (stderr is owned by ratatui)
    let home = crate::home_dir();
    let log_path = home.join("app.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();
    if let Some(file) = log_file {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("agend_terminal=debug")
            .with_writer(Mutex::new(file))
            .with_ansi(false)
            .with_target(false)
            .try_init();
    }

    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal);
    ratatui::restore();
    result
}

fn run_app(terminal: &mut DefaultTerminal) -> Result<()> {
    let home = crate::home_dir();
    let mut layout = Layout::new();
    let mut key_handler = KeyHandler::new();
    let mut overlay = Overlay::None;
    let mut last_tab: usize = 0;

    // Wakeup channel: pane_id only (no data clone)
    let (wakeup_tx, wakeup_rx) = crossbeam::channel::unbounded::<usize>();

    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let pane_rows = rows.saturating_sub(4);
    let pane_cols = cols.saturating_sub(2);

    // Fleet auto-start is disabled for now to avoid conflicts with running daemons.
    // Use Ctrl+B c to manually add agents.
    let fleet_path = home.join("fleet.yaml");

    spawn_shell_tab(&mut layout, "shell", pane_cols, pane_rows, &wakeup_tx)?;

    // Crossterm event reader thread
    let (event_tx, event_rx) = crossbeam::channel::unbounded::<Event>();
    std::thread::Builder::new()
        .name("crossterm_events".into())
        .spawn(move || loop {
            if let Ok(ev) = event::read() {
                if event_tx.send(ev).is_err() {
                    break;
                }
            }
        })
        .ok();

    loop {
        let scroll_offset = match &overlay {
            Overlay::Scroll { offset } => *offset,
            _ => 0,
        };
        let repeat_mode = key_handler.in_repeat();

        terminal.draw(|frame| {
            render::render(frame, &mut layout, scroll_offset, repeat_mode);
            match &overlay {
                Overlay::NewTabMenu { items, selected } => {
                    render::render_menu(frame, items, *selected);
                }
                Overlay::RenameTab { input } => {
                    render::render_rename(frame, input);
                }
                Overlay::TabList { selected } => {
                    render::render_tab_list(frame, &layout, *selected);
                }
                Overlay::Help => {
                    render::render_help(frame);
                }
                Overlay::Scroll { offset } => {
                    render::render_scroll_indicator(frame, *offset);
                }
                Overlay::None => {}
            }
        })?;

        crossbeam::select! {
            recv(event_rx) -> ev => {
                let ev = match ev {
                    Ok(e) => e,
                    Err(_) => break,
                };
                match ev {
                    Event::Key(key) => {
                        // Overlay handles input first
                        if !matches!(overlay, Overlay::None) {
                            match &mut overlay {
                                Overlay::NewTabMenu { ref items, ref mut selected } => {
                                    match key.code {
                                        KeyCode::Up | KeyCode::Char('k') => {
                                            if *selected > 0 { *selected -= 1; }
                                        }
                                        KeyCode::Down | KeyCode::Char('j') => {
                                            if *selected + 1 < items.len() { *selected += 1; }
                                        }
                                        KeyCode::Enter => {
                                            let sel = *selected;
                                            let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
                                            let pr = r.saturating_sub(4);
                                            let pc = c.saturating_sub(2);
                                            if let Overlay::NewTabMenu { items, .. } = std::mem::replace(&mut overlay, Overlay::None) {
                                                if let Some(item) = items.into_iter().nth(sel) {
                                                    match item.kind {
                                                        MenuItemKind::Shell => {
                                                            let name = format!("shell-{}", layout.tabs.len());
                                                            let _ = spawn_shell_tab(&mut layout, &name, pc, pr, &wakeup_tx);
                                                        }
                                                        MenuItemKind::Backend(backend) => {
                                                            let preset = backend.preset();
                                                            let name = preset.command.to_string();
                                                            let args: Vec<String> = preset.args.iter().map(|s| s.to_string()).collect();
                                                            let _ = spawn_agent_tab(
                                                                &mut layout, &home, &name, preset.command,
                                                                &args, None, &HashMap::new(), pc, pr, &wakeup_tx,
                                                            );
                                                        }
                                                        MenuItemKind::FleetInstance(inst_name) => {
                                                            if let Ok(fleet) = crate::fleet::FleetConfig::load(&fleet_path) {
                                                                if let Some(resolved) = fleet.resolve_instance(&inst_name) {
                                                                    let _ = spawn_agent_tab(
                                                                        &mut layout, &home, &inst_name,
                                                                        &resolved.backend_command, &resolved.args,
                                                                        resolved.working_directory.as_deref(),
                                                                        &resolved.env, pc, pr, &wakeup_tx,
                                                                    );
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        KeyCode::Esc | KeyCode::Char('q') => { overlay = Overlay::None; }
                                        _ => {}
                                    }
                                }
                                Overlay::RenameTab { ref mut input } => {
                                    match key.code {
                                        KeyCode::Enter => {
                                            let new_name = input.clone();
                                            if !new_name.is_empty() {
                                                if let Some(tab) = layout.active_tab_mut() {
                                                    tab.name = new_name;
                                                }
                                            }
                                            overlay = Overlay::None;
                                        }
                                        KeyCode::Esc => { overlay = Overlay::None; }
                                        KeyCode::Backspace => { input.pop(); }
                                        KeyCode::Char(c) => { input.push(c); }
                                        _ => {}
                                    }
                                }
                                Overlay::TabList { ref mut selected } => {
                                    match key.code {
                                        KeyCode::Up | KeyCode::Char('k') => {
                                            if *selected > 0 { *selected -= 1; }
                                        }
                                        KeyCode::Down | KeyCode::Char('j') => {
                                            if *selected + 1 < layout.tabs.len() { *selected += 1; }
                                        }
                                        KeyCode::Enter => {
                                            layout.goto_tab(*selected);
                                            overlay = Overlay::None;
                                        }
                                        KeyCode::Esc | KeyCode::Char('q') => { overlay = Overlay::None; }
                                        _ => {}
                                    }
                                }
                                Overlay::Help => {
                                    // Any key dismisses help
                                    overlay = Overlay::None;
                                }
                                Overlay::Scroll { ref mut offset } => {
                                    let max = layout.active_tab()
                                        .and_then(|t| t.focused_pane())
                                        .map(|p| p.vterm.max_scroll())
                                        .unwrap_or(0);
                                    match key.code {
                                        KeyCode::Up | KeyCode::Char('k') => {
                                            *offset = (*offset + 1).min(max);
                                        }
                                        KeyCode::Down | KeyCode::Char('j') => {
                                            *offset = offset.saturating_sub(1);
                                        }
                                        KeyCode::PageUp => {
                                            *offset = (*offset + 20).min(max);
                                        }
                                        KeyCode::PageDown => {
                                            *offset = offset.saturating_sub(20);
                                        }
                                        KeyCode::Char('q') | KeyCode::Esc => {
                                            overlay = Overlay::None;
                                        }
                                        _ => {}
                                    }
                                }
                                Overlay::None => {}
                            }
                            continue;
                        }

                        let action = key_handler.handle(key);
                        match action {
                            Action::Forward(key) => {
                                if let Some(tab) = layout.active_tab() {
                                    if let Some(pane) = tab.focused_pane() {
                                        let bytes = crate::tui::key_to_bytes(key.code, key.modifiers);
                                        if !bytes.is_empty() {
                                            pane.write_to_pty(&bytes);
                                        }
                                    }
                                }
                            }
                            Action::NewTab => {
                                overlay = Overlay::NewTabMenu {
                                    items: build_menu_items(&fleet_path),
                                    selected: 0,
                                };
                            }
                            Action::NextTab => {
                                last_tab = layout.active;
                                layout.next_tab();
                            }
                            Action::PrevTab => {
                                last_tab = layout.active;
                                layout.prev_tab();
                            }
                            Action::LastTab => {
                                let current = layout.active;
                                layout.goto_tab(last_tab);
                                last_tab = current;
                            }
                            Action::GotoTab(idx) => {
                                last_tab = layout.active;
                                layout.goto_tab(idx);
                            }
                            Action::RenameTab => {
                                let current_name = layout.active_tab()
                                    .map(|t| t.name.clone())
                                    .unwrap_or_default();
                                overlay = Overlay::RenameTab { input: current_name };
                            }
                            Action::ListTabs => {
                                overlay = Overlay::TabList { selected: layout.active };
                            }
                            Action::SplitVertical => {
                                let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
                                let pr = r.saturating_sub(4);
                                let pc = c.saturating_sub(2) / 2;
                                let name = format!("split-{}", layout.tabs.len());
                                let pane_id = layout.next_pane_id();
                                if let Ok(pane) = create_shell_pane(&name, pc, pr, pane_id, &wakeup_tx) {
                                    if let Some(tab) = layout.active_tab_mut() {
                                        tab.split_focused(SplitDir::Vertical, pane);
                                    }
                                }
                            }
                            Action::SplitHorizontal => {
                                let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
                                let pr = r.saturating_sub(4) / 2;
                                let pc = c.saturating_sub(2);
                                let name = format!("split-{}", layout.tabs.len());
                                let pane_id = layout.next_pane_id();
                                if let Ok(pane) = create_shell_pane(&name, pc, pr, pane_id, &wakeup_tx) {
                                    if let Some(tab) = layout.active_tab_mut() {
                                        tab.split_focused(SplitDir::Horizontal, pane);
                                    }
                                }
                            }
                            Action::CycleFocus => {
                                if let Some(tab) = layout.active_tab_mut() {
                                    tab.cycle_focus();
                                }
                            }
                            Action::ClosePane => {
                                if let Some(tab) = layout.active_tab_mut() {
                                    tab.close_focused();
                                }
                            }
                            Action::CloseTab => {
                                if layout.tabs.len() > 1 {
                                    let idx = layout.active;
                                    if let Some(tab) = layout.close_tab(idx) {
                                        tab.kill_all();
                                    }
                                }
                            }
                            Action::FocusUp => {
                                if let Some(tab) = layout.active_tab_mut() {
                                    tab.focus_direction(crate::layout::Direction::Up);
                                }
                            }
                            Action::FocusDown => {
                                if let Some(tab) = layout.active_tab_mut() {
                                    tab.focus_direction(crate::layout::Direction::Down);
                                }
                            }
                            Action::FocusLeft => {
                                if let Some(tab) = layout.active_tab_mut() {
                                    tab.focus_direction(crate::layout::Direction::Left);
                                }
                            }
                            Action::FocusRight => {
                                if let Some(tab) = layout.active_tab_mut() {
                                    tab.focus_direction(crate::layout::Direction::Right);
                                }
                            }
                            Action::ScrollMode => {
                                overlay = Overlay::Scroll { offset: 0 };
                            }
                            Action::ShowHelp => {
                                overlay = Overlay::Help;
                            }
                            Action::Detach => break,
                            Action::ToggleZoom => {
                                if let Some(tab) = layout.active_tab_mut() {
                                    tab.zoomed = !tab.zoomed;
                                }
                            }
                            Action::None => {}
                        }
                    }
                    Event::Resize(_new_cols, _new_rows) => {
                        // ratatui re-layouts automatically; PTY resize happens
                        // via pane.resize() which should be called when we know
                        // the actual pane area. For now, rely on the VTerm being
                        // slightly larger than needed (output still works).
                        // TODO: propagate exact pane sizes from render layout.
                    }
                    _ => {}
                }
            }
            recv(wakeup_rx) -> _ => {
                // Wakeup signal from PTY output — just triggers redraw
            }
            default(std::time::Duration::from_millis(50)) => {
                // Periodic redraw for state updates
            }
        }
    }

    Ok(())
}

/// Build the menu items for the new-tab selection.
fn build_menu_items(fleet_path: &Path) -> Vec<MenuItem> {
    let mut items = Vec::new();

    if let Ok(fleet) = crate::fleet::FleetConfig::load(fleet_path) {
        let mut names = fleet.instance_names();
        names.sort();
        for name in names {
            let label = if let Some(resolved) = fleet.resolve_instance(&name) {
                format!("{name}  ({backend})", backend = resolved.backend_command)
            } else {
                name.clone()
            };
            items.push(MenuItem {
                label: format!("[fleet] {label}"),
                kind: MenuItemKind::FleetInstance(name),
            });
        }
    }

    for backend in Backend::all() {
        if backend.is_installed() {
            items.push(MenuItem {
                label: format!("[backend] {}", backend.name()),
                kind: MenuItemKind::Backend(backend.clone()),
            });
        }
    }

    items.push(MenuItem {
        label: "[shell] bash".to_string(),
        kind: MenuItemKind::Shell,
    });

    items
}

/// Spawn an agent and add as a new tab.
fn spawn_agent_tab(
    layout: &mut Layout,
    home: &Path,
    name: &str,
    command: &str,
    args: &[String],
    working_dir: Option<&Path>,
    env: &HashMap<String, String>,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
) -> Result<()> {
    let pane_id = layout.next_pane_id();
    let pane = create_agent_pane(home, name, command, args, working_dir, env, cols, rows, pane_id, wakeup_tx)?;
    layout.add_tab(Tab::new(name.to_string(), pane));
    Ok(())
}

/// Create a pane running an agent backend.
fn create_agent_pane(
    home: &Path,
    name: &str,
    command: &str,
    args: &[String],
    working_dir: Option<&Path>,
    env: &HashMap<String, String>,
    cols: u16,
    rows: u16,
    pane_id: usize,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
) -> Result<Pane> {
    let work_dir = match working_dir {
        Some(d) => d.to_path_buf(),
        None => home.join("workspace").join(name),
    };
    std::fs::create_dir_all(&work_dir).ok();

    crate::instructions::generate(&work_dir, command);

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows, cols, pixel_width: 0, pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(command);
    for arg in args {
        cmd.arg(arg);
    }

    // Claude-specific MCP flags
    if let Some(Backend::ClaudeCode) = Backend::from_command(command) {
        let mcp_config = work_dir.join("mcp-config.json");
        if mcp_config.exists() {
            cmd.arg("--mcp-config");
            cmd.arg(mcp_config.display().to_string());
        }
        let settings = work_dir.join("claude-settings.json");
        if settings.exists() {
            cmd.arg("--settings");
            cmd.arg(settings.display().to_string());
        }
    }

    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("FORCE_COLOR", "1");
    cmd.env("AGEND_INSTANCE_NAME", name);
    cmd.env("AGEND_HOME", home.as_os_str());
    for (k, v) in env {
        cmd.env(k, v);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}:{path}", dir.display()));
        }
    }
    cmd.cwd(&work_dir);

    let child = pair.slave.spawn_command(cmd)?;
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;

    let pty_writer: Arc<Mutex<Box<dyn std::io::Write + Send>>> = Arc::new(Mutex::new(writer));
    let pty_master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));
    let child_handle: Arc<Mutex<Box<dyn portable_pty::Child + Send>>> = Arc::new(Mutex::new(child));

    let (pane_tx, pane_rx) = crossbeam::channel::unbounded::<Vec<u8>>();
    let tx = wakeup_tx.clone();

    std::thread::Builder::new()
        .name(format!("{name}_pty_reader"))
        .spawn(move || {
            use std::io::Read;
            let mut reader = reader;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if pane_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                        let _ = tx.send(pane_id);
                    }
                }
            }
        })
        .ok();

    let backend = Backend::from_command(command);

    Ok(Pane {
        agent_name: name.to_string(),
        vterm: VTerm::new(cols, rows),
        rx: pane_rx,
        id: pane_id,
        backend: backend.clone(),
        state_tracker: StateTracker::new(backend.as_ref()),
        pty_writer,
        pty_master,
        child: child_handle,
    })
}

/// Spawn a shell and add as a new tab.
fn spawn_shell_tab(
    layout: &mut Layout,
    name: &str,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
) -> Result<()> {
    let pane_id = layout.next_pane_id();
    let pane = create_shell_pane(name, cols, rows, pane_id, wakeup_tx)?;
    layout.add_tab(Tab::new(name.to_string(), pane));
    Ok(())
}

/// Create a pane with a shell PTY.
fn create_shell_pane(
    name: &str,
    cols: u16,
    rows: u16,
    pane_id: usize,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
) -> Result<Pane> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows, cols, pixel_width: 0, pixel_height: 0,
    })?;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let mut cmd = CommandBuilder::new(&shell);
    cmd.env("TERM", "xterm-256color");

    let child = pair.slave.spawn_command(cmd)?;
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;

    let pty_writer: Arc<Mutex<Box<dyn std::io::Write + Send>>> = Arc::new(Mutex::new(writer));
    let pty_master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));
    let child_handle: Arc<Mutex<Box<dyn portable_pty::Child + Send>>> = Arc::new(Mutex::new(child));

    let (pane_tx, pane_rx) = crossbeam::channel::unbounded::<Vec<u8>>();
    let tx = wakeup_tx.clone();
    let pane_name = name.to_string();

    std::thread::Builder::new()
        .name(format!("{pane_name}_pty_reader"))
        .spawn(move || {
            use std::io::Read;
            let mut reader = reader;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if pane_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                        let _ = tx.send(pane_id);
                    }
                }
            }
        })
        .ok();

    Ok(Pane {
        agent_name: name.to_string(),
        vterm: VTerm::new(cols, rows),
        rx: pane_rx,
        id: pane_id,
        backend: None,
        state_tracker: StateTracker::new(None),
        pty_writer,
        pty_master,
        child: child_handle,
    })
}
