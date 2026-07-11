//! #2453: `UiState` — the mutable key/UI-interaction state owned by `run_app`,
//! extracted from five loose locals, with `handle_key_event` as its first
//! behavior boundary. Keeps `run_app` (and `src/app/mod.rs`) smaller instead of
//! growing the monolith.
use super::dispatch::{self, DispatchCtx};
use super::overlay::{self, Overlay, OverlayCtx};
use crate::agent::AgentRegistry;
use crate::keybinds::KeyHandler;
use crate::layout::Layout;
use crossterm::event::KeyEvent;
use std::collections::HashMap;
use std::path::Path;

/// The five cohesive key/UI-interaction fields, owned by one type.
pub(super) struct UiState {
    pub(super) layout: Layout,
    pub(super) last_tab: usize,
    pub(super) name_counter: HashMap<String, usize>,
    pub(super) overlay: Overlay,
    pub(super) key_handler: KeyHandler,
}

/// Loop-stable shared (immutable) deps the key handlers borrow; built once.
pub(super) struct KeyDeps<'a> {
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
    pub(super) fn handle_key_event(&mut self, key: KeyEvent, deps: &KeyDeps<'_>) -> KeyOutcome {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    /// #2453 characterization: `handle_key_event` routes to the dispatch path
    /// (applying `new_overlay`) and to the active overlay (Esc closes it) — a
    /// deterministic transition, with no live daemon.
    #[test]
    fn handle_key_event_routes_dispatch_and_overlay() {
        let registry: AgentRegistry = std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = std::env::temp_dir().join(format!("agend-uistate-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let fleet_path = home.join("fleet.yaml");
        let (wakeup_tx, _rx) = crossbeam_channel::unbounded::<usize>();
        let deps = KeyDeps {
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
        };

        // Dispatch path: Ctrl+B then 'c' → Action::NewTab → new_overlay applied.
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

        // Active-overlay path: Esc is routed to overlay::handle_key → closes the menu.
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        ui.handle_key_event(esc, &deps);
        assert!(
            matches!(ui.overlay, Overlay::None),
            "Esc must close the overlay (routed to overlay::handle_key, not dispatch)"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
