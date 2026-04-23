//! Modal overlays — the non-pane UI elements (menus, rename prompts, help,
//! command palette, etc.). Key input flows through `handle_key` which mutates
//! the overlay and surrounding state via `OverlayCtx`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::agent::AgentRegistry;
use crate::backend::Backend;
use crate::layout::{Layout, Pane, SplitDir, Tab};

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

pub(super) enum CloseTarget {
    Pane,
    Tab,
}

pub enum TaskBoardMode {
    Board,
    Detail,
    NewTask {
        input: String,
    },
    Assign {
        /// (display_label, assignee_value)
        choices: Vec<(String, String)>,
        selected: usize,
    },
    Help,
}

pub(super) enum Overlay {
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
    /// Move-pane destination picker. Lists all tabs plus a trailing
    /// "[+] New tab" slot. Enter applies `Layout::move_pane_across_tabs`
    /// with the focused pane as source.
    MovePaneTarget {
        /// 0..tabs.len() → move into that tab. == tabs.len() → new tab.
        selected: usize,
        /// Source pane to move. Captured on overlay open so a subsequent
        /// mouse click that changes focus (while the overlay is modal,
        /// this shouldn't happen, but be defensive) can't retarget the move.
        source_pane_id: usize,
        /// Source tab index at overlay-open time. Same defensive capture as
        /// `source_pane_id`; `move_pane_across_tabs` re-verifies existence.
        source_tab_idx: usize,
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
    /// Task board overlay — 4-column kanban view with CRUD.
    Tasks {
        items: Vec<crate::tasks::Task>,
        /// Currently focused column (0=Backlog, 1=Open, 2=InProgress, 3=Done).
        col: usize,
        /// Currently focused row within the column.
        row: usize,
        /// Sub-mode: None=board, Detail, NewTask(input), Assign(selected).
        mode: TaskBoardMode,
    },
    /// Floating scratch shell (Ctrl+B ~). Esc kills the shell and closes the
    /// overlay. Pane is boxed because it's much larger than any other variant.
    ScratchShell {
        pane: Box<Pane>,
    },
}

/// Handle j/k/PgUp/PgDn scroll for list overlays. Returns true if handled, false to close.
pub(super) fn handle_list_scroll(key: KeyCode, scroll: &mut usize, len: usize) -> bool {
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

/// Bundle of mutable references passed to `handle_key` so overlay handlers can
/// affect layout / registry / telegram state / name counter without the event
/// loop needing a dedicated arg per overlay variant.
pub(super) struct OverlayCtx<'a> {
    pub layout: &'a mut Layout,
    pub registry: &'a AgentRegistry,
    pub home: &'a Path,
    pub fleet_path: &'a Path,
    pub wakeup_tx: &'a crossbeam::channel::Sender<usize>,
    pub name_counter: &'a mut HashMap<String, usize>,
    pub telegram_state: &'a Option<Arc<dyn crate::channel::Channel>>,
}

#[derive(Default)]
pub(super) struct OverlayOutcome {
    /// Layout changed in a way that requires a resize pass before next draw.
    pub needs_resize: bool,
}

/// Dispatch a key press to the currently-active overlay. Mutates `*overlay`
/// (including replacing it with `Overlay::None` to close) and may mutate
/// layout / registry state via `ctx`. Caller is responsible for only invoking
/// this when `!matches!(overlay, Overlay::None)`.
pub(super) fn handle_key(
    overlay: &mut Overlay,
    key: KeyEvent,
    ctx: &mut OverlayCtx<'_>,
) -> OverlayOutcome {
    let mut outcome = OverlayOutcome::default();
    match overlay {
        Overlay::NewTabMenu {
            items,
            ref mut selected,
        } => match key.code {
            KeyCode::Up | KeyCode::Char('k') if *selected > 0 => {
                *selected -= 1;
            }
            KeyCode::Down | KeyCode::Char('j') if *selected + 1 < items.len() => {
                *selected += 1;
            }
            KeyCode::Enter => {
                let sel = *selected;
                let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
                if let Overlay::NewTabMenu { items, .. } = std::mem::replace(overlay, Overlay::None)
                {
                    if let Some(item) = items.into_iter().nth(sel) {
                        let pc = c.saturating_sub(2);
                        let pr = r.saturating_sub(4);
                        if let Ok(pane) = super::pane_from_menu_item(
                            item,
                            ctx.fleet_path,
                            ctx.layout,
                            ctx.registry,
                            ctx.home,
                            pc,
                            pr,
                            ctx.wakeup_tx,
                            ctx.name_counter,
                        ) {
                            super::telegram_hooks::maybe_create_telegram_topic(
                                ctx.telegram_state,
                                ctx.registry,
                                ctx.home,
                                &pane,
                            );
                            let tab_name = pane.agent_name.clone();
                            ctx.layout.add_tab(Tab::new(tab_name, pane));
                            outcome.needs_resize = true;
                        }
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                *overlay = Overlay::None;
            }
            _ => {}
        },
        Overlay::SplitMenu {
            items,
            ref mut selected,
            dir,
        } => {
            let split_dir = *dir;
            match key.code {
                KeyCode::Up | KeyCode::Char('k') if *selected > 0 => {
                    *selected -= 1;
                }
                KeyCode::Down | KeyCode::Char('j') if *selected + 1 < items.len() => {
                    *selected += 1;
                }
                KeyCode::Enter => {
                    let sel = *selected;
                    let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
                    if let Overlay::SplitMenu { items, .. } =
                        std::mem::replace(overlay, Overlay::None)
                    {
                        if let Some(item) = items.into_iter().nth(sel) {
                            let (pc, pr) = match split_dir {
                                SplitDir::Vertical => {
                                    (c.saturating_sub(2) / 2, r.saturating_sub(4))
                                }
                                SplitDir::Horizontal => {
                                    (c.saturating_sub(2), r.saturating_sub(4) / 2)
                                }
                            };
                            match super::pane_from_menu_item(
                                item,
                                ctx.fleet_path,
                                ctx.layout,
                                ctx.registry,
                                ctx.home,
                                pc,
                                pr,
                                ctx.wakeup_tx,
                                ctx.name_counter,
                            ) {
                                Ok(p) => {
                                    super::telegram_hooks::maybe_create_telegram_topic(
                                        ctx.telegram_state,
                                        ctx.registry,
                                        ctx.home,
                                        &p,
                                    );
                                    if let Some(tab) = ctx.layout.active_tab_mut() {
                                        tab.split_focused(split_dir, p);
                                    }
                                    outcome.needs_resize = true;
                                }
                                Err(e) => tracing::error!(error = %e, "split failed"),
                            }
                        }
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    *overlay = Overlay::None;
                }
                _ => {}
            }
        }
        Overlay::RenameTab { ref mut input } => match key.code {
            KeyCode::Enter => {
                let new_name = input.clone();
                if !new_name.is_empty() {
                    if let Some(tab) = ctx.layout.active_tab_mut() {
                        tab.name = new_name;
                    }
                }
                *overlay = Overlay::None;
            }
            KeyCode::Esc => {
                *overlay = Overlay::None;
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) => {
                input.push(c);
            }
            _ => {}
        },
        Overlay::RenamePane { ref mut input } => match key.code {
            KeyCode::Enter => {
                let new_name = input.clone();
                if let Some(tab) = ctx.layout.active_tab_mut() {
                    let fid = tab.focus_id;
                    if let Some(pane) = tab.root_mut().find_pane_mut(fid) {
                        pane.display_name = if new_name.is_empty() {
                            None // clear → revert to agent_name
                        } else {
                            Some(new_name)
                        };
                    }
                }
                *overlay = Overlay::None;
            }
            KeyCode::Esc => {
                *overlay = Overlay::None;
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) => {
                input.push(c);
            }
            _ => {}
        },
        Overlay::ConfirmClose { target } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let is_tab = matches!(target, CloseTarget::Tab);
                *overlay = Overlay::None;
                if is_tab {
                    let idx = ctx.layout.active;
                    let closed: Vec<(String, Option<std::path::PathBuf>)> = ctx
                        .layout
                        .tabs
                        .get(idx)
                        .into_iter()
                        .flat_map(|t| {
                            t.root().pane_ids().into_iter().filter_map(|id| {
                                t.root().find_pane(id).and_then(|p| {
                                    p.fleet_instance_name
                                        .clone()
                                        .map(|name| (name, p.working_dir.clone()))
                                })
                            })
                        })
                        .collect();
                    for (name, _) in &closed {
                        super::telegram_hooks::maybe_delete_telegram_topic(
                            ctx.telegram_state,
                            ctx.home,
                            name,
                        );
                    }
                    if !closed.is_empty() {
                        let names: Vec<String> = closed.iter().map(|(n, _)| n.clone()).collect();
                        let _ = crate::fleet::remove_instances_from_yaml(ctx.home, &names);
                    }
                    if let Some(tab) = ctx.layout.close_tab(idx) {
                        for name in tab.root().agent_names() {
                            super::kill_agent(ctx.registry, &name);
                        }
                    }
                    for (name, wd) in &closed {
                        if let Some(wd) = wd {
                            crate::agent_ops::cleanup_working_dir(ctx.home, name, wd);
                        }
                    }
                    outcome.needs_resize = true;
                } else if let Some(tab) = ctx.layout.active_tab_mut() {
                    let fid = tab.focus_id;
                    let closed: Option<(String, Option<std::path::PathBuf>)> =
                        tab.root().find_pane(fid).and_then(|p| {
                            p.fleet_instance_name
                                .clone()
                                .map(|name| (name, p.working_dir.clone()))
                        });
                    if let Some((ref fleet_name, _)) = closed {
                        super::telegram_hooks::maybe_delete_telegram_topic(
                            ctx.telegram_state,
                            ctx.home,
                            fleet_name,
                        );
                        let _ = crate::fleet::remove_instance_from_yaml(ctx.home, fleet_name);
                    }
                    if let Some(name) = tab.close_focused() {
                        super::kill_agent(ctx.registry, &name);
                        outcome.needs_resize = true;
                    }
                    if let Some((name, Some(wd))) = closed {
                        crate::agent_ops::cleanup_working_dir(ctx.home, &name, &wd);
                    }
                }
            }
            _ => {
                *overlay = Overlay::None;
            }
        },
        Overlay::TabList { ref mut selected } => match key.code {
            KeyCode::Up | KeyCode::Char('k') if *selected > 0 => {
                *selected -= 1;
            }
            KeyCode::Down | KeyCode::Char('j') if *selected + 1 < ctx.layout.tabs.len() => {
                *selected += 1;
            }
            KeyCode::Enter => {
                ctx.layout.goto_tab(*selected);
                *overlay = Overlay::None;
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                *overlay = Overlay::None;
            }
            _ => {}
        },
        Overlay::MovePaneTarget {
            ref mut selected,
            source_pane_id,
            source_tab_idx,
        } => {
            // List length = tabs.len() + 1 (trailing "New tab" entry). The
            // "New tab" slot is always valid; tab slots other than source
            // are valid move targets. `selected` can legally equal tabs.len().
            let list_len = ctx.layout.tabs.len() + 1;
            match key.code {
                KeyCode::Up | KeyCode::Char('k') if *selected > 0 => {
                    *selected -= 1;
                }
                KeyCode::Down | KeyCode::Char('j') if *selected + 1 < list_len => {
                    *selected += 1;
                }
                KeyCode::Enter => {
                    let src_pane = *source_pane_id;
                    let src_tab = *source_tab_idx;
                    let sel = *selected;
                    let tabs_count = ctx.layout.tabs.len();
                    *overlay = Overlay::None;
                    if sel == tabs_count {
                        // New tab: name after the pane's agent for a sensible default.
                        let name = ctx
                            .layout
                            .tabs
                            .get(src_tab)
                            .and_then(|t| t.root().find_pane(src_pane))
                            .map(|p| p.agent_name.clone())
                            .unwrap_or_else(|| "new".to_string());
                        if ctx
                            .layout
                            .move_pane_across_tabs(
                                src_tab,
                                src_pane,
                                crate::layout::MovePlacement::NewTab { name },
                            )
                            .is_some()
                        {
                            outcome.needs_resize = true;
                        }
                    } else if sel != src_tab {
                        if let Some(new_idx) = ctx.layout.move_pane_across_tabs(
                            src_tab,
                            src_pane,
                            crate::layout::MovePlacement::SplitFocused {
                                to_tab: sel,
                                dir: crate::layout::SplitDir::Horizontal,
                            },
                        ) {
                            // Follow the pane — the user just pointed at this tab.
                            ctx.layout.goto_tab(new_idx);
                            outcome.needs_resize = true;
                        }
                    }
                    // sel == src_tab: no-op (already in that tab).
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    *overlay = Overlay::None;
                }
                _ => {}
            }
        }
        Overlay::Help => {
            *overlay = Overlay::None;
        }
        Overlay::Scroll => match key.code {
            KeyCode::Up | KeyCode::Char('k') => super::scroll_focused(ctx.layout, 1),
            KeyCode::Down | KeyCode::Char('j') => super::scroll_focused(ctx.layout, -1),
            KeyCode::PageUp => super::scroll_focused(ctx.layout, 20),
            KeyCode::PageDown => super::scroll_focused(ctx.layout, -20),
            KeyCode::Char('q') | KeyCode::Esc => {
                *overlay = Overlay::None;
            }
            _ => {}
        },
        Overlay::Command { ref mut input } => match key.code {
            KeyCode::Enter => {
                let cmd = input.clone();
                *overlay = Overlay::None;
                let mut cctx = super::commands::CommandCtx {
                    layout: &mut *ctx.layout,
                    registry: ctx.registry,
                    home: ctx.home,
                    wakeup_tx: ctx.wakeup_tx,
                    name_counter: &mut *ctx.name_counter,
                    telegram_state: ctx.telegram_state,
                };
                if super::commands::execute(&cmd, &mut cctx) {
                    outcome.needs_resize = true;
                }
            }
            KeyCode::Esc => {
                *overlay = Overlay::None;
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) => {
                input.push(c);
            }
            _ => {}
        },
        Overlay::Decisions {
            items,
            ref mut scroll,
        } => {
            if !handle_list_scroll(key.code, scroll, items.len()) {
                *overlay = Overlay::None;
            }
        }
        Overlay::Tasks {
            ref mut items,
            ref mut col,
            ref mut row,
            ref mut mode,
        } => {
            match mode {
                TaskBoardMode::Detail => {
                    if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                        *mode = TaskBoardMode::Board;
                    }
                }
                TaskBoardMode::NewTask { ref mut input } => match key.code {
                    KeyCode::Enter if !input.is_empty() => {
                        crate::tasks::handle(
                            ctx.home,
                            "user",
                            &serde_json::json!({
                                "action": "create",
                                "title": input.as_str(),
                                "priority": "normal",
                            }),
                        );
                        *items = crate::tasks::list_all(ctx.home);
                        *mode = TaskBoardMode::Board;
                    }
                    KeyCode::Char(c) => input.push(c),
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Esc => {
                        *mode = TaskBoardMode::Board;
                    }
                    _ => {}
                },
                TaskBoardMode::Assign {
                    ref choices,
                    ref mut selected,
                } => match key.code {
                    KeyCode::Up | KeyCode::Char('k') if *selected > 0 => {
                        *selected -= 1;
                    }
                    KeyCode::Down | KeyCode::Char('j') if *selected + 1 < choices.len() => {
                        *selected += 1;
                    }
                    KeyCode::Enter if !choices.is_empty() => {
                        let columns = crate::render::task_board_columns(items);
                        if let Some(task) = columns[*col].get(*row) {
                            let assignee = &choices[*selected].1;
                            crate::tasks::handle(
                                ctx.home,
                                "user",
                                &serde_json::json!({
                                    "action": "update",
                                    "id": task.id,
                                    "assignee": assignee,
                                }),
                            );
                            *items = crate::tasks::list_all(ctx.home);
                        }
                        *mode = TaskBoardMode::Board;
                    }
                    KeyCode::Esc => {
                        *mode = TaskBoardMode::Board;
                    }
                    _ => {}
                },
                TaskBoardMode::Help => match key.code {
                    KeyCode::Esc | KeyCode::Char('?') => {
                        *mode = TaskBoardMode::Board;
                    }
                    _ => {}
                },
                TaskBoardMode::Board => {
                    let columns = crate::render::task_board_columns(items);
                    match key.code {
                        KeyCode::Left | KeyCode::Char('h')
                            if !key.modifiers.contains(KeyModifiers::SHIFT) && *col > 0 =>
                        {
                            *col -= 1;
                            *row = (*row).min(columns[*col].len().saturating_sub(1));
                        }
                        KeyCode::Right | KeyCode::Char('l')
                            if !key.modifiers.contains(KeyModifiers::SHIFT) && *col < 3 =>
                        {
                            *col += 1;
                            *row = (*row).min(columns[*col].len().saturating_sub(1));
                        }
                        KeyCode::Up | KeyCode::Char('k') if *row > 0 => {
                            *row -= 1;
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            let col_len = columns[*col].len();
                            if *row + 1 < col_len {
                                *row += 1;
                            }
                        }
                        KeyCode::Enter if !columns[*col].is_empty() => {
                            *mode = TaskBoardMode::Detail;
                        }
                        // n — new task
                        KeyCode::Char('n') => {
                            *mode = TaskBoardMode::NewTask {
                                input: String::new(),
                            };
                        }
                        // d — cancel task
                        KeyCode::Char('d')
                            if !key.modifiers.contains(KeyModifiers::SHIFT)
                                && !columns[*col].is_empty() =>
                        {
                            if let Some(task) = columns[*col].get(*row) {
                                crate::tasks::handle(
                                    ctx.home,
                                    "user",
                                    &serde_json::json!({
                                        "action": "update",
                                        "id": task.id,
                                        "status": "cancelled",
                                    }),
                                );
                                *items = crate::tasks::list_all(ctx.home);
                                let new_cols = crate::render::task_board_columns(items);
                                *row = (*row).min(new_cols[*col].len().saturating_sub(1));
                            }
                        }
                        // D (Shift+D) — mark task done from any column
                        // Match 'D' (legacy terminals) and 'd'+SHIFT (Kitty protocol)
                        KeyCode::Char('D') | KeyCode::Char('d') if !columns[*col].is_empty() => {
                            if let Some(task) = columns[*col].get(*row) {
                                crate::tasks::handle(
                                    ctx.home,
                                    "user",
                                    &serde_json::json!({
                                        "action": "done",
                                        "id": task.id,
                                    }),
                                );
                                *items = crate::tasks::list_all(ctx.home);
                                let new_cols = crate::render::task_board_columns(items);
                                *row = (*row).min(new_cols[*col].len().saturating_sub(1));
                            }
                        }
                        // a — assign
                        KeyCode::Char('a') if !columns[*col].is_empty() => {
                            let mut choices: Vec<(String, String)> = Vec::new();
                            let mut seen = std::collections::HashSet::new();
                            // Teams
                            let team_list = crate::teams::list(ctx.home);
                            if let Some(teams) = team_list["teams"].as_array() {
                                for t in teams {
                                    if let Some(name) = t["name"].as_str() {
                                        choices.push((format!("🏷 {name}"), name.to_string()));
                                        seen.insert(name.to_string());
                                    }
                                }
                            }
                            // All instances (from metadata dir)
                            let meta_dir = ctx.home.join("metadata");
                            if let Ok(entries) = std::fs::read_dir(&meta_dir) {
                                for entry in entries.flatten() {
                                    if let Some(name) =
                                        entry.path().file_stem().and_then(|s| s.to_str())
                                    {
                                        if !seen.contains(name) {
                                            choices.push((name.to_string(), name.to_string()));
                                            seen.insert(name.to_string());
                                        }
                                    }
                                }
                            }
                            if choices.is_empty() {
                                choices.push(("(no agents/teams)".to_string(), String::new()));
                            }
                            *mode = TaskBoardMode::Assign {
                                choices,
                                selected: 0,
                            };
                        }
                        // Shift+← / Shift+→ — move task status
                        // Match 'H' (legacy) and 'h'+SHIFT (Kitty protocol)
                        KeyCode::Char('H') | KeyCode::Char('h')
                            if !columns[*col].is_empty() && *col > 0 =>
                        {
                            if let Some(task) = columns[*col].get(*row) {
                                let update = match *col {
                                    1 => Some(("priority", "low")),   // Open → Backlog
                                    2 => Some(("status", "open")),    // InProgress → Open
                                    3 => Some(("status", "claimed")), // Done → InProgress
                                    _ => None,
                                };
                                if let Some((field, val)) = update {
                                    crate::tasks::handle(
                                        ctx.home,
                                        "user",
                                        &serde_json::json!({"action": "update", "id": task.id, field: val}),
                                    );
                                    *items = crate::tasks::list_all(ctx.home);
                                    *col -= 1;
                                    let new_cols = crate::render::task_board_columns(items);
                                    *row = (*row).min(new_cols[*col].len().saturating_sub(1));
                                }
                            }
                        }
                        // Match 'L' (legacy) and 'l'+SHIFT (Kitty protocol)
                        KeyCode::Char('L') | KeyCode::Char('l')
                            if !columns[*col].is_empty() && *col < 3 =>
                        {
                            if let Some(task) = columns[*col].get(*row) {
                                let update = match *col {
                                    0 => Some(("priority", "normal")), // Backlog → Open
                                    1 => Some(("status", "claimed")),  // Open → InProgress
                                    2 => Some(("status", "done")),     // InProgress → Done
                                    _ => None,
                                };
                                if let Some((field, val)) = update {
                                    crate::tasks::handle(
                                        ctx.home,
                                        "user",
                                        &serde_json::json!({"action": "update", "id": task.id, field: val}),
                                    );
                                    *items = crate::tasks::list_all(ctx.home);
                                    *col += 1;
                                    let new_cols = crate::render::task_board_columns(items);
                                    *row = (*row).min(new_cols[*col].len().saturating_sub(1));
                                }
                            }
                        }
                        KeyCode::Char('?') => {
                            *mode = TaskBoardMode::Help;
                        }
                        KeyCode::Esc | KeyCode::Char('q') => {
                            *overlay = Overlay::None;
                        }
                        _ => {}
                    }
                }
            }
        }
        Overlay::ScratchShell { pane } => match key.code {
            KeyCode::Esc => {
                // Capture the agent name before dropping the pane so we can
                // kill the shell process. The registry owns the PTY master;
                // kill_agent drops it, the forwarder thread sees rx close,
                // and exits. The pane (and its subscriber rx) drop with the
                // overlay.
                let name = pane.agent_name.clone();
                *overlay = Overlay::None;
                super::kill_agent(ctx.registry, &name);
            }
            _ => {
                let bytes = crate::tui::key_to_bytes(key.code, key.modifiers);
                if !bytes.is_empty() {
                    pane.write_input(ctx.registry, &bytes);
                }
            }
        },
        Overlay::None => {}
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn task_overlay() -> Overlay {
        Overlay::Tasks {
            items: Vec::new(),
            col: 0,
            row: 0,
            mode: TaskBoardMode::Board,
        }
    }

    fn get_mode(overlay: &Overlay) -> &TaskBoardMode {
        match overlay {
            Overlay::Tasks { mode, .. } => mode,
            _ => panic!("expected Tasks overlay"),
        }
    }

    #[test]
    fn task_board_question_mark_shows_help() {
        let home = std::env::temp_dir().join("overlay_test_help_show");
        std::fs::create_dir_all(&home).ok();
        let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = crossbeam::channel::unbounded();
        let mut name_counter = HashMap::new();
        let tg: Option<Arc<dyn crate::channel::Channel>> = None;
        let mut layout = crate::layout::Layout::new();
        let mut ctx = OverlayCtx {
            layout: &mut layout,
            registry: &registry,
            home: &home,
            fleet_path: &home,
            wakeup_tx: &tx,
            name_counter: &mut name_counter,
            telegram_state: &tg,
        };
        let mut overlay = task_overlay();
        handle_key(&mut overlay, press(KeyCode::Char('?')), &mut ctx);
        assert!(matches!(get_mode(&overlay), TaskBoardMode::Help));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_board_help_esc_returns_to_board() {
        let home = std::env::temp_dir().join("overlay_test_help_esc");
        std::fs::create_dir_all(&home).ok();
        let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = crossbeam::channel::unbounded();
        let mut name_counter = HashMap::new();
        let tg: Option<Arc<dyn crate::channel::Channel>> = None;
        let mut layout = crate::layout::Layout::new();
        let mut ctx = OverlayCtx {
            layout: &mut layout,
            registry: &registry,
            home: &home,
            fleet_path: &home,
            wakeup_tx: &tx,
            name_counter: &mut name_counter,
            telegram_state: &tg,
        };
        let mut overlay = Overlay::Tasks {
            items: Vec::new(),
            col: 0,
            row: 0,
            mode: TaskBoardMode::Help,
        };
        handle_key(&mut overlay, press(KeyCode::Esc), &mut ctx);
        assert!(matches!(get_mode(&overlay), TaskBoardMode::Board));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_board_help_question_mark_toggles() {
        let home = std::env::temp_dir().join("overlay_test_help_toggle");
        std::fs::create_dir_all(&home).ok();
        let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = crossbeam::channel::unbounded();
        let mut name_counter = HashMap::new();
        let tg: Option<Arc<dyn crate::channel::Channel>> = None;
        let mut layout = crate::layout::Layout::new();
        let mut ctx = OverlayCtx {
            layout: &mut layout,
            registry: &registry,
            home: &home,
            fleet_path: &home,
            wakeup_tx: &tx,
            name_counter: &mut name_counter,
            telegram_state: &tg,
        };
        let mut overlay = Overlay::Tasks {
            items: Vec::new(),
            col: 0,
            row: 0,
            mode: TaskBoardMode::Help,
        };
        handle_key(&mut overlay, press(KeyCode::Char('?')), &mut ctx);
        assert!(matches!(get_mode(&overlay), TaskBoardMode::Board));
        std::fs::remove_dir_all(&home).ok();
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-overlay-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn make_ctx<'a>(
        home: &'a std::path::Path,
        layout: &'a mut crate::layout::Layout,
        registry: &'a crate::agent::AgentRegistry,
        tx: &'a crossbeam::channel::Sender<usize>,
        name_counter: &'a mut HashMap<String, usize>,
        tg: &'a Option<Arc<dyn crate::channel::Channel>>,
    ) -> OverlayCtx<'a> {
        OverlayCtx {
            layout,
            registry,
            home,
            fleet_path: home,
            wakeup_tx: tx,
            name_counter,
            telegram_state: tg,
        }
    }

    /// Regression: Shift+L (Kitty protocol: 'l'+SHIFT) must update task
    /// status and persist to tasks.json.
    #[test]
    fn task_board_l_updates_task_status_and_persists() {
        let home = tmp_home("l_persist");
        let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = crossbeam::channel::unbounded();
        let mut name_counter = HashMap::new();
        let tg: Option<Arc<dyn crate::channel::Channel>> = None;
        let mut layout = crate::layout::Layout::new();

        // Create a task in Open column (priority=normal, status=open)
        crate::tasks::handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "test", "priority": "normal"}),
        );
        let items = crate::tasks::list_all(&home);
        let task_id = items[0].id.clone();

        let mut overlay = Overlay::Tasks {
            items,
            col: 1,
            row: 0,
            mode: TaskBoardMode::Board,
        };

        // Kitty protocol: Shift+L → KeyCode::Char('l') + SHIFT
        let mut ctx = make_ctx(&home, &mut layout, &registry, &tx, &mut name_counter, &tg);
        handle_key(&mut overlay, shift(KeyCode::Char('l')), &mut ctx);

        // Reload from disk — must be persisted
        let reloaded = crate::tasks::list_all(&home);
        let task = reloaded.iter().find(|t| t.id == task_id).expect("task");
        assert_eq!(task.status, "claimed", "L must persist status change");

        std::fs::remove_dir_all(&home).ok();
    }

    /// Regression: Shift+D must mark task done from any column.
    /// Tests both Kitty protocol ('d'+SHIFT) and legacy ('D') key events.
    #[test]
    fn task_board_shift_d_marks_done_from_any_column() {
        for (label, key_event) in [
            ("kitty", shift(KeyCode::Char('d'))),
            ("legacy", press(KeyCode::Char('D'))),
        ] {
            let home = tmp_home(&format!("shift_d_{label}"));
            let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
            let (tx, _rx) = crossbeam::channel::unbounded();
            let mut name_counter = HashMap::new();
            let tg: Option<Arc<dyn crate::channel::Channel>> = None;
            let mut layout = crate::layout::Layout::new();

            crate::tasks::handle(
                &home,
                "user",
                &serde_json::json!({"action": "create", "title": "t", "priority": "normal"}),
            );
            let items = crate::tasks::list_all(&home);
            let task_id = items[0].id.clone();

            let mut overlay = Overlay::Tasks {
                items,
                col: 1,
                row: 0,
                mode: TaskBoardMode::Board,
            };

            let mut ctx = make_ctx(&home, &mut layout, &registry, &tx, &mut name_counter, &tg);
            handle_key(&mut overlay, key_event, &mut ctx);

            let reloaded = crate::tasks::list_all(&home);
            let task = reloaded.iter().find(|t| t.id == task_id).expect("task");
            assert_eq!(task.status, "done", "Shift+D ({label}) must mark done");

            std::fs::remove_dir_all(&home).ok();
        }
    }

    /// Regression: cursor index must resolve the correct task after
    /// filter/sort. Shift+L on row N must move the Nth task in the
    /// column, not a different one.
    #[test]
    fn task_board_cursor_resolves_correct_task_index() {
        let home = tmp_home("cursor_resolve");
        let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = crossbeam::channel::unbounded();
        let mut name_counter = HashMap::new();
        let tg: Option<Arc<dyn crate::channel::Channel>> = None;
        let mut layout = crate::layout::Layout::new();

        // Create two tasks in Open column with different priorities
        crate::tasks::handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "low-pri", "priority": "normal"}),
        );
        // Sleep >1s to guarantee distinct task IDs (second-level precision)
        std::thread::sleep(std::time::Duration::from_millis(1100));
        crate::tasks::handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "high-pri", "priority": "high"}),
        );

        let items = crate::tasks::list_all(&home);
        assert_eq!(items.len(), 2);

        // After sort: high-pri is row 0, low-pri is row 1
        let cols = crate::render::task_board_columns(&items);
        assert_eq!(cols[1][0].title, "high-pri");
        assert_eq!(cols[1][1].title, "low-pri");
        let high_id = cols[1][0].id.clone();

        // Move cursor to row 0 (high-pri), press Shift+L
        let mut overlay = Overlay::Tasks {
            items,
            col: 1,
            row: 0,
            mode: TaskBoardMode::Board,
        };

        let mut ctx = make_ctx(&home, &mut layout, &registry, &tx, &mut name_counter, &tg);
        handle_key(&mut overlay, shift(KeyCode::Char('l')), &mut ctx);

        // high-pri must have moved, low-pri must stay
        let reloaded = crate::tasks::list_all(&home);
        let high = reloaded.iter().find(|t| t.id == high_id).expect("high-pri");
        assert_eq!(
            high.status, "claimed",
            "cursor row 0 must move high-pri task"
        );
        let low = reloaded
            .iter()
            .find(|t| t.title == "low-pri")
            .expect("low-pri");
        assert_eq!(low.status, "open", "low-pri must remain in Open");

        std::fs::remove_dir_all(&home).ok();
    }
}
