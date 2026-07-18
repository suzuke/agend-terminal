//! Typed ownership for one host's owned maintenance cycle.

use crate::agent::{AgentRegistry, ExternalRegistry};
use crate::api::ConfigRegistry;
use crate::daemon::owner_services::{OwnerRole, OwnerServicesStarted};
use crate::daemon::per_tick::{run_handlers_with_progress, PerTickHandler, TickContext};
use crate::daemon::tick_stall::{TickProgress, TickStallMonitorGuard};
use std::path::Path;
use std::sync::Arc;

/// Owned-mode maintenance state shared by the daemon and TUI hosts.
///
/// This type owns only the per-host handler profile, the owner-service witness,
/// and optional progress/stall-monitor state. The cadence producer, receiver,
/// select topology, bootstrap, shutdown, and shadow planes remain in each host.
pub(crate) struct OwnedMaintenanceCycle {
    handlers: Vec<Box<dyn PerTickHandler>>,
    _owner_services: OwnerServicesStarted,
    progress: Option<Arc<TickProgress>>,
    _stall_monitor: Option<TickStallMonitorGuard>,
}

impl OwnedMaintenanceCycle {
    /// Construct an owned cycle and centralize progress/stall-monitor setup.
    pub(crate) fn new(
        handlers: Vec<Box<dyn PerTickHandler>>,
        owner_services: OwnerServicesStarted,
        host: &'static str,
        home: &Path,
    ) -> Self {
        let (progress, stall_monitor) =
            crate::daemon::tick_stall::start_for_host(host, &handlers, home);
        Self::from_parts(handlers, owner_services, progress, stall_monitor)
    }

    /// Test/host seam for supplying already-created progress and monitor state.
    pub(crate) fn from_parts(
        handlers: Vec<Box<dyn PerTickHandler>>,
        owner_services: OwnerServicesStarted,
        progress: Option<Arc<TickProgress>>,
        stall_monitor: Option<TickStallMonitorGuard>,
    ) -> Self {
        Self {
            handlers,
            _owner_services: owner_services,
            progress,
            _stall_monitor: stall_monitor,
        }
    }

    /// Construct only for an owned host. Attached hosts return `None` without
    /// consuming a witness or creating any cycle state.
    pub(crate) fn new_for_role(
        role: OwnerRole,
        handlers: Vec<Box<dyn PerTickHandler>>,
        owner_services: Option<OwnerServicesStarted>,
        host: &'static str,
        home: &Path,
    ) -> Option<Self> {
        if role == OwnerRole::Attached {
            return None;
        }
        Some(Self::new(
            handlers,
            owner_services.expect("owned maintenance requires owner-service witness"),
            host,
            home,
        ))
    }

    /// Publish the between-tick boundary. Hosts call this immediately before
    /// waiting in their own select loop.
    pub(crate) fn enter_waiting(&self) {
        if let Some(progress) = &self.progress {
            progress.enter_waiting();
        }
    }

    /// Run one maintenance cycle. The cycle closes in `PostHandlers`; callers
    /// retain host-specific control of any crash dispatch before entering
    /// `Waiting` for the next select.
    pub(crate) fn run_once(
        &self,
        home: &Path,
        registry: &AgentRegistry,
        externals: &ExternalRegistry,
        configs: &ConfigRegistry,
    ) {
        if let Some(progress) = &self.progress {
            progress.enter_preflight();
        }
        crate::runtime_controls::reload_runtime_controls(home);
        let tick_ctx = TickContext {
            home,
            registry,
            externals,
            configs,
        };
        run_handlers_with_progress(&self.handlers, &tick_ctx, self.progress.as_deref());
    }

    #[cfg(test)]
    fn handler_names(&self) -> Vec<&'static str> {
        self.handlers.iter().map(|handler| handler.name()).collect()
    }

    #[cfg(test)]
    fn phase_for_test(&self) -> Option<crate::daemon::tick_stall::Phase> {
        self.progress
            .as_deref()
            .and_then(crate::daemon::tick_stall::TickProgress::phase_for_test)
    }
}

#[cfg(test)]
mod tests {
    use super::OwnedMaintenanceCycle;
    use crate::agent::{AgentRegistry, ExternalRegistry};
    use crate::daemon::owner_services::{
        start_owner_monitoring, start_owner_stream_observers, OwnerMonitoringStarters, OwnerRole,
        OwnerStreamStarters,
    };
    use crate::daemon::per_tick::{PerTickHandler, TickContext};
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct RecordingHandler {
        runs: Arc<AtomicUsize>,
    }

    impl PerTickHandler for RecordingHandler {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn run(&self, _ctx: &TickContext<'_>) {
            self.runs.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn owner_services_started() -> crate::daemon::owner_services::OwnerServicesStarted {
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let monitoring = OwnerMonitoringStarters {
            monitor_tick: &|_, _, _| {},
            api_activity_probe: &|_, _| {},
        };
        let streams = OwnerStreamStarters {
            rollout: &|_, _, _| {},
            opencode: &|_, _, _| {},
            kiro: &|_, _, _| {},
        };
        let phase_one = start_owner_monitoring(
            OwnerRole::Owned,
            Path::new("/tmp/owned-maintenance-cycle-red"),
            &registry,
            &monitoring,
        );
        start_owner_stream_observers(
            OwnerRole::Owned,
            &phase_one,
            Path::new("/tmp/owned-maintenance-cycle-red"),
            &registry,
            &streams,
        )
    }

    #[test]
    fn run_once_owns_profile_and_finishes_in_post_handlers() {
        let runs = Arc::new(AtomicUsize::new(0));
        let progress = crate::daemon::tick_stall::TickProgress::new(
            "owned-maintenance-red",
            Arc::from(vec!["recording"]),
        );
        let cycle = OwnedMaintenanceCycle::from_parts(
            vec![Box::new(RecordingHandler {
                runs: Arc::clone(&runs),
            })],
            owner_services_started(),
            Some(Arc::clone(&progress)),
            None,
        );
        let home = std::env::temp_dir().join(format!(
            "agend-owned-maintenance-red-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).expect("create owned-maintenance test home");
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));

        cycle.run_once(&home, &registry, &externals, &configs);

        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(cycle.handler_names(), ["recording"]);
        assert_eq!(
            cycle.phase_for_test(),
            Some(crate::daemon::tick_stall::Phase::PostHandlers)
        );
    }

    #[test]
    fn attached_role_constructs_no_cycle() {
        assert!(OwnedMaintenanceCycle::new_for_role(
            OwnerRole::Attached,
            Vec::new(),
            None,
            "attached-maintenance-red",
            Path::new("/tmp/owned-maintenance-cycle-red"),
        )
        .is_none());
    }
}
