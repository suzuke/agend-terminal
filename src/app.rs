//! Terminal application — multi-tab/pane TUI for agent management.
//!
//! Uses agent::spawn_agent() for all panes (agents and shells), sharing the
//! same PTY lifecycle as the daemon: auto-dismiss, state tracking, broadcast.

use crate::agent::{self, AgentRegistry};
use crate::backend::Backend;
use crate::keybinds::{Action, KeyHandler};
use crate::layout::{Layout, Pane, SplitDir, Tab};
use crate::render;
use crate::vterm::VTerm;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, MouseEventKind};
use ratatui::DefaultTerminal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Saved session layout for persistence across restarts.
#[derive(Serialize, Deserialize)]
struct Session {
    tabs: Vec<SessionTab>,
    active_tab: usize,
}

#[derive(Serialize, Deserialize)]
struct SessionTab {
    name: String,
    root: SessionNode,
}

#[derive(Serialize, Deserialize)]
enum SessionNode {
    Leaf(SessionPane),
    Split {
        dir: SplitDir,
        first: Box<SessionNode>,
        second: Box<SessionNode>,
    },
}

#[derive(Serialize, Deserialize)]
struct SessionPane {
    agent_name: String,
    backend_command: String,
    working_dir: Option<String>,
    submit_key: String,
    display_name: Option<String>,
}

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

enum CloseTarget {
    Pane,
    Tab,
}

enum Overlay {
    None,
    /// New tab selection menu.
    NewTabMenu { items: Vec<MenuItem>, selected: usize },
    /// Split pane selection menu — choose what to run in the new pane.
    SplitMenu { items: Vec<MenuItem>, selected: usize, dir: SplitDir },
    RenameTab { input: String },
    RenamePane { input: String },
    ConfirmClose { target: CloseTarget },
    TabList { selected: usize },
    Help,
    /// Keyboard scroll mode (j/k/PgUp/PgDn). Pane's scroll_offset is used directly.
    Scroll,
}

/// Run the terminal application.
pub fn run(fleet_path_override: Option<&str>) -> Result<()> {
    // Redirect tracing to log file BEFORE ratatui takes over stderr.
    // Must happen before main.rs's tracing init — caller should skip init for App.
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

    let fleet_path = fleet_path_override.map(PathBuf::from);

    // Enable mouse support for scroll
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture).ok();

    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal, fleet_path.as_deref());
    ratatui::restore();

    crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture).ok();
    result
}

fn run_app(terminal: &mut DefaultTerminal, fleet_override: Option<&Path>) -> Result<()> {
    let home = crate::home_dir();

    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));

    let _api_guard = start_api_server(&home, &registry);

    let mut layout = Layout::new();
    let mut key_handler = KeyHandler::new();
    let mut overlay = Overlay::None;
    let mut last_tab: usize = 0;
    // Counter for auto-dedup agent names
    let mut name_counter: HashMap<String, usize> = HashMap::new();

    let (wakeup_tx, wakeup_rx) = crossbeam::channel::unbounded::<usize>();

    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let pane_rows = rows.saturating_sub(4);
    let pane_cols = cols.saturating_sub(2);

    let fleet_path = fleet_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| home.join("fleet.yaml"));

    // Try to restore saved session, otherwise start with a shell tab
    let restored = restore_session(
        &home, &mut layout, &registry, &wakeup_tx, &mut name_counter,
        pane_cols, pane_rows,
    );
    if !restored {
        spawn_pane_tab(
        &mut layout,
        &registry,
        &home,
        "shell",
        &std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string()),
        &[],
        None,
        &HashMap::new(),
        "\r",
        pane_cols,
        pane_rows,
        &wakeup_tx,
        &mut name_counter,
    )?;
    }

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
        let repeat_mode = key_handler.in_repeat();

        terminal.draw(|frame| {
            render::render(frame, &mut layout, repeat_mode, &registry);
            match &overlay {
                Overlay::NewTabMenu { items, selected }
                | Overlay::SplitMenu { items, selected, .. } => {
                    render::render_menu(frame, items, *selected);
                }
                Overlay::RenameTab { input } | Overlay::RenamePane { input } => {
                    render::render_rename(frame, input);
                }
                Overlay::ConfirmClose { target } => {
                    let msg = match target {
                        CloseTarget::Pane => "Close pane? (y/n)",
                        CloseTarget::Tab => "Close tab and kill all agents? (y/n)",
                    };
                    render::render_confirm(frame, msg);
                }
                Overlay::TabList { selected } => {
                    render::render_tab_list(frame, &layout, *selected);
                }
                Overlay::Help => {
                    render::render_help(frame);
                }
                Overlay::Scroll => {
                    let so = layout.active_tab()
                        .and_then(|t| t.focused_pane())
                        .map(|p| p.scroll_offset)
                        .unwrap_or(0);
                    render::render_scroll_indicator(frame, so);
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
                        // Overlay input handling
                        if !matches!(overlay, Overlay::None) {
                            match &mut overlay {
                                Overlay::NewTabMenu { ref items, ref mut selected } => {
                                    match key.code {
                                        KeyCode::Up | KeyCode::Char('k') => { if *selected > 0 { *selected -= 1; } }
                                        KeyCode::Down | KeyCode::Char('j') => { if *selected + 1 < items.len() { *selected += 1; } }
                                        KeyCode::Enter => {
                                            let sel = *selected;
                                            let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
                                            if let Overlay::NewTabMenu { items, .. } = std::mem::replace(&mut overlay, Overlay::None) {
                                                if let Some(item) = items.into_iter().nth(sel) {
                                                    let pc = c.saturating_sub(2);
                                                    let pr = r.saturating_sub(4);
                                                    if let Ok(pane) = pane_from_menu_item(
                                                        item, &fleet_path, &mut layout, &registry, &home,
                                                        pc, pr, &wakeup_tx, &mut name_counter,
                                                    ) {
                                                        let tab_name = pane.agent_name.clone();
                                                        layout.add_tab(Tab::new(tab_name, pane));
                                                    }
                                                }
                                            }
                                        }
                                        KeyCode::Esc | KeyCode::Char('q') => { overlay = Overlay::None; }
                                        _ => {}
                                    }
                                }
                                Overlay::SplitMenu { ref items, ref mut selected, dir } => {
                                    let split_dir = *dir;
                                    match key.code {
                                        KeyCode::Up | KeyCode::Char('k') => { if *selected > 0 { *selected -= 1; } }
                                        KeyCode::Down | KeyCode::Char('j') => { if *selected + 1 < items.len() { *selected += 1; } }
                                        KeyCode::Enter => {
                                            let sel = *selected;
                                            let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
                                            if let Overlay::SplitMenu { items, .. } = std::mem::replace(&mut overlay, Overlay::None) {
                                                if let Some(item) = items.into_iter().nth(sel) {
                                                    let (pc, pr) = match split_dir {
                                                        SplitDir::Vertical => (c.saturating_sub(2) / 2, r.saturating_sub(4)),
                                                        SplitDir::Horizontal => (c.saturating_sub(2), r.saturating_sub(4) / 2),
                                                    };
                                                    match pane_from_menu_item(
                                                        item, &fleet_path, &mut layout, &registry, &home,
                                                        pc, pr, &wakeup_tx, &mut name_counter,
                                                    ) {
                                                        Ok(p) => {
                                                            if let Some(tab) = layout.active_tab_mut() {
                                                                tab.split_focused(split_dir, p);
                                                            }
                                                        }
                                                        Err(e) => tracing::error!(error = %e, "split failed"),
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
                                Overlay::RenamePane { ref mut input } => {
                                    match key.code {
                                        KeyCode::Enter => {
                                            let new_name = input.clone();
                                            if let Some(tab) = layout.active_tab_mut() {
                                                let fid = tab.focus_id;
                                                if let Some(pane) = tab.root_mut().find_pane_mut(fid) {
                                                    pane.display_name = if new_name.is_empty() {
                                                        None // clear → revert to agent_name
                                                    } else {
                                                        Some(new_name)
                                                    };
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
                                Overlay::ConfirmClose { ref target } => {
                                    match key.code {
                                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                                            let is_tab = matches!(target, CloseTarget::Tab);
                                            overlay = Overlay::None;
                                            if is_tab {
                                                if layout.tabs.len() > 1 {
                                                    let idx = layout.active;
                                                    if let Some(tab) = layout.close_tab(idx) {
                                                        for name in tab.root().agent_names() {
                                                            kill_agent(&registry, &name);
                                                        }
                                                    }
                                                }
                                            } else if let Some(tab) = layout.active_tab_mut() {
                                                if let Some(name) = tab.close_focused() {
                                                    kill_agent(&registry, &name);
                                                }
                                            }
                                        }
                                        _ => { overlay = Overlay::None; }
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
                                Overlay::Help => { overlay = Overlay::None; }
                                Overlay::Scroll => {
                                    match key.code {
                                        KeyCode::Up | KeyCode::Char('k') => scroll_focused(&mut layout, 1),
                                        KeyCode::Down | KeyCode::Char('j') => scroll_focused(&mut layout, -1),
                                        KeyCode::PageUp => scroll_focused(&mut layout, 20),
                                        KeyCode::PageDown => scroll_focused(&mut layout, -20),
                                        KeyCode::Char('q') | KeyCode::Esc => { overlay = Overlay::None; }
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
                                // Forward key to focused pane's PTY via registry
                                if let Some(name) = layout.active_tab()
                                    .and_then(|t| t.focused_pane())
                                    .map(|p| p.agent_name.clone())
                                {
                                    let bytes = crate::tui::key_to_bytes(key.code, key.modifiers);
                                    if !bytes.is_empty() {
                                        let reg = agent::lock_registry(&registry);
                                        if let Some(handle) = reg.get(&name) {
                                            let _ = agent::write_to_agent(handle, &bytes);
                                        }
                                    }
                                }
                            }
                            Action::NewTab => {
                                overlay = Overlay::NewTabMenu {
                                    items: build_menu_items(&fleet_path, &registry),
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
                            Action::RenamePane => {
                                let current = layout.active_tab()
                                    .and_then(|t| t.focused_pane())
                                    .map(|p| p.label().to_string())
                                    .unwrap_or_default();
                                overlay = Overlay::RenamePane { input: current };
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
                                overlay = Overlay::SplitMenu {
                                    items: build_menu_items(&fleet_path, &registry),
                                    selected: 0,
                                    dir: SplitDir::Vertical,
                                };
                            }
                            Action::SplitHorizontal => {
                                overlay = Overlay::SplitMenu {
                                    items: build_menu_items(&fleet_path, &registry),
                                    selected: 0,
                                    dir: SplitDir::Horizontal,
                                };
                            }
                            Action::CycleFocus => {
                                if let Some(tab) = layout.active_tab_mut() {
                                    tab.cycle_focus();
                                }
                            }
                            Action::ClosePane => {
                                overlay = Overlay::ConfirmClose { target: CloseTarget::Pane };
                            }
                            Action::CloseTab => {
                                if layout.tabs.len() > 1 {
                                    overlay = Overlay::ConfirmClose { target: CloseTarget::Tab };
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
                                overlay = Overlay::Scroll;
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
                    Event::Mouse(mouse) => {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => scroll_focused(&mut layout, 3),
                            MouseEventKind::ScrollDown => scroll_focused(&mut layout, -3),
                            _ => {}
                        }
                    }
                    Event::Resize(_cols, _rows) => {
                        // PTY + VTerm resize handled in render_node
                    }
                    _ => {}
                }
            }
            recv(wakeup_rx) -> _ => {
                // Wakeup from PTY output — triggers redraw
            }
            default(std::time::Duration::from_millis(50)) => {
                // Periodic redraw for state updates
            }
        }
    }

    // Save session before cleanup
    save_session(&home, &layout, &registry);

    // Cleanup: kill all agents
    for tab in &layout.tabs {
        for name in tab.root().agent_names() {
            kill_agent(&registry, &name);
        }
    }

    Ok(())
}

/// Build menu items for new-tab selection.
/// Fleet instances already running in the registry are excluded.
fn build_menu_items(fleet_path: &Path, registry: &AgentRegistry) -> Vec<MenuItem> {
    let mut items = Vec::new();

    // Collect already-running agent names
    let running: Vec<String> = {
        let reg = agent::lock_registry(registry);
        reg.keys().cloned().collect()
    };

    if let Ok(fleet) = crate::fleet::FleetConfig::load(fleet_path) {
        let mut names = fleet.instance_names();
        names.sort();
        for name in names {
            // Skip if exact name or deduped variant (name-1, name-2...) is running
            let already_open = running.iter().any(|r| {
                r == &name || r.starts_with(&format!("{name}-"))
            });
            if already_open {
                continue;
            }
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

/// Create a pane from a menu item selection (shared by NewTab and Split handlers).
#[allow(clippy::too_many_arguments)]
fn pane_from_menu_item(
    item: MenuItem,
    fleet_path: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    home: &Path,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
) -> Result<Pane> {
    match item.kind {
        MenuItemKind::Shell => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
            create_pane(
                layout, registry, home, "shell", &shell,
                &[], None, &HashMap::new(), "\r",
                cols, rows, wakeup_tx, name_counter,
            )
        }
        MenuItemKind::Backend(backend) => {
            let preset = backend.preset();
            let args: Vec<String> = preset.args.iter().map(|s| s.to_string()).collect();
            create_pane(
                layout, registry, home, preset.command,
                preset.command, &args, None, &HashMap::new(),
                preset.submit_key, cols, rows, wakeup_tx, name_counter,
            )
        }
        MenuItemKind::FleetInstance(inst_name) => {
            let fleet = crate::fleet::FleetConfig::load(fleet_path)?;
            let resolved = fleet.resolve_instance(&inst_name)
                .ok_or_else(|| anyhow::anyhow!("fleet instance '{inst_name}' not found"))?;
            create_pane(
                layout, registry, home, &inst_name,
                &resolved.backend_command, &resolved.args,
                resolved.working_directory.as_deref(),
                &resolved.env, &resolved.submit_key,
                cols, rows, wakeup_tx, name_counter,
            )
        }
    }
}

/// Spawn an agent/shell via spawn_agent and add as a new tab.
#[allow(clippy::too_many_arguments)]
fn spawn_pane_tab(
    layout: &mut Layout,
    registry: &AgentRegistry,
    home: &Path,
    base_name: &str,
    command: &str,
    args: &[String],
    working_dir: Option<&Path>,
    env: &HashMap<String, String>,
    submit_key: &str,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
) -> Result<()> {
    let pane = create_pane(
        layout, registry, home, base_name, command, args, working_dir,
        env, submit_key, cols, rows, wakeup_tx, name_counter,
    )?;
    let tab_name = pane.agent_name.clone();
    layout.add_tab(Tab::new(tab_name, pane));
    Ok(())
}

/// Create a pane backed by spawn_agent.
#[allow(clippy::too_many_arguments)]
fn create_pane(
    layout: &mut Layout,
    registry: &AgentRegistry,
    home: &Path,
    base_name: &str,
    command: &str,
    args: &[String],
    working_dir: Option<&Path>,
    env: &HashMap<String, String>,
    submit_key: &str,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
) -> Result<Pane> {
    // Auto-dedup name
    let count = name_counter.entry(base_name.to_string()).or_insert(0);
    let name = if *count == 0 {
        base_name.to_string()
    } else {
        format!("{base_name}-{count}")
    };
    *count += 1;

    // Resolve working directory
    let work_dir = working_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| home.join("workspace").join(&name));

    // Generate MCP config for agent backends
    if Backend::from_command(command).is_some() {
        crate::instructions::generate(&work_dir, command);
    }

    // Build args with MCP config flags for Claude
    let mut final_args = args.to_vec();
    if let Some(Backend::ClaudeCode) = Backend::from_command(command) {
        let mcp_config = work_dir.join("mcp-config.json");
        if mcp_config.exists() {
            final_args.push("--mcp-config".to_string());
            final_args.push(mcp_config.display().to_string());
        }
        let settings = work_dir.join("claude-settings.json");
        if settings.exists() {
            final_args.push("--settings".to_string());
            final_args.push(settings.display().to_string());
        }
    }

    // Use the daemon's spawn_agent — gets auto-dismiss, state tracking, broadcast
    agent::spawn_agent(
        &agent::SpawnConfig {
            name: &name,
            backend_command: command,
            args: &final_args,
            cols,
            rows,
            env: Some(env),
            working_dir: Some(&work_dir),
            submit_key,
            home: Some(home),
            crash_tx: None,
            shutdown: None,
        },
        registry,
    )?;

    // Subscribe to the agent's output
    let (rx, dump) = {
        let reg = agent::lock_registry(registry);
        let handle = reg.get(&name).ok_or_else(|| anyhow::anyhow!("agent not found after spawn"))?;
        agent::subscribe_with_dump(handle)
    };

    // Create local VTerm and feed the screen dump
    let mut vterm = VTerm::new(cols, rows);
    vterm.process(&dump);

    // Forward subscriber output to wakeup channel
    let pane_id = layout.next_pane_id();
    let tx = wakeup_tx.clone();
    let pane_rx = {
        let (fwd_tx, fwd_rx) = crossbeam::channel::unbounded::<Vec<u8>>();
        std::thread::Builder::new()
            .name(format!("{name}_fwd"))
            .spawn(move || {
                while let Ok(data) = rx.recv() {
                    if fwd_tx.send(data).is_err() {
                        break;
                    }
                    let _ = tx.send(pane_id);
                }
            })
            .ok();
        fwd_rx
    };

    let backend = Backend::from_command(command);

    Ok(Pane {
        agent_name: name,
        vterm,
        rx: pane_rx,
        id: pane_id,
        backend,
        working_dir: Some(work_dir),
        display_name: None,
        scroll_offset: 0,
        has_notification: false,
    })
}

/// Save current session layout to disk (including split tree structure).
fn save_session(home: &Path, layout: &Layout, registry: &AgentRegistry) {
    let reg = agent::lock_registry(registry);
    let tabs: Vec<SessionTab> = layout
        .tabs
        .iter()
        .filter_map(|tab| {
            let node = save_node(tab.root(), &reg)?;
            Some(SessionTab {
                name: tab.name.clone(),
                root: node,
            })
        })
        .collect();
    drop(reg);

    let session = Session {
        active_tab: layout.active,
        tabs,
    };

    let path = home.join("session.json");
    if let Ok(json) = serde_json::to_string_pretty(&session) {
        let _ = std::fs::write(&path, json);
        tracing::info!(path = %path.display(), tabs = session.tabs.len(), "session saved");
    }
}

fn save_node(
    node: &crate::layout::PaneNode,
    reg: &std::collections::HashMap<String, crate::agent::AgentHandle>,
) -> Option<SessionNode> {
    match node {
        crate::layout::PaneNode::Leaf(pane) => {
            let handle = reg.get(&pane.agent_name)?;
            Some(SessionNode::Leaf(SessionPane {
                agent_name: pane.agent_name.clone(),
                backend_command: handle.backend_command.clone(),
                working_dir: pane.working_dir.as_ref().map(|p| p.display().to_string()),
                submit_key: handle.submit_key.clone(),
                display_name: pane.display_name.clone(),
            }))
        }
        crate::layout::PaneNode::Split { dir, first, second } => {
            Some(SessionNode::Split {
                dir: *dir,
                first: Box::new(save_node(first, reg)?),
                second: Box::new(save_node(second, reg)?),
            })
        }
    }
}

/// Try to restore a saved session. Returns true if restored.
fn restore_session(
    home: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
    cols: u16,
    rows: u16,
) -> bool {
    let path = home.join("session.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let session: Session = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(_) => return false,
    };

    if session.tabs.is_empty() {
        return false;
    }

    let _ = std::fs::remove_file(&path);

    for tab in &session.tabs {
        if let Some(root_node) = restore_node(&tab.root, home, layout, registry, wakeup_tx, name_counter, cols, rows) {
            layout.add_tab(Tab::with_root(tab.name.clone(), root_node));
        }
    }

    if session.active_tab < layout.tabs.len() {
        layout.active = session.active_tab;
    }

    tracing::info!(tabs = session.tabs.len(), "session restored");
    true
}

fn restore_node(
    node: &SessionNode,
    home: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
    cols: u16,
    rows: u16,
) -> Option<crate::layout::PaneNode> {
    match node {
        SessionNode::Leaf(sp) => {
            let backend = Backend::from_command(&sp.backend_command);
            let args: Vec<String> = backend
                .as_ref()
                .map(|b| {
                    let preset = b.preset();
                    let mut a: Vec<String> = preset.args.iter().map(|s| s.to_string()).collect();
                    a.extend(preset.resume_mode.args_for(home, &sp.agent_name));
                    a
                })
                .unwrap_or_default();

            let submit_key = backend
                .as_ref()
                .map(|b| b.preset().submit_key.to_string())
                .unwrap_or_else(|| sp.submit_key.clone());

            let work_dir = sp.working_dir.as_ref().map(PathBuf::from);
            let mut pane = create_pane(
                layout, registry, home, &sp.agent_name, &sp.backend_command,
                &args, work_dir.as_deref(), &HashMap::new(), &submit_key,
                cols, rows, wakeup_tx, name_counter,
            ).ok()?;
            pane.display_name = sp.display_name.clone();
            Some(crate::layout::PaneNode::Leaf(Box::new(pane)))
        }
        SessionNode::Split { dir, first, second } => {
            let first_node = restore_node(first, home, layout, registry, wakeup_tx, name_counter, cols, rows)?;
            let second_node = restore_node(second, home, layout, registry, wakeup_tx, name_counter, cols, rows)?;
            Some(crate::layout::PaneNode::Split {
                dir: *dir,
                first: Box::new(first_node),
                second: Box::new(second_node),
            })
        }
    }
}

/// Adjust scroll offset of the focused pane by `delta` lines (positive = up, negative = down).
fn scroll_focused(layout: &mut Layout, delta: i32) {
    if let Some(tab) = layout.active_tab_mut() {
        let fid = tab.focus_id;
        if let Some(pane) = tab.root_mut().find_pane_mut(fid) {
            let max = pane.vterm.max_scroll();
            if delta > 0 {
                pane.scroll_offset = (pane.scroll_offset + delta as usize).min(max);
            } else {
                pane.scroll_offset = pane.scroll_offset.saturating_sub((-delta) as usize);
            }
        }
    }
}

/// Kill and remove an agent from the registry.
fn kill_agent(registry: &AgentRegistry, name: &str) {
    let mut reg = agent::lock_registry(registry);
    if let Some(handle) = reg.get(name) {
        let mut child = handle.child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
    }
    reg.remove(name);
}

// --- API server ---

struct ApiGuard {
    run_dir: Option<PathBuf>,
}

impl Drop for ApiGuard {
    fn drop(&mut self) {
        if let Some(ref dir) = self.run_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn start_api_server(home: &Path, registry: &AgentRegistry) -> ApiGuard {
    if crate::daemon::find_active_run_dir(home).is_some() {
        tracing::info!("existing daemon found, skipping in-process API server");
        return ApiGuard { run_dir: None };
    }

    let run = crate::daemon::run_dir(home);
    if std::fs::create_dir_all(&run).is_err() {
        return ApiGuard { run_dir: None };
    }
    let pid = std::process::id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::write(run.join(".daemon"), format!("{pid}:{now}"));

    let api_registry = Arc::clone(registry);
    let configs: crate::api::ConfigRegistry = Arc::new(Mutex::new(HashMap::new()));
    let externals: crate::agent::ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let api_home = home.to_path_buf();
    std::thread::Builder::new()
        .name("app_api_server".into())
        .spawn(move || {
            crate::api::serve(&api_home, api_registry, shutdown, configs, externals);
        })
        .ok();

    tracing::info!(path = %run.display(), "in-process API server started");
    ApiGuard {
        run_dir: Some(run),
    }
}
