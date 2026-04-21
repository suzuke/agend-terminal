//! Modal overlays — the non-pane UI elements (menus, rename prompts, help,
//! command palette, etc.). Key input flows through `handle_key` which mutates
//! the overlay and surrounding state via `OverlayCtx`.

use crossterm::event::{KeyCode, KeyEvent};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::agent::AgentRegistry;
use crate::backend::Backend;
use crate::layout::{Layout, SplitDir, Tab};

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
    pub telegram_state: &'a Option<Arc<Mutex<crate::telegram::TelegramState>>>,
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
            items,
            ref mut scroll,
        } => {
            if !handle_list_scroll(key.code, scroll, items.len()) {
                *overlay = Overlay::None;
            }
        }
        Overlay::None => {}
    }
    outcome
}
