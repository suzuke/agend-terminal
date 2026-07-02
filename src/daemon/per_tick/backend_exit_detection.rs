//! #2538: backend-exit detection — the daemon already knows (via
//! `AgentHandle::backend_command`, mutated by `agent::on_clean_exit_shell_fallback`
//! and any other exit-driven respawn) when an instance's LIVE foreground identity
//! no longer matches its fleet.yaml-DECLARED backend (e.g. a `codex`-configured
//! instance whose backend exited cleanly and the daemon deliberately spawned a
//! bare shell in its place — the pane looks alive, `health_state` stays
//! `healthy`, and nothing ever notices). This ground-truth signal was already in
//! hand and simply never consumed — same family as #2413's API-activity probe /
//! #1523 state-detection.
//!
//! Every tick: snapshot each agent's live `backend_command` (registry lock, no
//! file IO), resolve the fleet.yaml-declared backend ONCE for the whole tick (no
//! lock held), then re-lock briefly to apply the `HealthState::Unhealthy`
//! transition (or clear it once the mismatch resolves) and fire a debounced
//! `backend_exited` notify.

use super::{PerTickHandler, TickContext};
use crate::backend::Backend;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Grace window after an agent's most recent spawn during which a mismatch is
/// NOT reported — covers both the immediate post-spawn transient (backend
/// still initializing) and a brief dialog-dismiss-triggered respawn blip.
const BACKEND_EXIT_GRACE: Duration = Duration::from_secs(15);

/// Notify debounce — mirrors `daemon::supervisor::NOTIFY_COOLDOWN` (60s), the
/// existing per-agent supervisor-notify cooldown for other error-class states.
const NOTIFY_COOLDOWN: Duration = Duration::from_secs(60);

/// Pure: does the LIVE `backend_command` mismatch the fleet.yaml-DECLARED one?
/// `Backend::from_command(configured) == None` means the instance is configured
/// with a Shell or unrecognized Raw backend — there is no known preset identity
/// to compare against, so it is exempt. This is also how a `backend: shell`
/// instance is naturally exempt (its resolved command never maps to a preset).
pub(crate) fn backend_mismatch(_configured_backend_command: &str, _live_backend_command: &str) -> bool {
    // #2538 RED: comparison not yet implemented.
    false
}

/// Pure: should this tick fire a backend-exit detection for an agent whose live
/// backend mismatches its configured one, `elapsed_since_spawn` since its last
/// (re)spawn?
pub(crate) fn should_fire_backend_exit(
    _configured_backend_command: &str,
    _live_backend_command: &str,
    _elapsed_since_spawn: Duration,
) -> bool {
    // #2538 RED: comparison not yet implemented.
    false
}

pub(crate) struct BackendExitDetectionHandler {
    last_notified: Mutex<HashMap<String, Instant>>,
}

impl BackendExitDetectionHandler {
    pub(crate) fn new() -> Self {
        Self {
            last_notified: Mutex::new(HashMap::new()),
        }
    }
}

impl PerTickHandler for BackendExitDetectionHandler {
    fn name(&self) -> &'static str {
        "backend_exit_detection"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        // Phase 1 (registry lock, NO file IO — DAEMON-LOCK-ORDERING): snapshot
        // each agent's live backend_command + spawn instant.
        let snaps: Vec<(String, String, Instant)> = {
            let reg = crate::agent::lock_registry(ctx.registry);
            reg.values()
                .map(|h| (h.name.to_string(), h.backend_command.clone(), h.spawned_at))
                .collect()
        };
        if snaps.is_empty() {
            return;
        }

        // Phase 2 (no lock): resolve fleet.yaml ONCE for the whole tick.
        let Some(fleet) =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(ctx.home)).ok()
        else {
            return;
        };

        let now = Instant::now();
        let mut mismatched: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut to_notify: Vec<(String, String, String)> = Vec::new();

        for (name, live_backend, spawned_at) in &snaps {
            let Some(resolved) = fleet.resolve_instance(name) else {
                continue;
            };
            if backend_mismatch(&resolved.backend_command, live_backend) {
                mismatched.insert(name.clone());
            }
            if should_fire_backend_exit(&resolved.backend_command, live_backend, spawned_at.elapsed())
            {
                let mut tracker = self.last_notified.lock();
                let should_notify = tracker
                    .get(name)
                    .is_none_or(|t| now.duration_since(*t) >= NOTIFY_COOLDOWN);
                if should_notify {
                    tracker.insert(name.clone(), now);
                    to_notify.push((name.clone(), resolved.backend_command.clone(), live_backend.clone()));
                }
            } else {
                // Recovered (or never mismatched) — drop any stale debounce entry
                // so a LATER re-mismatch doesn't inherit an old cooldown.
                self.last_notified.lock().remove(name);
            }
        }

        // Phase 3 (registry lock): apply the HealthState transition. Skips
        // `Paused` — operator-owned terminal state, never auto-overridden
        // (mirrors `HealthTracker::maybe_decay_at`'s guard).
        {
            let reg = crate::agent::lock_registry(ctx.registry);
            for handle in reg.values() {
                let mut core = handle.core.lock();
                if core.health.state == crate::health::HealthState::Paused {
                    continue;
                }
                if mismatched.contains(handle.name.as_str()) {
                    core.health.state = crate::health::HealthState::Unhealthy;
                } else if core.health.state == crate::health::HealthState::Unhealthy {
                    core.health.state = crate::health::HealthState::Healthy;
                }
            }
        }

        for (name, expected, live) in to_notify {
            notify_backend_exited(ctx.home, &name, &expected, &live);
        }
    }
}

fn notify_backend_exited(home: &std::path::Path, name: &str, expected: &str, live: &str) {
    tracing::warn!(
        agent = %name, %expected, %live,
        "#2538: backend exited — foreground identity mismatch"
    );
    crate::event_log::log(
        home,
        "backend_exited",
        name,
        &format!("expected={expected} observed={live}"),
    );
    let msg = format!(
        "⚠️ {name}: backend process exited unexpectedly (expected `{expected}`, pane now shows \
         `{live}`). Dispatches to this agent will sit unread until it's respawned or replaced."
    );
    crate::channel::notify_all_escalation_channels(
        name,
        crate::channel::NotifySeverity::Error,
        &msg,
        false,
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::agent::{AgentRegistry, ExternalRegistry};
    use parking_lot::Mutex as PLMutex;
    use std::collections::HashMap as StdHashMap;
    use std::sync::Arc;

    // ── §2538 scenario 1: exit fires ──────────────────────────────────────
    #[test]
    fn backend_mismatch_fires_when_backend_exited_to_shell() {
        assert!(
            backend_mismatch("codex", "/bin/bash"),
            "a codex-configured instance whose live process is now a bare shell must mismatch"
        );
    }

    // ── §2538 scenario 2: normal (matching) does not fire ─────────────────
    #[test]
    fn backend_mismatch_false_when_live_matches_configured() {
        assert!(
            !backend_mismatch("codex", "codex"),
            "matching live/configured backend must not mismatch"
        );
        assert!(
            !backend_mismatch("codex", "/usr/local/bin/codex"),
            "a resolved absolute path for the same backend must still match by basename"
        );
    }

    // ── §2538 scenario 3: shell-configured backend is exempt ──────────────
    #[test]
    fn backend_mismatch_false_when_configured_is_shell() {
        assert!(
            !backend_mismatch("/bin/bash", "/bin/zsh"),
            "a shell-configured instance has no preset identity to compare — must never mismatch"
        );
        assert!(
            !backend_mismatch("/bin/bash", "codex"),
            "even a live codex under a shell-configured instance must not fire — the instance was \
             never declared to be running a managed backend"
        );
    }

    // ── §2538 scenario 4: transient spawn grace suppresses the fire ───────
    #[test]
    fn should_fire_backend_exit_false_within_grace_even_when_mismatched() {
        assert!(
            !should_fire_backend_exit("codex", "/bin/bash", Duration::from_secs(2)),
            "a mismatch within the post-spawn grace window must not fire (startup / dismiss-dialog \
             transient)"
        );
    }

    #[test]
    fn should_fire_backend_exit_true_past_grace_when_mismatched() {
        assert!(
            should_fire_backend_exit("codex", "/bin/bash", Duration::from_secs(60)),
            "a mismatch that persists past the grace window must fire"
        );
    }

    #[test]
    fn should_fire_backend_exit_false_when_no_mismatch_regardless_of_elapsed() {
        assert!(
            !should_fire_backend_exit("codex", "codex", Duration::from_secs(600)),
            "no mismatch → never fires, no matter how long the agent has been up"
        );
    }

    /// Smoke test mirroring `hang_detection::tests::run_is_noop_on_empty_registry`
    /// — the handler-level integration surface (constructing a live `AgentHandle`
    /// needs a real PTY/child process and can't be built in a unit test, per the
    /// same constraint `agent::mod.rs`'s `startup_failure_from` extraction notes).
    /// The real entry point for the DECISION logic is the pure functions above,
    /// exercised directly; this test only pins that `run()` doesn't panic when
    /// there is nothing to scan.
    #[test]
    fn run_is_noop_on_empty_registry() {
        let home = std::env::temp_dir().join(format!(
            "agend-backend-exit-handler-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).ok();
        let registry: AgentRegistry = Arc::new(PLMutex::new(StdHashMap::new()));
        let externals: ExternalRegistry = Arc::new(PLMutex::new(StdHashMap::new()));
        let configs = Arc::new(PLMutex::new(StdHashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        BackendExitDetectionHandler::new().run(&ctx);

        assert!(registry.lock().is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn name_matches_module() {
        assert_eq!(
            BackendExitDetectionHandler::new().name(),
            "backend_exit_detection"
        );
    }
}
