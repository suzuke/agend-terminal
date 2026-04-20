//! In-process API server lifecycle + fleet auto-start on cold boot.
//!
//! `ApiGuard` is an RAII handle that cleans up the run-dir when the TUI exits
//! and holds the [`crate::bootstrap::OwnedFleet`] — i.e. the `.daemon.lock`
//! flock guard, `api.cookie` bytes, and Telegram polling state — for the full
//! TUI lifetime. `auto_start_fleet` spawns every fleet.yaml instance as a new
//! tab during first-time startup (no session.json present).

use crate::agent::AgentRegistry;
use crate::layout::{Layout, Tab};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::TuiEventSender;

pub(super) struct ApiGuard {
    run_dir: Option<PathBuf>,
    // Held for lifetime: keeps .daemon.lock flock, api.cookie, telegram
    // polling state alive until the TUI exits.
    _owned: Option<Box<crate::bootstrap::OwnedFleet>>,
}

impl Drop for ApiGuard {
    fn drop(&mut self) {
        if let Some(ref dir) = self.run_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Spawn the in-process API server using a fleet prepared by
/// [`crate::bootstrap::prepare`]. The prepared fleet owns the run dir + cookie
/// (issued by bootstrap, fixing the `api.cookie missing; aborting serve`
/// regression that previously broke Telegram delivery in app mode) and is
/// held inside the guard for the TUI's lifetime.
pub(super) fn start_api_server(
    prepared: Box<crate::bootstrap::OwnedFleet>,
    registry: &AgentRegistry,
    tui_tx: TuiEventSender,
) -> ApiGuard {
    let run_dir = prepared.run_dir.clone();
    let api_home = prepared.home.clone();

    let api_registry = Arc::clone(registry);
    let configs: crate::api::ConfigRegistry = Arc::new(Mutex::new(HashMap::new()));
    let externals: crate::agent::ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

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

    tracing::info!(path = %run_dir.display(), "in-process API server started");
    ApiGuard {
        run_dir: Some(run_dir),
        _owned: Some(prepared),
    }
}

/// Construct an ApiGuard that does nothing — used when the current process
/// attached to an existing daemon and therefore must not touch its run dir.
pub(super) fn noop_guard() -> ApiGuard {
    ApiGuard {
        run_dir: None,
        _owned: None,
    }
}

/// Auto-start all fleet instances as tabs. Returns true if any were spawned.
#[allow(clippy::too_many_arguments)]
pub(super) fn auto_start_fleet(
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
            match super::pane_factory::create_pane_from_resolved(
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
