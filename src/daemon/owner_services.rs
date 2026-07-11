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
//! The structural wiring guards for these helpers live in `app::tests`
//! (`owner_services_*`), where the shared comment/string masker already exists —
//! no masker is duplicated here and this module stays production-only.

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
