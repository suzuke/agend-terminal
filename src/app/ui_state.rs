//! #2453: `UiState` — the mutable key/UI-interaction state owned by `run_app`,
//! extracted from loose locals, with `handle_key_event` / `handle_mouse_event`
//! as its behavior boundaries. Keeps `run_app` (and `src/app/mod.rs`) smaller
//! instead of growing the monolith.
use super::dispatch::{self, DispatchCtx};
use super::mouse;
use super::overlay::{self, Overlay, OverlayCtx};
use crate::agent::AgentRegistry;
use crate::keybinds::KeyHandler;
use crate::layout::Layout;
use crossterm::event::{KeyEvent, MouseEvent};
use std::collections::HashMap;
use std::path::Path;

/// The cohesive key/UI-interaction fields, owned by one type.
pub(super) struct UiState {
    pub(super) layout: Layout,
    pub(super) last_tab: usize,
    pub(super) name_counter: HashMap<String, usize>,
    pub(super) overlay: Overlay,
    pub(super) key_handler: KeyHandler,
    pub(super) mouse_state: mouse::MouseState,
}

/// Loop-stable shared (immutable) deps the key/mouse handlers borrow; built once.
pub(super) struct UiDeps<'a> {
    pub(super) registry: &'a AgentRegistry,
    pub(super) home: &'a Path,
    pub(super) fleet_path: &'a Path,
    pub(super) wakeup_tx: &'a crossbeam_channel::Sender<usize>,
}

/// Signals `handle_key_event` returns to the event loop (applied independently).
#[derive(Default)]
pub(super) struct KeyOutcome {
    pub(super) needs_resize: bool,
    pub(super) should_break: bool,
}

impl UiState {
    /// Route one key to the active overlay, else through keybinding → dispatch.
    /// Field-splits `self` so the existing free `OverlayCtx`/`DispatchCtx`
    /// handlers borrow layout/name_counter/last_tab disjointly from `overlay`
    /// (no `&mut self` re-borrow, no RefCell/mem::take). Mirrors the former
    /// inline `Event::Key` branch (overlay path returns early == `continue`).
    pub(super) fn handle_key_event(&mut self, key: KeyEvent, deps: &UiDeps<'_>) -> KeyOutcome {
        let mut out = KeyOutcome::default();
        if !matches!(self.overlay, Overlay::None) {
            let mut octx = OverlayCtx {
                layout: &mut self.layout,
                registry: deps.registry,
                home: deps.home,
                fleet_path: deps.fleet_path,
                wakeup_tx: deps.wakeup_tx,
                name_counter: &mut self.name_counter,
            };
            let outcome = overlay::handle_key(&mut self.overlay, key, &mut octx);
            out.needs_resize = outcome.needs_resize;
            return out;
        }

        let action = self.key_handler.handle(key);
        let mut dctx = DispatchCtx {
            layout: &mut self.layout,
            registry: deps.registry,
            home: deps.home,
            fleet_path: deps.fleet_path,
            last_tab: &mut self.last_tab,
            wakeup_tx: deps.wakeup_tx,
            name_counter: &mut self.name_counter,
        };
        let res = dispatch::dispatch(action, &mut dctx);
        out.needs_resize = res.needs_resize;
        out.should_break = res.should_break;
        if let Some(ov) = res.new_overlay {
            self.overlay = ov;
        }
        out
    }

    /// Route one mouse event. While an overlay is modal the event is swallowed —
    /// mouse must not reach hidden panes (this is `mouse::handle`'s own caller
    /// contract), leaving `mouse_state`/`layout` untouched. Otherwise field-split
    /// `self` so `mouse::handle` borrows `layout`/`mouse_state` disjointly, then
    /// apply its tab/overlay outputs to `self`. Returns `needs_resize` — the only
    /// signal the loop still applies. Mirrors `handle_key_event`.
    pub(super) fn handle_mouse_event(&mut self, mouse_evt: MouseEvent, deps: &UiDeps<'_>) -> bool {
        if !matches!(self.overlay, Overlay::None) {
            return false; // modal swallow: leave mouse_state/layout untouched
        }
        let out = mouse::handle(
            mouse_evt,
            &mut self.layout,
            &mut self.mouse_state,
            deps.fleet_path,
            deps.registry,
        );
        let needs_resize = out.needs_resize;
        if let Some(prev) = out.new_last_tab {
            self.last_tab = prev;
        }
        if let Some(ov) = out.new_overlay {
            self.overlay = ov;
        }
        needs_resize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use crossterm::event::{MouseButton, MouseEventKind};

    /// #2453 characterization: one `UiState` routes key events (dispatch path
    /// applies `new_overlay`; the active overlay consumes Esc) AND mouse events
    /// (non-modal routes through `mouse::handle`; a modal overlay swallows) — a
    /// deterministic sequence over a single `ui`/`deps`, with no live daemon.
    #[test]
    fn ui_state_routes_key_and_mouse_events() {
        let registry: AgentRegistry = std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = std::env::temp_dir().join(format!("agend-uistate-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let fleet_path = home.join("fleet.yaml");
        let (wakeup_tx, _rx) = crossbeam_channel::unbounded::<usize>();
        let deps = UiDeps {
            registry: &registry,
            home: &home,
            fleet_path: &fleet_path,
            wakeup_tx: &wakeup_tx,
        };
        let mut ui = UiState {
            layout: Layout::new(),
            last_tab: 0,
            name_counter: HashMap::new(),
            overlay: Overlay::None,
            key_handler: KeyHandler::new(),
            mouse_state: mouse::MouseState::default(),
        };

        // Key, dispatch path: Ctrl+B then 'c' → Action::NewTab → new_overlay applied.
        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        ui.handle_key_event(ctrl_b, &deps);
        assert!(
            matches!(ui.overlay, Overlay::None),
            "prefix must not open overlay"
        );
        let c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty());
        let out = ui.handle_key_event(c, &deps);
        assert!(!out.should_break, "NewTab must not break the loop");
        assert!(
            matches!(ui.overlay, Overlay::NewTabMenu { .. }),
            "dispatch path must apply new_overlay (NewTab opens the menu)"
        );

        // Key, active-overlay path: Esc routes to overlay::handle_key → closes it.
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        ui.handle_key_event(esc, &deps);
        assert!(
            matches!(ui.overlay, Overlay::None),
            "Esc must close the overlay (routed to overlay::handle_key, not dispatch)"
        );

        // Mouse, non-modal (overlay is None here): a seeded border-drag + MouseUp
        // routes through mouse::handle → clears the drag and requests a resize.
        seed_border_drag(&mut ui);
        let needs_resize = ui.handle_mouse_event(mouse_up(), &deps);
        assert!(
            needs_resize,
            "non-modal border-drag mouse-up must request a resize"
        );
        assert!(
            ui.mouse_state.border_drag.is_none(),
            "mouse-up must clear the border-drag (routed to mouse::handle)"
        );

        // Mouse, modal: with an overlay open the event is swallowed — mouse::handle
        // is never called, so the re-seeded drag and the overlay are preserved and
        // no resize is requested (mouse must not reach hidden panes).
        seed_border_drag(&mut ui);
        ui.overlay = Overlay::Help;
        let needs_resize = ui.handle_mouse_event(mouse_up(), &deps);
        assert!(
            !needs_resize,
            "modal overlay must swallow the mouse event (no resize)"
        );
        assert!(
            ui.mouse_state.border_drag.is_some(),
            "modal swallow must leave the border-drag untouched"
        );
        assert!(
            matches!(ui.overlay, Overlay::Help),
            "modal swallow must not change the overlay"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// A left mouse-up carrying no coordinate meaning — the outcome is decided
    /// by the seeded `border_drag`, not by where the pointer is.
    fn mouse_up() -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        }
    }

    /// Seed a mid-gesture split-border drag so the next MouseUp deterministically
    /// completes the resize in `mouse::handle_up` (no screen coordinates).
    fn seed_border_drag(ui: &mut UiState) {
        ui.mouse_state.border_drag = Some((
            crate::layout::SplitBorderHit {
                split_area: (0, 1, 60, 38),
                dir: crate::layout::SplitDir::Vertical,
            },
            ratatui::layout::Rect::new(0, 1, 120, 38),
        ));
    }
}
