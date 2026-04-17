//! In-process API server lifecycle + fleet auto-start on cold boot.
//!
//! `ApiGuard` is an RAII handle that cleans up the run-dir when the TUI exits.
//! `start_api_server` skips spinning up the server if another daemon already owns
//! the run-dir. `auto_start_fleet` spawns every fleet.yaml instance as a new tab
//! during first-time startup (no session.json present).

use crate::agent::AgentRegistry;
use crate::layout::{Layout, Tab};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::TuiEventSender;

pub(super) struct ApiGuard {
    run_dir: Option<PathBuf>,
}

impl Drop for ApiGuard {
    fn drop(&mut self) {
        if let Some(ref dir) = self.run_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

pub(super) fn start_api_server(
    home: &Path,
    registry: &AgentRegistry,
    tui_tx: TuiEventSender,
) -> ApiGuard {
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
