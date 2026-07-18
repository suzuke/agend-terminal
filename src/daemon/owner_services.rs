//! #2453 Stage 1a: a host-agnostic TWO-PHASE seam that owns the WIRING (not the
//! JoinHandles) of the owner-only background services started identically by
//! BOTH run hosts — the TUI owned-mode `app::run_app` and the headless
//! `daemon::run_core` (via `build_tick_infrastructure`). Centralizing the spawn
//! calls closes the dual-host drift class (#982/#1002/#1720/#2434: "wired in one
//! host, silently dead in the other"). Behavior-preserving: same calls, same
//! order, same args, same cfg — no new threads/locks/state/channels.
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
//! - `OwnerServicesStarted` is retained by each owner host (app: held by the
//!   owned-only `OwnedMaintenanceCycle`; daemon: held in `TickKeepalive`), so a host
//!   that skips the seam fails to compile (structural I2 — dual-host reach).

use crate::agent::AgentRegistry;
use std::path::Path;
use std::sync::Arc;

/// Which host role is composing the owner services. Owned → start them;
/// Attached (a TUI connected to another daemon that owns the fleet) → start none.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OwnerRole {
    Owned,
    /// Production gates attached-exclusion via the unchanged `if !attached_mode`
    /// wrapper in `run_app` (the seam is called with `Owned` INSIDE it, preserving
    /// #2737's exact order), so this variant is constructed only by tests today —
    /// it encodes the "attached starts none" contract that
    /// `attached_two_phases_start_none` verifies behaviorally against the real seam.
    #[cfg_attr(not(test), allow(dead_code))]
    Attached,
}

/// Capability token gating the five owner-service spawns. The single field is
/// PRIVATE to this module, so only `owner_services` code can mint one: other
/// modules may name `&OwnerServicePermit` in a signature but cannot construct it,
/// which makes a host-body direct spawn fail to compile (structural I4 — the
/// former `owner_services_spawns_absent_from_hosts…` masker guard, now a compile
/// property).
pub(crate) struct OwnerServicePermit(());

/// Phase-1 witness — proof that the monitoring phase ran. Required as an argument
/// by phase 2, so the compiler enforces monitoring-before-stream ordering.
/// Ctor is private to this module.
#[must_use]
pub(crate) struct OwnerMonitoringStarted(());

/// Final witness — proof that BOTH owner-service phases ran. Retained by each
/// owner host (app: held by the owned-only `OwnedMaintenanceCycle`; daemon: held
/// in `TickKeepalive`), so a host that skips the seam fails to compile
/// (structural I2 — the former `owner_services_called_by_both_hosts` guard).
/// Ctor is private to this module.
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

impl OwnerMonitoringStarters<'static> {
    /// Production starters — forward to the real thread-spawning services.
    /// `&fn_item` promotes to `&'static dyn Fn` (fn items are zero-sized consts).
    pub(crate) fn real() -> Self {
        OwnerMonitoringStarters {
            monitor_tick: &real_monitor_tick,
            api_activity_probe: &real_api_activity_probe,
        }
    }
}

impl OwnerStreamStarters<'static> {
    /// Production starters — forward to the real thread-spawning observers.
    pub(crate) fn real() -> Self {
        OwnerStreamStarters {
            rollout: &real_rollout,
            opencode: &real_opencode,
            kiro: &real_kiro,
        }
    }
}

fn real_monitor_tick(permit: &OwnerServicePermit, home: &Path, registry: &AgentRegistry) {
    crate::instance_monitor::spawn_monitor_tick(permit, home.to_path_buf(), Arc::clone(registry));
}

/// #2413 Phase 1: out-of-path lsof API-activity probe (feeds AgentCore::api_activity
/// for false-idle detection). Self-disables if `lsof` is absent.
fn real_api_activity_probe(permit: &OwnerServicePermit, registry: &AgentRegistry) {
    crate::api_activity_probe::spawn(permit, Arc::clone(registry));
}

/// #2413 Phase D: codex rollout-tail — read-only tail of
/// ~/.codex/sessions/.../rollout-*.jsonl → Evidence → shared buffer.
fn real_rollout(permit: &OwnerServicePermit, home: &Path, registry: &AgentRegistry) {
    crate::daemon::shadow::rollout::spawn(permit, Arc::clone(registry), home.to_path_buf());
}

/// #2413 opencode plane: SSE `/event` observer (per-agent embedded server).
fn real_opencode(permit: &OwnerServicePermit, home: &Path, registry: &AgentRegistry) {
    crate::daemon::shadow::opencode::spawn(permit, Arc::clone(registry), home.to_path_buf());
}

/// #2413 kiro plane: read-only tail of ~/.kiro/sessions/cli/<uuid>.jsonl.
fn real_kiro(permit: &OwnerServicePermit, home: &Path, registry: &AgentRegistry) {
    crate::daemon::shadow::kiro::spawn(permit, Arc::clone(registry), home.to_path_buf());
}

/// Phase 1: owner monitoring services (`instance_monitor` + `api_activity_probe`).
/// `Owned` starts them via the injected starters; `Attached` starts none. Returns
/// the `OwnerMonitoringStarted` token phase 2 requires. No-op / no threads under
/// `Attached`.
pub(crate) fn start_owner_monitoring(
    role: OwnerRole,
    home: &Path,
    registry: &AgentRegistry,
    starters: &OwnerMonitoringStarters<'_>,
) -> OwnerMonitoringStarted {
    if role == OwnerRole::Owned {
        let permit = OwnerServicePermit(());
        (starters.monitor_tick)(&permit, home, registry);
        (starters.api_activity_probe)(&permit, registry);
    }
    OwnerMonitoringStarted(())
}

/// Phase 2: owner Shadow-Observer stream planes (rollout + opencode + kiro).
/// Requires `&OwnerMonitoringStarted` so it cannot be called before phase 1
/// (compile-enforced ordering). `Owned` starts them; `Attached` starts none.
/// Returns the final `OwnerServicesStarted` witness. Each observer is itself a
/// no-op under `AGEND_SHADOW_OBSERVER=0` (default-ON). `shadow::start` (the
/// socket-ingest plane) is deliberately NOT here — its per-host ordering differs
/// (separate fork), so it stays host-local between the two phases in `run_app`.
pub(crate) fn start_owner_stream_observers(
    role: OwnerRole,
    _monitoring: &OwnerMonitoringStarted,
    home: &Path,
    registry: &AgentRegistry,
    starters: &OwnerStreamStarters<'_>,
) -> OwnerServicesStarted {
    if role == OwnerRole::Owned {
        let permit = OwnerServicePermit(());
        (starters.rollout)(&permit, home, registry);
        (starters.opencode)(&permit, home, registry);
        (starters.kiro)(&permit, home, registry);
    }
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
