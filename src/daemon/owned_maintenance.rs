//! Typed ownership for one host's owned maintenance cycle.

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
        std::fs::create_dir_all(&home).unwrap();
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));

        cycle.run_once(&home, &registry, &externals, &configs);

        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(cycle.handler_names(), ["recording"]);
        assert_eq!(cycle.phase_for_test(), Some(crate::daemon::tick_stall::Phase::PostHandlers));
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
