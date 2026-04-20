//! Keybinding action dispatcher — translates a resolved `Action` into a
//! layout/overlay/break side-effect. Extracted from `run_app` so the main
//! loop is pure orchestration.

use std::path::Path;

use crate::agent::AgentRegistry;
use crate::keybinds::Action;
use crate::layout::{Layout, SplitDir};

use super::overlay::{CloseTarget, Overlay};

/// Borrowed state dispatch needs. `last_tab` is `&mut usize` because tab-
/// switch arms read the current active tab into it before moving away.
pub(super) struct DispatchCtx<'a> {
    pub layout: &'a mut Layout,
    pub registry: &'a AgentRegistry,
    pub home: &'a Path,
    pub fleet_path: &'a Path,
    pub last_tab: &'a mut usize,
}

/// Signals back to `run_app`. Fields are applied independently — a single
/// action can both open an overlay and request a resize.
#[derive(Default)]
pub(super) struct DispatchResult {
    pub needs_resize: bool,
    pub new_overlay: Option<Overlay>,
    /// `Action::Detach` sets this; caller breaks the event loop.
    pub should_break: bool,
}

/// Apply one action. Caller must have already drained overlay input — this
/// is only called when no overlay is active.
pub(super) fn dispatch(action: Action, ctx: &mut DispatchCtx<'_>) -> DispatchResult {
    let mut out = DispatchResult::default();
    match action {
        Action::Forward(key) => {
            let bytes = crate::tui::key_to_bytes(key.code, key.modifiers);
            if !bytes.is_empty() {
                super::write_to_focused(ctx.layout, ctx.registry, &bytes);
            }
        }
        Action::NewTab => {
            out.new_overlay = Some(Overlay::NewTabMenu {
                items: super::build_menu_items(ctx.fleet_path, ctx.registry),
                selected: 0,
            });
        }
        Action::NextTab => {
            *ctx.last_tab = ctx.layout.active;
            ctx.layout.next_tab();
            out.needs_resize = true;
        }
        Action::PrevTab => {
            *ctx.last_tab = ctx.layout.active;
            ctx.layout.prev_tab();
            out.needs_resize = true;
        }
        Action::LastTab => {
            let current = ctx.layout.active;
            ctx.layout.goto_tab(*ctx.last_tab);
            *ctx.last_tab = current;
            out.needs_resize = true;
        }
        Action::GotoTab(idx) => {
            *ctx.last_tab = ctx.layout.active;
            ctx.layout.goto_tab(idx);
            out.needs_resize = true;
        }
        Action::RenamePane => {
            let current = ctx
                .layout
                .active_tab()
                .and_then(|t| t.focused_pane())
                .map(|p| p.label().to_string())
                .unwrap_or_default();
            out.new_overlay = Some(Overlay::RenamePane { input: current });
        }
        Action::RenameTab => {
            let current_name = ctx
                .layout
                .active_tab()
                .map(|t| t.name.clone())
                .unwrap_or_default();
            out.new_overlay = Some(Overlay::RenameTab {
                input: current_name,
            });
        }
        Action::ListTabs => {
            out.new_overlay = Some(Overlay::TabList {
                selected: ctx.layout.active,
            });
        }
        Action::SplitVertical => {
            out.new_overlay = Some(Overlay::SplitMenu {
                items: super::build_menu_items(ctx.fleet_path, ctx.registry),
                selected: 0,
                dir: SplitDir::Vertical,
            });
        }
        Action::SplitHorizontal => {
            out.new_overlay = Some(Overlay::SplitMenu {
                items: super::build_menu_items(ctx.fleet_path, ctx.registry),
                selected: 0,
                dir: SplitDir::Horizontal,
            });
        }
        Action::CycleFocus => {
            if let Some(tab) = ctx.layout.active_tab_mut() {
                tab.cycle_focus();
            }
        }
        Action::ClosePane => {
            // A single-pane tab has nothing to pane-close — promote to tab close
            // so the confirm prompt accurately warns about killing the agent.
            let target = if ctx
                .layout
                .active_tab()
                .is_some_and(|t| t.root().pane_count() <= 1)
            {
                CloseTarget::Tab
            } else {
                CloseTarget::Pane
            };
            out.new_overlay = Some(Overlay::ConfirmClose { target });
        }
        Action::CloseTab => {
            out.new_overlay = Some(Overlay::ConfirmClose {
                target: CloseTarget::Tab,
            });
        }
        Action::FocusUp => {
            if let Some(tab) = ctx.layout.active_tab_mut() {
                tab.focus_direction(crate::layout::Direction::Up);
            }
        }
        Action::FocusDown => {
            if let Some(tab) = ctx.layout.active_tab_mut() {
                tab.focus_direction(crate::layout::Direction::Down);
            }
        }
        Action::FocusLeft => {
            if let Some(tab) = ctx.layout.active_tab_mut() {
                tab.focus_direction(crate::layout::Direction::Left);
            }
        }
        Action::FocusRight => {
            if let Some(tab) = ctx.layout.active_tab_mut() {
                tab.focus_direction(crate::layout::Direction::Right);
            }
        }
        Action::ScrollMode => {
            out.new_overlay = Some(Overlay::Scroll);
        }
        Action::CommandPalette => {
            out.new_overlay = Some(Overlay::Command {
                input: String::new(),
            });
        }
        Action::ShowDecisions => {
            let items = crate::decisions::list_all(ctx.home);
            out.new_overlay = Some(Overlay::Decisions { items, scroll: 0 });
        }
        Action::ShowTasks => {
            let items = crate::tasks::list_all(ctx.home);
            out.new_overlay = Some(Overlay::Tasks { items, scroll: 0 });
        }
        Action::ShowHelp => {
            out.new_overlay = Some(Overlay::Help);
        }
        Action::Detach => {
            out.should_break = true;
        }
        Action::ToggleZoom => {
            if let Some(tab) = ctx.layout.active_tab_mut() {
                tab.zoomed = !tab.zoomed;
            }
            out.needs_resize = true;
        }
        Action::NextLayout => {
            if let Some(tab) = ctx.layout.active_tab_mut() {
                tab.next_layout();
            }
            out.needs_resize = true;
        }
        Action::ResizeUp | Action::ResizeDown | Action::ResizeLeft | Action::ResizeRight => {
            let dir = match action {
                Action::ResizeUp => crate::layout::Direction::Up,
                Action::ResizeDown => crate::layout::Direction::Down,
                Action::ResizeLeft => crate::layout::Direction::Left,
                _ => crate::layout::Direction::Right,
            };
            if let Some(tab) = ctx.layout.active_tab_mut() {
                let focus = tab.focus_id;
                // Pane tree occupies terminal height minus the tab bar row
                // and status bar row (see render::render_app chrome layout).
                let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
                let area = (0, 1, cols, rows.saturating_sub(2));
                crate::layout::resize_focused(tab.root_mut(), area, focus, dir, 0.05);
            }
            out.needs_resize = true;
        }
        Action::None => {}
    }
    out
}
