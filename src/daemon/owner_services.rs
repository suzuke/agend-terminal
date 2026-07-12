//! #2453 Stage 1a: two host-agnostic helpers that own the WIRING (not the
//! JoinHandles) of the owner-only background services started identically by
//! BOTH run hosts — the TUI owned-mode `app::run_app` and the headless
//! `daemon::run_core` (via `build_tick_infrastructure`). Extracting the shared
//! spawn calls into one place closes the dual-host drift class (#982/#1002/
//! #1720/#2434: "wired in one host, silently dead in the other"). Pure,
//! behavior-preserving: same calls, same order, same args, same cfg — no new
//! threads/locks/state/channels.
//!
//! Deliberately EXCLUDED (each a separate decision fork, per d-20260711201257672833-2):
//! `shadow::start` (different ordering per host), `supervisor::spawn` (`#[cfg(unix)]`
//! delta), `router`/`TaskSweep`/recovery (headless-only), bootstrap/restart/shutdown.
//!
//! #2737 (decision d-20260712034336318886-6): the old app::tests string masker +
//! source-scan guards are replaced by the TYPED two-phase seam below. Both hosts
//! reach the owner services by construction/real call-level tests instead of by
//! grepping source text:
//! - `start_owner_monitoring` → `OwnerMonitoringStarted`, then (in app) `shadow::start`,
//!   then `start_owner_stream_observers(…, &OwnerMonitoringStarted, …)` → `OwnerServicesStarted`.
//!   The phase-2 `&OwnerMonitoringStarted` parameter makes "monitoring runs before stream
//!   observers" a COMPILE dependency (you cannot call phase 2 without phase 1's token).
//! - the five low-level spawns require a private `OwnerServicePermit`, so a host-body
//!   direct spawn cannot compile (structural I4 — no host-local copy).
//! - `OwnerServicesStarted` is retained by each owner host (app: required by the
//!   owned-only `app_maintenance_tick`; daemon: held in `TickKeepalive`), so a host
//!   that skips the seam fails to compile (structural I2 — dual-host reach).

use crate::agent::AgentRegistry;
use std::path::Path;
use std::sync::Arc;

/// Owner-only agent liveness/activity monitoring services. Each spawns a
/// process-lifetime thread internally and returns `()` — this helper owns the
/// WIRING, not a `JoinHandle`. Called identically by owned `run_app` and headless
/// `build_tick_infrastructure`.
pub(crate) fn start_shared_monitoring_services(home: &Path, registry: &AgentRegistry) {
    crate::instance_monitor::spawn_monitor_tick(home.to_path_buf(), Arc::clone(registry));
    // #2413 Phase 1: out-of-path lsof API-activity probe (feeds
    // AgentCore::api_activity for false-idle detection). Self-disables if `lsof`
    // is absent.
    crate::api_activity_probe::spawn(Arc::clone(registry));
}

/// Owner-only Shadow Observer per-backend Evidence-SOURCE planes (Stream plane).
/// Each is a no-op under `AGEND_SHADOW_OBSERVER=0` (default-ON). The live fleet
/// daemon is app mode, so gating these run_core-only would leave each backend's
/// observer source dead in production (#2434). `shadow::start` (the socket-ingest
/// plane) is deliberately NOT here — its per-host ordering differs (separate fork).
pub(crate) fn start_shared_stream_observers(home: &Path, registry: &AgentRegistry) {
    // #2413 Phase D: codex rollout-tail — read-only tail of
    // ~/.codex/sessions/.../rollout-*.jsonl → Evidence → shared buffer.
    crate::daemon::shadow::rollout::spawn(Arc::clone(registry), home.to_path_buf());
    // #2413 opencode plane: SSE `/event` observer (per-agent embedded server).
    crate::daemon::shadow::opencode::spawn(Arc::clone(registry), home.to_path_buf());
    // #2413 kiro plane: read-only tail of ~/.kiro/sessions/cli/<uuid>.jsonl.
    crate::daemon::shadow::kiro::spawn(Arc::clone(registry), home.to_path_buf());
}

// ===================================================================
// #2737 typed two-phase owner-service seam (RED scaffolding).
// Commit A: declarations + stub phase bodies that start NOTHING, so the
// call-level tests below are RED. Commit B fills in the real bodies, adds the
// permit to the five low-level spawns, wires both hosts, and deletes the old
// string-masker guards. The `#[allow(dead_code)]` marks below are removed in
// commit B once production wires these items.
// ===================================================================

/// Which host role is composing the owner services. Owned → start them;
/// Attached (a TUI connected to another daemon that owns the fleet) → start none.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OwnerRole {
    Owned,
    Attached,
}

/// Capability token gating the five owner-service spawns. The single field is
/// PRIVATE to this module, so only `owner_services` code can mint one: other
/// modules may name `&OwnerServicePermit` in a signature but cannot construct it,
/// which makes a host-body direct spawn fail to compile (structural I4).
#[allow(dead_code)]
pub(crate) struct OwnerServicePermit(());

/// Phase-1 witness — proof that the monitoring phase ran. Required as an argument
/// by phase 2, so the compiler enforces monitoring-before-stream ordering.
/// Ctor is private to this module.
#[allow(dead_code)]
#[must_use]
pub(crate) struct OwnerMonitoringStarted(());

/// Final witness — proof that BOTH owner-service phases ran. Retained by each
/// owner host (structural I2). Ctor is private to this module.
#[allow(dead_code)]
#[must_use]
pub(crate) struct OwnerServicesStarted(());

/// Injected phase-1 starters. Two REQUIRED fields → the struct literal (incl. the
/// production `real()` ctor) fails to compile until every monitoring service is
/// wired (I1 completeness for phase 1). Production uses `real()`; tests inject
/// recording fakes that start no threads.
pub(crate) struct OwnerMonitoringStarters<'a> {
    pub monitor_tick: &'a dyn Fn(&OwnerServicePermit, &Path, &AgentRegistry),
    pub api_activity_probe: &'a dyn Fn(&OwnerServicePermit, &AgentRegistry),
}

/// Injected phase-2 starters. Three REQUIRED fields (I1 completeness for phase 2).
pub(crate) struct OwnerStreamStarters<'a> {
    pub rollout: &'a dyn Fn(&OwnerServicePermit, &Path, &AgentRegistry),
    pub opencode: &'a dyn Fn(&OwnerServicePermit, &Path, &AgentRegistry),
    pub kiro: &'a dyn Fn(&OwnerServicePermit, &Path, &AgentRegistry),
}

/// Phase 1: owner monitoring services. `Owned` starts them via the injected
/// starters; `Attached` starts none. Returns the `OwnerMonitoringStarted` token
/// phase 2 requires.
#[allow(dead_code)]
pub(crate) fn start_owner_monitoring(
    _role: OwnerRole,
    _home: &Path,
    _registry: &AgentRegistry,
    _starters: &OwnerMonitoringStarters<'_>,
) -> OwnerMonitoringStarted {
    // RED stub (commit A): starts nothing → the owned call-level test is RED.
    OwnerMonitoringStarted(())
}

/// Phase 2: owner stream observers. Requires `&OwnerMonitoringStarted` so it
/// cannot be called before phase 1 (compile-enforced ordering). `Owned` starts
/// them; `Attached` starts none. Returns the final `OwnerServicesStarted` witness.
#[allow(dead_code)]
pub(crate) fn start_owner_stream_observers(
    _role: OwnerRole,
    _monitoring: &OwnerMonitoringStarted,
    _home: &Path,
    _registry: &AgentRegistry,
    _starters: &OwnerStreamStarters<'_>,
) -> OwnerServicesStarted {
    // RED stub (commit A): starts nothing → the owned call-level test is RED.
    OwnerServicesStarted(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn empty_registry() -> AgentRegistry {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn tmp() -> &'static Path {
        Path::new("/tmp/owner-services-seam-test")
    }

    /// Drive both phases with recording fakes (no threads) under `role`, returning
    /// the ordered labels of the services that were started. The fake closures are
    /// locals here so they outlive the starter structs they are borrowed into.
    fn run_both_phases(role: OwnerRole) -> Vec<&'static str> {
        let log = RefCell::new(Vec::new());
        let reg = empty_registry();
        let monitoring = OwnerMonitoringStarters {
            monitor_tick: &|_p, _h, _r| log.borrow_mut().push("monitor_tick"),
            api_activity_probe: &|_p, _r| log.borrow_mut().push("api_activity_probe"),
        };
        let stream = OwnerStreamStarters {
            rollout: &|_p, _h, _r| log.borrow_mut().push("rollout"),
            opencode: &|_p, _h, _r| log.borrow_mut().push("opencode"),
            kiro: &|_p, _h, _r| log.borrow_mut().push("kiro"),
        };
        let mon = start_owner_monitoring(role, tmp(), &reg, &monitoring);
        let _services = start_owner_stream_observers(role, &mon, tmp(), &reg, &stream);
        let started = log.borrow().clone();
        started
    }

    /// I1 completeness + PHASE ORDER: owned mode starts exactly the five owner
    /// services, monitoring pair BEFORE stream trio, in this exact order.
    /// Reverse-mutation: omitting/reordering a starter, or a stub that starts
    /// nothing, turns this RED. (Swapping the two phase CALLS won't even compile —
    /// phase 2 requires the phase-1 token.)
    #[test]
    fn owned_two_phases_start_all_five_in_order() {
        assert_eq!(
            run_both_phases(OwnerRole::Owned),
            [
                "monitor_tick",
                "api_activity_probe",
                "rollout",
                "opencode",
                "kiro"
            ],
            "owned mode must start exactly the five owner services, monitoring before stream"
        );
    }

    /// I3 attached exclusion: an attached TUI passes through both phases but
    /// starts NONE. Reverse-mutation: dropping the `Owned` gate turns this RED.
    #[test]
    fn attached_two_phases_start_none() {
        assert!(
            run_both_phases(OwnerRole::Attached).is_empty(),
            "attached mode must start no owner services"
        );
    }
}
