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
use ratatui::layout::Rect;
use ratatui::DefaultTerminal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Events sent from the API server to the TUI event loop when agents or teams
/// are created/deleted via MCP tools. The TUI reacts by auto-creating or
/// removing tabs/panes.
#[derive(Debug, Clone)]
pub(crate) enum TuiEvent {
    InstanceCreated {
        name: String,
        layout: LayoutHint,
        spawner: Option<String>,
    },
    InstanceDeleted {
        name: String,
    },
    TeamCreated {
        name: String,
        members: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) enum LayoutHint {
    #[default]
    Tab,
    SplitRight,
    SplitBelow,
}

impl LayoutHint {
    /// Parse a layout-hint string into the enum.
    /// Named `parse_hint` (not `from_str`) to avoid shadowing `std::str::FromStr::from_str`.
    pub(crate) fn parse_hint(s: &str) -> Self {
        match s {
            "split-right" => Self::SplitRight,
            "split-below" => Self::SplitBelow,
            _ => Self::Tab,
        }
    }
}

pub(crate) type TuiEventSender = crossbeam::channel::Sender<TuiEvent>;

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
        #[serde(default = "default_ratio")]
        ratio: f32,
        first: Box<SessionNode>,
        second: Box<SessionNode>,
    },
}

fn default_ratio() -> f32 {
    0.5
}

/// Layout-only pane info. Agent config comes from fleet.yaml on restore.
#[derive(Serialize, Deserialize)]
struct SessionPane {
    /// Fleet instance name (key in fleet.yaml). None for shell panes.
    fleet_instance_name: Option<String>,
    /// User-defined display name override.
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
    NewTabMenu {
        items: Vec<MenuItem>,
        selected: usize,
    },
    /// Split pane selection menu — choose what to run in the new pane.
    SplitMenu {
        items: Vec<MenuItem>,
        selected: usize,
        dir: SplitDir,
    },
    RenameTab {
        input: String,
    },
    RenamePane {
        input: String,
    },
    ConfirmClose {
        target: CloseTarget,
    },
    TabList {
        selected: usize,
    },
    Help,
    /// Keyboard scroll mode (j/k/PgUp/PgDn). Pane's scroll_offset is used directly.
    Scroll,
    /// Command palette (:command input).
    Command {
        input: String,
    },
    /// Decisions overlay panel (read-only, scrollable).
    Decisions {
        items: Vec<crate::decisions::Decision>,
        scroll: usize,
    },
    /// Task board overlay panel (read-only, scrollable).
    Tasks {
        items: Vec<crate::tasks::Task>,
        scroll: usize,
    },
}

/// Handle j/k/PgUp/PgDn scroll for list overlays. Returns true if handled, false to close.
fn handle_list_scroll(key: KeyCode, scroll: &mut usize, len: usize) -> bool {
    match key {
        KeyCode::Up | KeyCode::Char('k') => {
            *scroll = scroll.saturating_sub(1);
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if *scroll + 1 < len {
                *scroll += 1;
            }
            true
        }
        KeyCode::PageUp => {
            *scroll = scroll.saturating_sub(10);
            true
        }
        KeyCode::PageDown => {
            *scroll = (*scroll + 10).min(len.saturating_sub(1));
            true
        }
        KeyCode::Esc | KeyCode::Char('q') => false,
        _ => true,
    }
}

/// Run the terminal application.
pub fn run(fleet_path_override: Option<&str>) -> Result<()> {
    // Redirect tracing to log file BEFORE ratatui takes over stderr.
    // Must happen before main.rs's tracing init — caller should skip init for App.
    let home = crate::home_dir();
    let log_path = home.join("app.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
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

    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste,
    )
    .ok();

    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal, fleet_path.as_deref());
    ratatui::restore();

    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
    )
    .ok();
    result
}

fn run_app(terminal: &mut DefaultTerminal, fleet_override: Option<&Path>) -> Result<()> {
    let home = crate::home_dir();

    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));

    let (tui_event_tx, tui_event_rx) = crossbeam::channel::bounded::<TuiEvent>(256);
    let _api_guard = start_api_server(&home, &registry, tui_event_tx);

    let mut layout = Layout::new();
    let mut key_handler = KeyHandler::new();
    let mut overlay = Overlay::None;
    let mut last_tab: usize = 0;
    // Active border drag state for mouse resize
    let mut border_drag: Option<(crate::layout::SplitBorderHit, Rect)> = None;
    // Counter for auto-dedup agent names
    let mut name_counter: HashMap<String, usize> = HashMap::new();

    let (wakeup_tx, wakeup_rx) = crossbeam::channel::unbounded::<usize>();

    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let pane_rows = rows.saturating_sub(4);
    let pane_cols = cols.saturating_sub(2);

    let fleet_path = fleet_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| home.join("fleet.yaml"));

    // Reconcile fleet.yaml (agent definitions) with session.json (layout hint)
    let started = restore_with_reconciliation(
        &home,
        &fleet_path,
        &mut layout,
        &registry,
        &wakeup_tx,
        &mut name_counter,
        pane_cols,
        pane_rows,
    );
    if !started {
        // Rule 4: nothing to restore → open shell tab
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

    // Flag to trigger resize pass after layout changes (split, close, zoom, tab switch).
    // Start true so restored split panes get correct sizes before first draw.
    let mut needs_resize = true;

    // Initialize Telegram: start polling, auto-create topics, update status
    let (telegram_state, telegram_status) = {
        let fleet = crate::fleet::FleetConfig::load(&fleet_path).ok();
        if let Some(ref config) = fleet {
            let submit_keys: HashMap<String, String> = config
                .instances
                .keys()
                .filter_map(|name| match config.resolve_instance(name) {
                    Some(r) => Some((name.clone(), r.submit_key)),
                    None => {
                        tracing::warn!(%name, "failed to resolve fleet instance");
                        None
                    }
                })
                .collect();
            match crate::telegram::init_from_config(config, &home, submit_keys) {
                Some(state) => (Some(state), render::TelegramStatus::Connected),
                None => (None, telegram_status_from_config(config)),
            }
        } else {
            (None, render::TelegramStatus::NotConfigured)
        }
    };

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
        if needs_resize {
            let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
            let pane_area = ratatui::layout::Rect::new(0, 1, c, r.saturating_sub(2));
            render::resize_panes(pane_area, &mut layout, &registry);
            needs_resize = false;
        }

        let repeat_mode = key_handler.in_repeat();

        terminal.draw(|frame| {
            render::render(frame, &mut layout, repeat_mode, &registry, telegram_status);
            match &overlay {
                Overlay::NewTabMenu { items, selected }
                | Overlay::SplitMenu {
                    items, selected, ..
                } => {
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
                    let so = layout
                        .active_tab()
                        .and_then(|t| t.focused_pane())
                        .map(|p| p.scroll_offset)
                        .unwrap_or(0);
                    render::render_scroll_indicator(frame, so);
                }
                Overlay::Command { ref input } => {
                    render::render_command_palette(frame, input);
                }
                Overlay::Decisions { ref items, scroll } => {
                    render::render_decisions(frame, items, *scroll);
                }
                Overlay::Tasks { ref items, scroll } => {
                    render::render_tasks(frame, items, *scroll);
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
                                                        maybe_create_telegram_topic(&telegram_state, &registry, &home, &pane);
                                                        let tab_name = pane.agent_name.clone();
                                                        layout.add_tab(Tab::new(tab_name, pane));
                                                        needs_resize = true;
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
                                                            maybe_create_telegram_topic(&telegram_state, &registry, &home, &p);
                                                            if let Some(tab) = layout.active_tab_mut() {
                                                                tab.split_focused(split_dir, p);
                                                            }
                                                            needs_resize = true;
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
                                                    // Collect fleet names before closing tab
                                                    let fleet_names: Vec<String> = layout.tabs.get(idx)
                                                        .into_iter()
                                                        .flat_map(|t| t.root().pane_ids().into_iter()
                                                            .filter_map(|id| t.root().find_pane(id)
                                                                .and_then(|p| p.fleet_instance_name.clone())))
                                                        .collect();
                                                    for fname in &fleet_names {
                                                        maybe_delete_telegram_topic(&telegram_state, &home, fname);
                                                    }
                                                    if !fleet_names.is_empty() {
                                                        let _ = crate::fleet::remove_instances_from_yaml(&home, &fleet_names);
                                                    }
                                                    if let Some(tab) = layout.close_tab(idx) {
                                                        for name in tab.root().agent_names() {
                                                            kill_agent(&registry, &name);
                                                        }
                                                    }
                                                    needs_resize = true;
                                                }
                                            } else if let Some(tab) = layout.active_tab_mut() {
                                                // Remove from fleet.yaml before closing pane
                                                let fid = tab.focus_id;
                                                if let Some(pane) = tab.root().find_pane(fid) {
                                                    if let Some(ref fleet_name) = pane.fleet_instance_name {
                                                        maybe_delete_telegram_topic(&telegram_state, &home, fleet_name);
                                                        let _ = crate::fleet::remove_instance_from_yaml(&home, fleet_name);
                                                    }
                                                }
                                                if let Some(name) = tab.close_focused() {
                                                    kill_agent(&registry, &name);
                                                    needs_resize = true;
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
                                Overlay::Command { ref mut input } => {
                                    match key.code {
                                        KeyCode::Enter => {
                                            let cmd = input.clone();
                                            overlay = Overlay::None;
                                            if execute_command(&cmd, &home, &mut layout, &registry, &wakeup_tx, &mut name_counter, &telegram_state) {
                                                needs_resize = true;
                                            }
                                        }
                                        KeyCode::Esc => { overlay = Overlay::None; }
                                        KeyCode::Backspace => { input.pop(); }
                                        KeyCode::Char(c) => { input.push(c); }
                                        _ => {}
                                    }
                                }
                                Overlay::Decisions { ref items, ref mut scroll } => {
                                    if !handle_list_scroll(key.code, scroll, items.len()) {
                                        overlay = Overlay::None;
                                    }
                                }
                                Overlay::Tasks { ref items, ref mut scroll } => {
                                    if !handle_list_scroll(key.code, scroll, items.len()) {
                                        overlay = Overlay::None;
                                    }
                                }
                                Overlay::None => {}
                            }
                            continue;
                        }

                        let action = key_handler.handle(key);
                        match action {
                            Action::Forward(key) => {
                                let bytes = crate::tui::key_to_bytes(key.code, key.modifiers);
                                if !bytes.is_empty() {
                                    write_to_focused(&layout, &registry, &bytes);
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
                                needs_resize = true;
                            }
                            Action::PrevTab => {
                                last_tab = layout.active;
                                layout.prev_tab();
                                needs_resize = true;
                            }
                            Action::LastTab => {
                                let current = layout.active;
                                layout.goto_tab(last_tab);
                                last_tab = current;
                                needs_resize = true;
                            }
                            Action::GotoTab(idx) => {
                                last_tab = layout.active;
                                layout.goto_tab(idx);
                                needs_resize = true;
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
                            Action::CommandPalette => {
                                overlay = Overlay::Command { input: String::new() };
                            }
                            Action::ShowDecisions => {
                                let items = crate::decisions::list_all(&home);
                                overlay = Overlay::Decisions { items, scroll: 0 };
                            }
                            Action::ShowTasks => {
                                let items = crate::tasks::list_all(&home);
                                overlay = Overlay::Tasks { items, scroll: 0 };
                            }
                            Action::ShowHelp => {
                                overlay = Overlay::Help;
                            }
                            Action::Detach => break,
                            Action::ToggleZoom => {
                                if let Some(tab) = layout.active_tab_mut() {
                                    tab.zoomed = !tab.zoomed;
                                }
                                needs_resize = true;
                            }
                            Action::NextLayout => {
                                if let Some(tab) = layout.active_tab_mut() {
                                    tab.next_layout();
                                }
                                needs_resize = true;
                            }
                            Action::ResizeUp | Action::ResizeDown
                            | Action::ResizeLeft | Action::ResizeRight => {
                                let dir = match action {
                                    Action::ResizeUp => crate::layout::Direction::Up,
                                    Action::ResizeDown => crate::layout::Direction::Down,
                                    Action::ResizeLeft => crate::layout::Direction::Left,
                                    _ => crate::layout::Direction::Right,
                                };
                                if let Some(tab) = layout.active_tab_mut() {
                                    let focus = tab.focus_id;
                                    // Pane tree occupies terminal height minus
                                    // the tab bar row and status bar row (see
                                    // render::render_app chrome layout).
                                    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
                                    let area = (0, 1, cols, rows.saturating_sub(2));
                                    crate::layout::resize_focused(tab.root_mut(), area, focus, dir, 0.05);
                                }
                                needs_resize = true;
                            }
                            Action::None => {}
                        }
                    }
                    // #11: Overlays (Help, Tasks, Decisions, Command palette, rename,
                    // etc.) are modal — mouse events must not reach hidden panes,
                    // otherwise drag/selection state accumulates on panes the user
                    // can't see. Swallow mouse events while any overlay is active.
                    Event::Mouse(_) if !matches!(overlay, Overlay::None) => {}
                    Event::Mouse(mouse) => {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => scroll_focused(&mut layout, 3),
                            MouseEventKind::ScrollDown => scroll_focused(&mut layout, -3),
                            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                                if mouse.row == 0 {
                                    match tab_bar_hit_test(&layout, mouse.column) {
                                        Some(TabBarClick::Tab(idx)) => {
                                            last_tab = layout.active;
                                            layout.goto_tab(idx);
                                            needs_resize = true;
                                        }
                                        Some(TabBarClick::NewTab) => {
                                            overlay = Overlay::NewTabMenu {
                                                items: build_menu_items(&fleet_path, &registry),
                                                selected: 0,
                                            };
                                        }
                                        None => {}
                                    }
                                } else {
                                    // Title-bar hit is checked before split-border so that
                                    // horizontally-stacked panes (whose top border coincides
                                    // with the split line) can be grabbed for drag-to-swap.
                                    // Horizontal-split borders must be resized via keyboard.
                                    let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
                                    let pa = Rect::new(0, 1, c, r.saturating_sub(2));
                                    // #12: When a tab is zoomed, only the focused pane is
                                    // visible; the split tree still exists but its borders
                                    // aren't rendered. Disable title-bar AND border hit-tests
                                    // so users can't drag invisible borders.
                                    let zoomed = layout.active_tab().is_some_and(|t| t.zoomed);
                                    let title_hit = (!zoomed)
                                        .then(|| {
                                            layout
                                                .active_tab()
                                                .and_then(|tab| tab.title_bar_at(mouse.column, mouse.row))
                                        })
                                        .flatten();
                                    if let Some(pane_id) = title_hit {
                                        if let Some(tab) = layout.active_tab_mut() {
                                            tab.focus_id = pane_id;
                                            // #2: Only start a drag when there's a possible
                                            // swap target. Otherwise the source pane briefly
                                            // flashes magenta for a no-op.
                                            if tab.root().pane_count() > 1 {
                                                tab.dragging_pane = Some(pane_id);
                                                tab.drag_target = None;
                                            }
                                        }
                                    } else if !zoomed {
                                        let hit = layout.active_tab().and_then(|tab| {
                                            crate::layout::find_split_border(
                                                tab.root(),
                                                (pa.x, pa.y, pa.width, pa.height),
                                                mouse.column,
                                                mouse.row,
                                            )
                                        });
                                        if let Some(h) = hit {
                                            border_drag = Some((h, pa));
                                        } else {
                                            handle_mouse_selection(&mut layout, &mouse);
                                        }
                                    } else {
                                        // Zoomed: only selection inside the one visible pane.
                                        handle_mouse_selection(&mut layout, &mouse);
                                    }
                                }
                            }
                            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                                if let Some((ref hit, ref pa)) = border_drag {
                                    let mouse_pos = match hit.dir {
                                        SplitDir::Horizontal => mouse.row,
                                        SplitDir::Vertical => mouse.column,
                                    };
                                    if let Some(tab) = layout.active_tab_mut() {
                                        crate::layout::adjust_split_ratio(
                                            tab.root_mut(),
                                            (pa.x, pa.y, pa.width, pa.height),
                                            hit.split_area,
                                            mouse_pos,
                                            hit.dir,
                                        );
                                    }
                                    // Don't fire PTY resize per-tick: the render
                                    // loop recomputes pane_rects from the updated
                                    // ratio so the drag is visually smooth, but
                                    // resizing the PTY every mouse cell triggers
                                    // the backend (Claude/etc.) to reflow its
                                    // entire UI and floods us with redraw data.
                                    // Defer the single PTY resize to mouse-up.
                                } else if layout.active_tab().is_some_and(|t| t.dragging_pane.is_some()) {
                                    let target = layout.active_tab().and_then(|tab| {
                                        let source = tab.dragging_pane?;
                                        tab.pane_at(mouse.column, mouse.row)
                                            .filter(|&id| id != source)
                                    });
                                    if let Some(tab) = layout.active_tab_mut() {
                                        tab.drag_target = target;
                                    }
                                } else {
                                    handle_mouse_selection(&mut layout, &mouse);
                                }
                            }
                            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                                if border_drag.is_some() {
                                    border_drag = None;
                                    // Ratio was updated live during drag but
                                    // PTY resizes were deferred — fire one now.
                                    needs_resize = true;
                                } else if layout.active_tab().is_some_and(|t| t.dragging_pane.is_some()) {
                                    let source_id = layout.active_tab().and_then(|t| t.dragging_pane);
                                    let target_id = layout.active_tab().and_then(|t| t.drag_target);
                                    if let (Some(src), Some(tgt)) = (source_id, target_id) {
                                        if let Some(tab) = layout.active_tab_mut() {
                                            crate::layout::swap_panes(tab.root_mut(), src, tgt);
                                        }
                                        needs_resize = true;
                                    }
                                    if let Some(tab) = layout.active_tab_mut() {
                                        tab.clear_drag();
                                    }
                                } else {
                                    handle_mouse_selection(&mut layout, &mouse);
                                }
                            }
                            _ => {}
                        }
                    }
                    Event::Paste(text) => {
                        match &mut overlay {
                            Overlay::RenameTab { ref mut input }
                            | Overlay::RenamePane { ref mut input }
                            | Overlay::Command { ref mut input } => {
                                input.push_str(&text);
                            }
                            Overlay::None => {
                                write_to_focused(&layout, &registry, text.as_bytes());
                            }
                            _ => {} // ignore paste in non-input overlays
                        }
                    }
                    Event::Resize(cols, rows) => {
                        let pane_area = ratatui::layout::Rect::new(0, 1, cols, rows.saturating_sub(2));
                        render::resize_panes(pane_area, &mut layout, &registry);
                    }
                    _ => {}
                }
            }
            recv(wakeup_rx) -> _ => {
                // Wakeup from PTY output — triggers redraw
            }
            recv(tui_event_rx) -> ev => {
                if let Ok(event) = ev {
                    handle_tui_event(
                        event,
                        &mut layout,
                        &registry,
                        &wakeup_tx,
                    );
                    needs_resize = true;
                }
            }
            default(std::time::Duration::from_millis(50)) => {
                // Periodic redraw for state updates
            }
        }
    }

    // Save session IDs so resume works after reattach
    save_all_session_ids(&home, &layout);

    // Sync fleet.yaml to match current state, then save layout
    sync_fleet_yaml(&home, &layout);
    save_session(&home, &layout);

    // Cleanup: kill all agents
    for tab in &layout.tabs {
        for name in tab.root().agent_names() {
            kill_agent(&registry, &name);
        }
    }

    Ok(())
}

/// Auto-start all fleet instances as tabs. Returns true if any were spawned.
#[allow(clippy::too_many_arguments)]
fn auto_start_fleet(
    fleet_path: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    home: &Path,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
) -> bool {
    let fleet = match crate::fleet::FleetConfig::load(fleet_path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut names = fleet.instance_names();
    if names.is_empty() {
        return false;
    }
    names.sort();
    let mut spawned = false;
    for name in &names {
        if let Some(resolved) = fleet.resolve_instance(name) {
            match create_pane_from_resolved(
                name,
                &resolved,
                layout,
                registry,
                home,
                cols,
                rows,
                wakeup_tx,
                name_counter,
            ) {
                Ok(pane) => {
                    let tab_name = pane.agent_name.clone();
                    layout.add_tab(Tab::new(tab_name, pane));
                    spawned = true;
                }
                Err(e) => tracing::error!(instance = name, error = %e, "fleet auto-start failed"),
            }
        }
    }
    spawned
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
            let already_open = running
                .iter()
                .any(|r| r == &name || r.starts_with(&format!("{name}-")));
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
                layout,
                registry,
                home,
                "shell",
                &shell,
                &[],
                None,
                &HashMap::new(),
                "\r",
                cols,
                rows,
                wakeup_tx,
                name_counter,
            )
        }
        MenuItemKind::Backend(backend) => {
            let preset = backend.preset();
            let inst_name = unique_fleet_name(home, preset.command);
            if let Err(e) = crate::fleet::add_instance_to_yaml(
                home,
                &inst_name,
                &crate::fleet::InstanceYamlEntry {
                    backend: Some(backend.name().to_string()),
                    working_directory: None,
                    role: None,
                },
            ) {
                tracing::warn!(error = %e, "failed to write fleet.yaml");
            }
            // Resolve from fleet to get defaults merged
            let fleet = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).ok();
            if let Some(resolved) = fleet.as_ref().and_then(|f| f.resolve_instance(&inst_name)) {
                create_pane_from_resolved(
                    &inst_name,
                    &resolved,
                    layout,
                    registry,
                    home,
                    cols,
                    rows,
                    wakeup_tx,
                    name_counter,
                )
            } else {
                let args: Vec<String> = preset.args.iter().map(|s| s.to_string()).collect();
                create_pane(
                    layout,
                    registry,
                    home,
                    &inst_name,
                    preset.command,
                    &args,
                    None,
                    &HashMap::new(),
                    preset.submit_key,
                    cols,
                    rows,
                    wakeup_tx,
                    name_counter,
                )
            }
        }
        MenuItemKind::FleetInstance(inst_name) => {
            let fleet = crate::fleet::FleetConfig::load(fleet_path)?;
            let resolved = fleet
                .resolve_instance(&inst_name)
                .ok_or_else(|| anyhow::anyhow!("fleet instance '{inst_name}' not found"))?;
            create_pane_from_resolved(
                &inst_name,
                &resolved,
                layout,
                registry,
                home,
                cols,
                rows,
                wakeup_tx,
                name_counter,
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
        layout,
        registry,
        home,
        base_name,
        command,
        args,
        working_dir,
        env,
        submit_key,
        cols,
        rows,
        wakeup_tx,
        name_counter,
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
        let handle = reg
            .get(&name)
            .ok_or_else(|| anyhow::anyhow!("agent not found after spawn"))?;
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
        fleet_instance_name: None,
        selection: None,
    })
}

/// Attach a pane to an already-running agent (no spawn — subscribe only).
/// Used when the API server creates an agent via MCP and the TUI needs to show it.
fn attach_pane(
    name: &str,
    registry: &AgentRegistry,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    layout: &mut Layout,
) -> Result<Pane> {
    let (rx, dump, backend_command) = {
        let reg = agent::lock_registry(registry);
        let handle = reg
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("agent '{name}' not found in registry"))?;
        let (rx, dump) = agent::subscribe_with_dump(handle);
        (rx, dump, handle.backend_command.clone())
    };

    let mut vterm = VTerm::new(cols, rows);
    vterm.process(&dump);

    let pane_id = layout.next_pane_id();
    let tx = wakeup_tx.clone();
    let pane_rx = {
        let n = name.to_string();
        let (fwd_tx, fwd_rx) = crossbeam::channel::unbounded::<Vec<u8>>();
        std::thread::Builder::new()
            .name(format!("{n}_fwd"))
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

    let backend = Backend::from_command(&backend_command);

    Ok(Pane {
        agent_name: name.to_string(),
        vterm,
        rx: pane_rx,
        id: pane_id,
        backend,
        working_dir: None,
        display_name: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: Some(name.to_string()),
        selection: None,
    })
}

/// Create a pane from a fleet ResolvedInstance (full config: env, args, model, etc.).
#[allow(clippy::too_many_arguments)]
fn create_pane_from_resolved(
    fleet_name: &str,
    resolved: &crate::fleet::ResolvedInstance,
    layout: &mut Layout,
    registry: &AgentRegistry,
    home: &Path,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
) -> Result<Pane> {
    // Build fleet peer list for agent instructions
    let fleet_path = home.join("fleet.yaml");
    let peers: Vec<(String, Option<String>)> = crate::fleet::FleetConfig::load(&fleet_path)
        .map(|f| {
            f.instances
                .iter()
                .map(|(n, c)| (n.clone(), c.role.clone()))
                .collect()
        })
        .unwrap_or_default();
    let ctx = crate::instructions::AgentContext {
        name: fleet_name,
        role: resolved.role.as_deref(),
        fleet_peers: &peers,
    };

    let mut pane = create_pane(
        layout,
        registry,
        home,
        fleet_name,
        &resolved.backend_command,
        &resolved.args,
        resolved.working_directory.as_deref(),
        &resolved.env,
        &resolved.submit_key,
        cols,
        rows,
        wakeup_tx,
        name_counter,
    )?;

    // Overwrite basic instructions with fleet-aware version
    if let Some(ref wd) = pane.working_dir {
        crate::instructions::generate_with_context(wd, &resolved.backend_command, Some(&ctx));
    }
    pane.fleet_instance_name = Some(fleet_name.to_string());
    Ok(pane)
}

/// Derive Telegram status from an already-loaded FleetConfig (no disk I/O).
fn telegram_status_from_config(config: &crate::fleet::FleetConfig) -> render::TelegramStatus {
    match config.channel {
        Some(crate::fleet::ChannelConfig::Telegram {
            ref bot_token_env, ..
        }) => {
            if std::env::var(bot_token_env).is_ok() {
                render::TelegramStatus::Connected
            } else {
                render::TelegramStatus::NoToken
            }
        }
        None => render::TelegramStatus::NotConfigured,
    }
}

/// Create a Telegram topic for a newly spawned fleet instance (non-blocking).
/// Spawns a background thread for the Telegram API call to avoid freezing the TUI.
fn maybe_create_telegram_topic(
    tg: &Option<Arc<Mutex<crate::telegram::TelegramState>>>,
    registry: &AgentRegistry,
    home: &Path,
    pane: &Pane,
) {
    let Some(tg) = tg else { return };
    let Some(fleet_name) = &pane.fleet_instance_name else {
        return;
    };
    {
        let s = crate::telegram::lock_state(tg);
        if s.instance_to_topic.contains_key(fleet_name) {
            return;
        }
    }
    let submit_key = {
        let reg = agent::lock_registry(registry);
        reg.get(&pane.agent_name)
            .map(|h| h.submit_key.clone())
            .unwrap_or_else(|| "\r".to_string())
    };
    let tg = Arc::clone(tg);
    let home = home.to_path_buf();
    let fleet_name = fleet_name.clone();
    std::thread::spawn(move || {
        match crate::telegram::create_topic_for_instance(&home, &fleet_name) {
            Some(tid) => {
                let mut s = crate::telegram::lock_state(&tg);
                s.instance_to_topic.insert(fleet_name.clone(), tid);
                s.topic_to_instance.insert(tid, fleet_name.clone());
                s.submit_keys.insert(fleet_name, submit_key);
            }
            None => tracing::warn!(%fleet_name, "failed to create Telegram topic"),
        }
    });
}

/// Delete Telegram topic for a fleet instance (non-blocking).
/// State is updated immediately; the Telegram API call runs on a background thread.
fn maybe_delete_telegram_topic(
    tg: &Option<Arc<Mutex<crate::telegram::TelegramState>>>,
    home: &Path,
    fleet_name: &str,
) {
    let Some(tg) = tg else { return };
    let tid = {
        let mut s = crate::telegram::lock_state(tg);
        match s.instance_to_topic.remove(fleet_name) {
            Some(tid) => {
                s.topic_to_instance.remove(&tid);
                s.submit_keys.remove(fleet_name);
                tid
            }
            None => return,
        }
    };
    let home = home.to_path_buf();
    std::thread::spawn(move || {
        crate::telegram::delete_topic(&home, tid);
    });
}

/// Resolve a backend command string into (command, args, submit_key).
/// If `fresh` is true, uses fresh_args (no resume) when available.
fn resolve_backend(backend_name: &str, fresh: bool) -> (String, Vec<String>, String) {
    if let Some(b) = Backend::from_command(backend_name) {
        let p = b.preset();
        let args = if fresh {
            p.fresh_args.unwrap_or(p.args)
        } else {
            p.args
        };
        (
            p.command.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
            p.submit_key.to_string(),
        )
    } else {
        (backend_name.to_string(), vec![], "\r".to_string())
    }
}

/// Execute a command palette command. Returns true if layout changed (needs resize).
fn execute_command(
    cmd: &str,
    home: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
    telegram_state: &Option<Arc<Mutex<crate::telegram::TelegramState>>>,
) -> bool {
    let parts: Vec<&str> = cmd.trim().splitn(3, ' ').collect();
    if parts.is_empty() {
        return false;
    }
    match parts[0] {
        "spawn" | "vsplit" | "hsplit" => {
            let base_name = parts.get(1).unwrap_or(&"agent");
            let backend_name = parts.get(2).unwrap_or(&"claude");
            let fleet_path = home.join("fleet.yaml");
            let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
            let pc = cols.saturating_sub(2);
            let pr = rows.saturating_sub(4);

            // unique_fleet_name guarantees inst_name is not yet in fleet.yaml
            let inst_name = unique_fleet_name(home, base_name);
            if let Err(e) = crate::fleet::add_instance_to_yaml(
                home,
                &inst_name,
                &crate::fleet::InstanceYamlEntry {
                    backend: Some(backend_name.to_string()),
                    working_directory: None,
                    role: None,
                },
            ) {
                tracing::warn!(name = %inst_name, error = %e, "failed to write fleet.yaml");
            }
            let fleet = crate::fleet::FleetConfig::load(&fleet_path).ok();
            let pane_result = if let Some(resolved) =
                fleet.as_ref().and_then(|f| f.resolve_instance(&inst_name))
            {
                create_pane_from_resolved(
                    &inst_name,
                    &resolved,
                    layout,
                    registry,
                    home,
                    pc,
                    pr,
                    wakeup_tx,
                    name_counter,
                )
            } else {
                let (command, args, submit_key) = resolve_backend(backend_name, false);
                create_pane(
                    layout,
                    registry,
                    home,
                    &inst_name,
                    &command,
                    &args,
                    None,
                    &HashMap::new(),
                    &submit_key,
                    pc,
                    pr,
                    wakeup_tx,
                    name_counter,
                )
            };
            match pane_result {
                Ok(pane) => {
                    maybe_create_telegram_topic(telegram_state, registry, home, &pane);
                    match parts[0] {
                        "vsplit" => {
                            if let Some(tab) = layout.active_tab_mut() {
                                tab.split_focused(SplitDir::Vertical, pane);
                            }
                        }
                        "hsplit" => {
                            if let Some(tab) = layout.active_tab_mut() {
                                tab.split_focused(SplitDir::Horizontal, pane);
                            }
                        }
                        _ => {
                            let tab_name = pane.agent_name.clone();
                            layout.add_tab(Tab::new(tab_name, pane));
                        }
                    }
                    return true;
                }
                Err(e) => {
                    tracing::error!(name = %inst_name, backend = *backend_name, error = %e, "spawn failed")
                }
            }
        }
        "kill" => {
            if let Some(name) = parts.get(1) {
                if let Some(fleet_name) = lookup_fleet_name(layout, name) {
                    maybe_delete_telegram_topic(telegram_state, home, &fleet_name);
                    let _ = crate::fleet::remove_instance_from_yaml(home, &fleet_name);
                }
                kill_agent(registry, name);
                remove_agent_pane(name, layout);
                return true;
            }
        }
        "restart" => {
            let target_name = parts.get(1).map(|s| s.to_string()).or_else(|| {
                layout
                    .active_tab()
                    .and_then(|t| t.focused_pane())
                    .map(|p| p.agent_name.clone())
            });
            if let Some(name) = target_name {
                // Single pass: find pane info, fleet name, and location
                #[allow(clippy::type_complexity)]
                let mut pane_info: Option<(
                    String,
                    Option<PathBuf>,
                    Option<String>,
                    Option<String>,
                )> = None;
                let mut pane_loc: Option<(usize, usize)> = None;
                'outer: for (ti, tab) in layout.tabs.iter().enumerate() {
                    for id in tab.root().pane_ids() {
                        if let Some(p) = tab.root().find_pane(id) {
                            if p.agent_name == name {
                                let cmd = match &p.backend {
                                    Some(b) => b.preset().command.to_string(),
                                    None => {
                                        tracing::warn!(agent = name, "cannot restart shell pane");
                                        break 'outer;
                                    }
                                };
                                pane_info = Some((
                                    cmd,
                                    p.working_dir.clone(),
                                    p.display_name.clone(),
                                    p.fleet_instance_name.clone(),
                                ));
                                pane_loc = Some((ti, id));
                                break 'outer;
                            }
                        }
                    }
                }

                if let Some((backend_cmd, work_dir, display_name, fleet_name)) = pane_info {
                    kill_agent(registry, &name);
                    let _ = std::fs::remove_file(home.join("sessions").join(format!("{name}.sid")));

                    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
                    let pc = cols.saturating_sub(2);
                    let pr = rows.saturating_sub(4);
                    name_counter.remove(&name);

                    let pane_result = if let Some(ref fname) = fleet_name {
                        // Fleet agent — resolve from fleet.yaml (full config)
                        let fleet_path = home.join("fleet.yaml");
                        let fleet = crate::fleet::FleetConfig::load(&fleet_path).ok();
                        if let Some(resolved) =
                            fleet.as_ref().and_then(|f| f.resolve_instance(fname))
                        {
                            create_pane_from_resolved(
                                fname,
                                &resolved,
                                layout,
                                registry,
                                home,
                                pc,
                                pr,
                                wakeup_tx,
                                name_counter,
                            )
                        } else {
                            let (command, args, submit_key) = resolve_backend(&backend_cmd, true);
                            create_pane(
                                layout,
                                registry,
                                home,
                                &name,
                                &command,
                                &args,
                                work_dir.as_deref(),
                                &HashMap::new(),
                                &submit_key,
                                pc,
                                pr,
                                wakeup_tx,
                                name_counter,
                            )
                        }
                    } else {
                        // Non-fleet pane — use backend preset directly
                        let (command, args, submit_key) = resolve_backend(&backend_cmd, true);
                        create_pane(
                            layout,
                            registry,
                            home,
                            &name,
                            &command,
                            &args,
                            work_dir.as_deref(),
                            &HashMap::new(),
                            &submit_key,
                            pc,
                            pr,
                            wakeup_tx,
                            name_counter,
                        )
                    };
                    if let Ok(mut new_pane) = pane_result {
                        // Swap only vterm + rx into the existing pane slot
                        if let Some((ti, pid)) = pane_loc {
                            if let Some(pane) = layout.tabs[ti].root_mut().find_pane_mut(pid) {
                                std::mem::swap(&mut pane.vterm, &mut new_pane.vterm);
                                std::mem::swap(&mut pane.rx, &mut new_pane.rx);
                                pane.agent_name = new_pane.agent_name;
                                pane.display_name = display_name;
                                pane.scroll_offset = 0;
                                pane.has_notification = false;
                                return true;
                            }
                        }
                        // Fallback: add as new tab
                        let tab_name = new_pane.agent_name.clone();
                        layout.add_tab(Tab::new(tab_name, new_pane));
                        return true;
                    }
                }
            }
        }
        "layout" => {
            let Some(tab) = layout.active_tab_mut() else {
                return false;
            };
            let Some(name) = parts.get(1) else {
                tab.next_layout();
                return true;
            };
            let Some(preset) = crate::layout::LayoutPreset::from_name(name) else {
                tracing::warn!(name = *name, valid = crate::layout::LayoutPreset::all_names(), "unknown layout preset");
                return false;
            };
            tab.apply_layout(preset);
            return true;
        }
        "send" => {
            if parts.len() >= 3 && !agent::send_to_registry(registry, "user", parts[1], parts[2]) {
                tracing::warn!(target = parts[1], "send: agent not found in registry");
            }
        }
        "broadcast" => {
            if let Some(msg) = parts.get(1) {
                agent::broadcast_registry(registry, "user", msg, None);
            }
        }
        "status" => {
            let reg = agent::lock_registry(registry);
            for (name, handle) in reg.iter() {
                if let Ok(core) = handle.core.lock() {
                    tracing::info!(agent = name, state = ?core.state.get_state(), "status");
                }
            }
        }
        _ => {
            tracing::warn!(cmd = cmd, "unknown command");
        }
    }
    false
}

/// Persist agent session IDs on detach so resume works after reattach.
fn save_all_session_ids(home: &Path, layout: &Layout) {
    for tab in &layout.tabs {
        for id in tab.root().pane_ids() {
            let Some(pane) = tab.root().find_pane(id) else {
                continue;
            };
            if pane.backend.is_none() {
                continue;
            }
            let Some(ref dir) = pane.working_dir else {
                continue;
            };
            let name = pane
                .fleet_instance_name
                .as_deref()
                .unwrap_or(&pane.agent_name);
            if let Some(sid) = crate::backend::read_session_id(dir) {
                crate::backend::save_session_id(home, name, &sid);
            }
        }
    }
}

/// Sync fleet.yaml to match current pane state on detach.
/// Removes fleet entries not present in any pane; adds panes with backend but missing from fleet.
fn sync_fleet_yaml(home: &Path, layout: &Layout) {
    let fleet_path = home.join("fleet.yaml");
    let fleet = crate::fleet::FleetConfig::load(&fleet_path).ok();

    // Collect all fleet_instance_names currently in panes
    let mut active_fleet_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for tab in &layout.tabs {
        for id in tab.root().pane_ids() {
            if let Some(pane) = tab.root().find_pane(id) {
                if let Some(ref name) = pane.fleet_instance_name {
                    active_fleet_names.insert(name.clone());
                }
            }
        }
    }

    // Batch-remove fleet entries not in any pane (single atomic write)
    if let Some(ref f) = fleet {
        let to_remove: Vec<String> = f
            .instance_names()
            .into_iter()
            .filter(|name| !active_fleet_names.contains(name))
            .collect();
        if !to_remove.is_empty() {
            let _ = crate::fleet::remove_instances_from_yaml(home, &to_remove);
        }
    }
}

/// Save current session layout to disk. Only stores layout geometry, not agent config.
fn save_session(home: &Path, layout: &Layout) {
    let tabs: Vec<SessionTab> = layout
        .tabs
        .iter()
        .map(|tab| SessionTab {
            name: tab.name.clone(),
            root: save_node(tab.root()),
        })
        .collect();

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

fn save_node(node: &crate::layout::PaneNode) -> SessionNode {
    match node {
        crate::layout::PaneNode::Leaf(pane) => SessionNode::Leaf(SessionPane {
            fleet_instance_name: pane.fleet_instance_name.clone(),
            display_name: pane.display_name.clone(),
        }),
        crate::layout::PaneNode::Split { dir, ratio, first, second } => SessionNode::Split {
            dir: *dir,
            ratio: *ratio,
            first: Box::new(save_node(first)),
            second: Box::new(save_node(second)),
        },
    }
}

/// Restore with reconciliation: fleet.yaml is source of truth for agents,
/// session.json is a layout hint. Returns true if anything was spawned.
#[allow(clippy::too_many_arguments)]
fn restore_with_reconciliation(
    home: &Path,
    fleet_path: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
    cols: u16,
    rows: u16,
) -> bool {
    let fleet = crate::fleet::FleetConfig::load(fleet_path).ok();
    let fleet_names: std::collections::HashSet<String> = fleet
        .as_ref()
        .map(|f| f.instance_names().into_iter().collect())
        .unwrap_or_default();

    // Try loading session.json as layout hint
    let session_path = home.join("session.json");
    let session: Option<Session> = std::fs::read_to_string(&session_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok());

    if let Some(session) = session {
        let _ = std::fs::remove_file(&session_path);
        if !session.tabs.is_empty() {
            let mut placed: std::collections::HashSet<String> = std::collections::HashSet::new();

            for tab in &session.tabs {
                if let Some(root_node) = restore_node_reconciled(
                    &tab.root,
                    fleet.as_ref(),
                    home,
                    layout,
                    registry,
                    wakeup_tx,
                    name_counter,
                    cols,
                    rows,
                    &mut placed,
                ) {
                    layout.add_tab(Tab::with_root(tab.name.clone(), root_node));
                }
            }

            // Rule 3: fleet agents not in session → append as new tabs
            let mut unplaced: Vec<String> = fleet_names.difference(&placed).cloned().collect();
            unplaced.sort();
            for name in &unplaced {
                if let Some(resolved) = fleet.as_ref().and_then(|f| f.resolve_instance(name)) {
                    if let Ok(pane) = create_pane_from_resolved(
                        name,
                        &resolved,
                        layout,
                        registry,
                        home,
                        cols,
                        rows,
                        wakeup_tx,
                        name_counter,
                    ) {
                        let tab_name = pane.agent_name.clone();
                        layout.add_tab(Tab::new(tab_name, pane));
                    }
                }
            }

            if session.active_tab < layout.tabs.len() {
                layout.active = session.active_tab;
            }

            if !layout.tabs.is_empty() {
                tracing::info!(
                    tabs = layout.tabs.len(),
                    "session restored with reconciliation"
                );
                return true;
            }
        }
    }

    // No session.json or empty → rule 1: auto-start fleet
    if !fleet_names.is_empty() {
        return auto_start_fleet(
            fleet_path,
            layout,
            registry,
            home,
            cols,
            rows,
            wakeup_tx,
            name_counter,
        );
    }

    // Rule 4: nothing → caller adds shell tab
    false
}

#[allow(clippy::too_many_arguments)]
fn restore_node_reconciled(
    node: &SessionNode,
    fleet: Option<&crate::fleet::FleetConfig>,
    home: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
    cols: u16,
    rows: u16,
    placed: &mut std::collections::HashSet<String>,
) -> Option<crate::layout::PaneNode> {
    match node {
        SessionNode::Leaf(sp) => {
            match &sp.fleet_instance_name {
                Some(fleet_name) => {
                    // Fleet agent — resolve from fleet.yaml, add resume args for session continuity.
                    // Safe: preset.args should not contain resume flags (those live in resume_mode).
                    let mut resolved = fleet?.resolve_instance(fleet_name)?;
                    if let Some(backend) = Backend::from_command(&resolved.backend_command) {
                        resolved
                            .args
                            .extend(backend.preset().resume_mode.args_for(home, fleet_name));
                    }
                    placed.insert(fleet_name.clone());
                    let mut pane = create_pane_from_resolved(
                        fleet_name,
                        &resolved,
                        layout,
                        registry,
                        home,
                        cols,
                        rows,
                        wakeup_tx,
                        name_counter,
                    )
                    .ok()?;
                    pane.display_name = sp.display_name.clone();
                    Some(crate::layout::PaneNode::Leaf(Box::new(pane)))
                }
                None => {
                    // Shell pane — recreate fresh
                    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
                    let mut pane = create_pane(
                        layout,
                        registry,
                        home,
                        "shell",
                        &shell,
                        &[],
                        None,
                        &HashMap::new(),
                        "\r",
                        cols,
                        rows,
                        wakeup_tx,
                        name_counter,
                    )
                    .ok()?;
                    pane.display_name = sp.display_name.clone();
                    Some(crate::layout::PaneNode::Leaf(Box::new(pane)))
                }
            }
        }
        SessionNode::Split { dir, ratio, first, second } => {
            let f = restore_node_reconciled(
                first,
                fleet,
                home,
                layout,
                registry,
                wakeup_tx,
                name_counter,
                cols,
                rows,
                placed,
            );
            let s = restore_node_reconciled(
                second,
                fleet,
                home,
                layout,
                registry,
                wakeup_tx,
                name_counter,
                cols,
                rows,
                placed,
            );
            match (f, s) {
                (Some(f), Some(s)) => Some(crate::layout::PaneNode::Split {
                    dir: *dir,
                    ratio: *ratio,
                    first: Box::new(f),
                    second: Box::new(s),
                }),
                // Rule 2: one side missing → collapse, sibling takes full space
                (Some(node), None) | (None, Some(node)) => Some(node),
                (None, None) => None,
            }
        }
    }
}

/// Write bytes to the focused pane's PTY.
fn write_to_focused(layout: &Layout, registry: &AgentRegistry, bytes: &[u8]) {
    if let Some(name) = layout
        .active_tab()
        .and_then(|t| t.focused_pane())
        .map(|p| p.agent_name.clone())
    {
        let reg = agent::lock_registry(registry);
        if let Some(handle) = reg.get(&name) {
            let _ = agent::write_to_agent(handle, bytes);
        }
    }
}

/// Handle a TuiEvent from the API server (auto-create/remove tabs/panes).
fn handle_tui_event(
    event: TuiEvent,
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
) {
    match event {
        TuiEvent::InstanceCreated {
            name,
            layout: hint,
            spawner,
        } => {
            handle_instance_created(&name, hint, spawner.as_deref(), layout, registry, wakeup_tx);
        }
        TuiEvent::InstanceDeleted { name } => {
            handle_instance_deleted(&name, layout);
        }
        TuiEvent::TeamCreated { name, members } => {
            handle_team_created(&name, &members, layout, registry, wakeup_tx);
        }
    }
}

fn handle_instance_created(
    name: &str,
    hint: LayoutHint,
    spawner: Option<&str>,
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
) {
    tracing::info!(agent = name, hint = ?hint, spawner = ?spawner, tabs_before = layout.tabs.len(), "handle_instance_created begin");
    if layout.tabs.iter().any(|tab| tab.root().has_agent(name)) {
        tracing::info!(agent = name, "handle_instance_created: agent already in layout, deduped");
        return;
    }

    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));

    // Resolve placement BEFORE attaching a pane. Each attach_pane call
    // subscribes to the agent's output and spawns a forwarder thread;
    // discarding an attached pane leaves an orphan subscription that lingers
    // until the agent next emits data (indefinite on idle agents). Pre-checking
    // ensures we only attach once.
    let split_target_idx = match hint {
        LayoutHint::SplitRight | LayoutHint::SplitBelow => spawner.and_then(|spawner_name| {
            layout
                .tabs
                .iter()
                .position(|tab| tab.root().has_agent(spawner_name))
        }),
        LayoutHint::Tab => None,
    };

    let pane = match attach_pane(
        name,
        registry,
        cols,
        rows.saturating_sub(4),
        wakeup_tx,
        layout,
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(agent = name, error = %e, "failed to attach pane for new instance");
            return;
        }
    };

    match (hint, split_target_idx) {
        (LayoutHint::SplitRight | LayoutHint::SplitBelow, Some(idx)) => {
            let dir = match hint {
                LayoutHint::SplitRight => SplitDir::Horizontal,
                _ => SplitDir::Vertical,
            };
            // split_focused consumes the pane. If the rare case of no focused
            // pane in the target tab occurs, the pane is lost — acceptable
            // since we've already validated the tab has the spawner agent.
            layout.tabs[idx].split_focused(dir, pane);
        }
        _ => {
            layout.add_tab(Tab::new(name.to_string(), pane));
        }
    }
}

fn handle_instance_deleted(name: &str, layout: &mut Layout) {
    remove_agent_pane(name, layout);
}

fn handle_team_created(
    team_name: &str,
    members: &[String],
    layout: &mut Layout,
    registry: &AgentRegistry,
    wakeup_tx: &crossbeam::channel::Sender<usize>,
) {
    tracing::info!(team = team_name, members = ?members, tabs_before = layout.tabs.len(), "handle_team_created begin");
    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let pane_rows = rows.saturating_sub(4);

    // Filter members in two passes:
    //   1. must exist in registry (spawn_agent completed)
    //   2. must NOT already be displayed in any tab — defensive guard against
    //      re-entry. Mirrors `handle_instance_created`'s dedup check. With
    //      per-member dedup in CREATE_TEAM this should never skip anyone, but
    //      the check keeps behavior safe if the API path changes.
    let (running, missing): (Vec<&str>, Vec<&str>) = {
        let reg = agent::lock_registry(registry);
        let (r, m): (Vec<_>, Vec<_>) = members
            .iter()
            .map(|m| m.as_str())
            .partition(|m| reg.contains_key(*m));
        (r, m)
    };
    if !missing.is_empty() {
        tracing::warn!(team = team_name, missing = ?missing, "handle_team_created: members not in registry, skipped");
    }
    let running: Vec<&str> = running
        .into_iter()
        .filter(|m| {
            let already = layout.tabs.iter().any(|tab| tab.root().has_agent(m));
            if already {
                tracing::warn!(team = team_name, member = m, "handle_team_created: member already in a tab, skipped");
            }
            !already
        })
        .collect();
    tracing::info!(team = team_name, running = ?running, "handle_team_created: filter complete");

    if running.is_empty() {
        tracing::warn!(team = team_name, "handle_team_created: no running members, no tab created");
        return;
    }

    let first_pane = match attach_pane(running[0], registry, cols, pane_rows, wakeup_tx, layout) {
        Ok(p) => {
            tracing::info!(team = team_name, first = running[0], "handle_team_created: first pane attached");
            p
        }
        Err(e) => {
            tracing::warn!(team = team_name, first = running[0], error = %e, "handle_team_created: first attach_pane failed, no tab created");
            return;
        }
    };

    let mut tab = Tab::new(team_name.to_string(), first_pane);
    let mut attached = 1usize;

    for member in &running[1..] {
        match attach_pane(member, registry, cols, pane_rows, wakeup_tx, layout) {
            Ok(pane) => {
                tab.split_focused(SplitDir::Horizontal, pane);
                attached += 1;
            }
            Err(e) => {
                tracing::warn!(team = team_name, member = member, error = %e, "handle_team_created: split attach_pane failed");
            }
        }
    }

    let panes_in_tab = tab.root().pane_count();
    layout.add_tab(tab);
    tracing::info!(
        team = team_name,
        expected = running.len(),
        attached,
        panes_in_tab,
        tabs_after = layout.tabs.len(),
        "handle_team_created end"
    );
}

/// Remove ALL panes for the given agent from every tab. Cleans up empty tabs.
fn remove_agent_pane(name: &str, layout: &mut Layout) {
    loop {
        let target = layout.tabs.iter().enumerate().find_map(|(tab_idx, tab)| {
            tab.root()
                .find_pane_id_by_agent(name)
                .map(|pane_id| (tab_idx, pane_id))
        });
        let (tab_idx, pane_id) = match target {
            Some(t) => t,
            None => break,
        };
        if layout.tabs[tab_idx].root().pane_count() <= 1 {
            layout.close_tab(tab_idx);
        } else {
            layout.tabs[tab_idx].close_pane_by_id(pane_id);
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

/// Kill an agent and remove from both registry and fleet.yaml.
fn kill_agent(registry: &AgentRegistry, name: &str) {
    let mut reg = agent::lock_registry(registry);
    if let Some(handle) = reg.get(name) {
        let mut child = handle.child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
    }
    reg.remove(name);
}

/// Generate a unique fleet instance name by checking fleet.yaml for collisions.
fn unique_fleet_name(home: &Path, base: &str) -> String {
    let Some(fleet) = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).ok() else {
        return base.to_string();
    };
    if !fleet.instances.contains_key(base) {
        return base.to_string();
    }
    // Infinite iterator over 2.. always finds a unique name
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !fleet.instances.contains_key(c))
        .expect("infinite iterator")
}

/// Look up the fleet_instance_name for an agent by scanning the layout.
fn lookup_fleet_name(layout: &Layout, agent_name: &str) -> Option<String> {
    for tab in &layout.tabs {
        for id in tab.root().pane_ids() {
            if let Some(pane) = tab.root().find_pane(id) {
                if pane.agent_name == agent_name {
                    return pane.fleet_instance_name.clone();
                }
            }
        }
    }
    None
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

fn start_api_server(home: &Path, registry: &AgentRegistry, tui_tx: TuiEventSender) -> ApiGuard {
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
            crate::api::serve(
                &api_home,
                api_registry,
                shutdown,
                configs,
                externals,
                Some(tui_tx),
            );
        })
        .ok();

    tracing::info!(path = %run.display(), "in-process API server started");
    ApiGuard { run_dir: Some(run) }
}

enum TabBarClick {
    Tab(usize),
    NewTab,
}

/// Hit-test the tab bar at the given column.
/// SYNC: layout math must match render_tab_bar() in render.rs.
fn tab_bar_hit_test(layout: &Layout, col: u16) -> Option<TabBarClick> {
    use unicode_width::UnicodeWidthStr;
    let mut x: u16 = 0;
    for (i, tab) in layout.tabs.iter().enumerate() {
        if i > 0 {
            x += 1;
        } // separator space
        let is_active = i == layout.active;
        let has_notif = tab.root().has_notification();
        let badge = if has_notif && !is_active { " !" } else { "" };
        let label = format!(" {}{badge} ", tab.name);
        let tab_w = 1 + label.width() as u16; // "*" + label
        if col >= x && col < x + tab_w {
            return Some(TabBarClick::Tab(i));
        }
        x += tab_w;
    }
    // " [+] " button
    if col >= x && col < x + 5 {
        return Some(TabBarClick::NewTab);
    }
    None
}

/// Handle mouse selection: down starts, drag extends, up copies to clipboard.
/// Works on any pane (not just focused) by finding the pane under the cursor.
fn handle_mouse_selection(layout: &mut Layout, mouse: &crossterm::event::MouseEvent) {
    let tab = match layout.active_tab_mut() {
        Some(t) => t,
        None => return,
    };

    // Down: hit-test pane_rects. Drag/Up: use cached selecting_pane.
    // When zoomed, only the focused pane is visible — skip hit-test.
    let target_id = match mouse.kind {
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            if tab.zoomed {
                Some(tab.focus_id)
            } else {
                tab.pane_rects
                    .iter()
                    .find(|(_, &(px, py, pw, ph))| {
                        mouse.column >= px
                            && mouse.column < px + pw
                            && mouse.row >= py
                            && mouse.row < py + ph
                    })
                    .map(|(&id, _)| id)
            }
        }
        _ => tab.selecting_pane,
    };
    let target_id = match target_id {
        Some(id) => id,
        None => return,
    };

    // Focus clicked pane and cache selection target on Down
    if matches!(
        mouse.kind,
        MouseEventKind::Down(crossterm::event::MouseButton::Left)
    ) {
        tab.focus_id = target_id;
        tab.selecting_pane = Some(target_id);
    }

    let rect = tab.pane_rects.get(&target_id).copied();
    let (px, py, pw, ph) = match rect {
        Some(r) => r,
        None => return,
    };
    let inner_x = px + 1;
    let inner_y = py + 1;
    let inner_w = pw.saturating_sub(2);
    let inner_h = ph.saturating_sub(2);
    if inner_w == 0 || inner_h == 0 {
        return;
    }

    // Scope the pane borrow so we can touch tab.selecting_pane afterwards.
    let finished = {
        let pane = match tab.root_mut().find_pane_mut(target_id) {
            Some(p) => p,
            None => return,
        };
        match mouse.kind {
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                if mouse.column >= inner_x
                    && mouse.column < inner_x + inner_w
                    && mouse.row >= inner_y
                    && mouse.row < inner_y + inner_h
                {
                    let col = mouse.column - inner_x;
                    let row = mouse.row - inner_y;
                    pane.selection = Some(crate::layout::Selection {
                        start: (row, col),
                        end: (row, col),
                    });
                }
                false
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                let col = mouse.column.max(inner_x).min(inner_x + inner_w - 1) - inner_x;
                let row = mouse.row.max(inner_y).min(inner_y + inner_h - 1) - inner_y;
                if let Some(ref mut sel) = pane.selection {
                    sel.end = (row, col);
                }
                false
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                if let Some(ref sel) = pane.selection {
                    let text = pane
                        .vterm
                        .extract_text(sel.start, sel.end, pane.scroll_offset);
                    if !text.is_empty() {
                        copy_to_clipboard(&text);
                    }
                }
                pane.selection = None;
                true
            }
            _ => false,
        }
    };

    if finished {
        tab.selecting_pane = None;
    }
}

/// Copy text to system clipboard (macOS / Linux / Windows).
fn copy_to_clipboard(text: &str) {
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
        Ok(()) => {}
        Err(e) => tracing::warn!(error = %e, "clipboard copy failed"),
    }
}
