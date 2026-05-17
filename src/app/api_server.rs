//! API-server guard + fleet auto-start hook.
//!
//! #879 — pre-fix this module hosted the in-process API server that the
//! TUI ran when it was the daemon (`BootstrapOutcome::Owned` path).
//! That path is gone: the app always attaches to a separately-spawned
//! daemon via [`super::ensure_attached`], so the in-process server +
//! its associated bridge (`TuiNotifier`) are no longer needed here.
//!
//! What remains: [`ApiGuard`] — a degenerate RAII no-op used by the
//! event loop's existing `_api_guard` binding. The cleanup it used to
//! do (`remove_dir_all` on the owned run_dir) is no longer applicable
//! in attached mode; the daemon process owns its own run dir lifecycle.
//! Kept (rather than deleted) so the call-site signature is stable
//! across the #879 transition — a future PR can prune it once the
//! call-site is touched for other reasons.

use crate::agent::AgentRegistry;
use crate::layout::{Layout, Tab};

use std::collections::HashMap;
use std::path::Path;

pub(super) struct ApiGuard;

/// Construct an ApiGuard that does nothing — used in attached mode,
/// where the TUI must not touch the daemon process's run dir.
pub(super) fn noop_guard() -> ApiGuard {
    ApiGuard
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
    wakeup_tx: &crossbeam_channel::Sender<usize>,
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
                crate::backend::SpawnMode::Resume,
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
