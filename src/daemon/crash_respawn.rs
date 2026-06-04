//! Crash-respawn logic — extracted from daemon/mod.rs (#1382).
//!
//! #1339 DAEMON-AUTONOMIC, GATE-EXEMPT BY DESIGN: this structural mutation
//! (respawning a crashed agent) is reached ONLY from the per-tick daemon loop on
//! an internal trigger — an agent process exit (`AgentExitEvent`) — never from
//! the API socket. It is a third trusted principal (daemon self-heal), distinct
//! from the socket-ingress principals (operator-transport vs agent-transport)
//! that `api::operator_gate` governs, so the operator-mode gate intentionally
//! does NOT apply here: the fleet keeps self-healing even in away/sleep. An
//! agent cannot invoke this (it can at most crash ITSELF → its own respawn).

use crate::agent::{self, AgentRegistry};
use crate::channel::NotifySeverity;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::{run_dir, serve_agent_tui, AgentConfig, DaemonContext};

pub(super) fn handle_crash_respawn(home: &Path, crashed_name: &str, ctx: &DaemonContext) {
    tracing::warn!(agent = %crashed_name, "crashed");
    crate::event_log::log(home, "crash", crashed_name, "agent crashed");

    let config = match ctx.configs.lock().get(crashed_name).cloned() {
        Some(c) => c,
        None => {
            tracing::debug!(agent = %crashed_name, "no config for respawn (likely deleted)");
            return;
        }
    };

    // #1441: registry is UUID-keyed; resolve the crashed name via fleet.yaml.
    let Some(instance_id) = crate::fleet::resolve_uuid(home, crashed_name) else {
        tracing::warn!(agent = %crashed_name, "no fleet UUID, skipping respawn");
        return;
    };

    // #1701: is the crashed agent its OWN team orchestrator? Resolved here (a
    // teams-file read) BEFORE taking the registry lock, so no file IO runs under
    // the lock (#1530 class). A self-orchestrator crash has no peer to relay an
    // inbox P0, so it escalates straight to the operator (below).
    let is_self_orch = crate::teams::is_self_orchestrator(home, crashed_name);

    let (should_respawn, delay, should_notify, fire_self_orch_p0, escalation_snapshot) = {
        let reg = agent::lock_registry(&ctx.registry);
        match reg.get(&instance_id) {
            Some(handle) => {
                let mut core = handle.core.lock();
                // #1701: decide the self-orch P0 BEFORE record_crash (which may
                // itself stamp the crash cooldown on its recent>=2 path) — the
                // accessor reads+stamps the crash-class cooldown (#1744-H3:
                // distinct from the hung cooldown), and for a self-orchestrator we
                // use ONLY this gate, never the generic recent>=2 one. The `&&`
                // short-circuits for non-orchestrators, so their cooldown is
                // untouched.
                let fire_p0 = is_self_orch && core.health.self_orch_crash_should_notify();
                let (respawn, delay, notify) = core.health.record_crash();
                // #1744-H2: snapshot the (just-mutated) escalation state under the
                // lock; persisted lock-free below so the crash budget + cooldown
                // survive a daemon restart.
                let snap = core.health.escalation_snapshot();
                (respawn, delay, notify, fire_p0, snap)
            }
            None => {
                tracing::warn!(agent = %crashed_name, "not in registry, skipping");
                return;
            }
        }
    };

    // #1744-H2: persist the crash budget + crash cooldown (lock released above).
    crate::daemon::escalation_persist::persist(home, crashed_name, &escalation_snapshot);

    // #1701: a self-orchestrator crash escalates to the operator on EVERY crash
    // (cooldown-gated) — the team is leaderless until respawn and no peer can
    // relay. A non-orchestrator agent keeps the generic recent>=2 crash notify.
    if fire_self_orch_p0 {
        notify_self_orch_crash(crashed_name, &instance_id, &ctx.registry);
    } else if !is_self_orch && should_notify {
        notify_crash(crashed_name, &instance_id, &ctx.registry);
    }

    if !should_respawn {
        tracing::warn!(agent = %crashed_name, "max retries exceeded, not respawning");
        return;
    }

    tracing::info!(agent = %crashed_name, ?delay, "respawning");
    let saved_health = {
        let r = ctx.registry.lock();
        r.get(&instance_id).map(|h| h.core.lock().health.clone())
    };

    let reg = Arc::clone(&ctx.registry);
    let home = home.to_path_buf();
    let tx = ctx.crash_tx.clone();
    let shutdown_for_respawn = Arc::clone(&ctx.shutdown);
    let name_for_err = crashed_name.to_owned();
    // fire-and-forget: respawn worker is short-lived (sleep delay then
    // spawn_agent + restore health + start TUI server). Observes
    // shutdown flag immediately after backoff to abort cleanly.
    if let Err(e) = std::thread::Builder::new()
        .name(format!("{crashed_name}_respawn"))
        .spawn(move || {
            respawn_agent_worker(
                &home,
                config,
                delay,
                saved_health,
                &reg,
                tx,
                &shutdown_for_respawn,
            );
        })
    {
        tracing::warn!(agent = %name_for_err, error = %e, "failed to spawn respawn thread");
    }
}

/// #1701: self-orchestrator-crash P0 — distinct from the generic [`notify_crash`]
/// in WORDING (it names the leaderless-team / no-peer-relay condition the
/// operator must act on) and in TRIGGER (fired on every orchestrator crash that
/// clears the cooldown, see [`crate::health::HealthTracker::self_orch_crash_should_notify`],
/// not the generic recent>=2 gate). Same `gated_notify(Error)` Sleep-penetrating
/// path as #1595's AuthError self-orch escalation.
fn notify_self_orch_crash(
    crashed_name: &str,
    instance_id: &crate::types::InstanceId,
    registry: &AgentRegistry,
) {
    let state = {
        let reg = agent::lock_registry(registry);
        reg.get(instance_id)
            .map(|h| h.core.lock().health.state.display_name())
            .unwrap_or("unknown")
    };
    tracing::warn!(agent = %crashed_name, %state, "#1701: self-orchestrator crashed — escalating P0 to operator");
    let msg = format!(
        "🛑 {crashed_name} (team orchestrator) crashed [{state}] — the team is leaderless \
         until it respawns and no peer can relay this. The respawn loses its in-memory \
         context; check for a crash-loop / re-prime it. Manual intervention may be required."
    );
    if let Some(ch) = crate::channel::active_channel() {
        let _ = crate::channel::gated_notify(
            ch.as_ref(),
            crashed_name,
            NotifySeverity::Error,
            &msg,
            false,
        );
    } else {
        tracing::debug!(agent = %crashed_name, "no active channel for self-orch crash P0");
    }
}

fn notify_crash(
    crashed_name: &str,
    instance_id: &crate::types::InstanceId,
    registry: &AgentRegistry,
) {
    let state = {
        let reg = agent::lock_registry(registry);
        reg.get(instance_id)
            .map(|h| h.core.lock().health.state.display_name())
            .unwrap_or("unknown")
    };
    tracing::warn!(agent = %crashed_name, %state, "notifying");
    let msg = format!("[health] {crashed_name}: {state}");
    if let Some(ch) = crate::channel::active_channel() {
        let _ = crate::channel::gated_notify(
            ch.as_ref(),
            crashed_name,
            NotifySeverity::Error,
            &msg,
            false,
        );
    } else {
        tracing::debug!(agent = %crashed_name, "no active channel for crash notification");
    }
}

fn respawn_agent_worker(
    home: &Path,
    config: AgentConfig,
    delay: std::time::Duration,
    saved_health: Option<crate::health::HealthTracker>,
    reg: &AgentRegistry,
    tx: crossbeam_channel::Sender<crate::agent::AgentExitEvent>,
    shutdown: &Arc<AtomicBool>,
) {
    std::thread::sleep(delay);
    if shutdown.load(Ordering::Relaxed) {
        tracing::info!(agent = %config.name, "shutdown during respawn backoff, aborting");
        return;
    }
    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    if let Some(ref wd) = config.working_dir {
        let skills_filter: Option<Vec<String>> =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                .ok()
                .and_then(|c| c.instances.get(&config.name).and_then(|i| i.skills.clone()));
        let backend_skill = crate::backend::Backend::from_command(&config.backend_command)
            .and_then(|b| b.skill_dir_name());
        if let Err(e) = crate::skills::install_for_agent_backend(
            home,
            wd,
            skills_filter.as_deref(),
            backend_skill,
        ) {
            tracing::warn!(agent = %config.name, error = %e, "crash-respawn skills install failed");
        }
    }
    match agent::spawn_agent(
        &agent::SpawnConfig {
            name: &config.name,
            backend_command: &config.backend_command,
            args: &config.args,
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols,
            rows,
            env: config.env.as_ref(),
            working_dir: config.working_dir.as_deref(),
            submit_key: &config.submit_key,
            home: Some(home),
            crash_tx: Some(tx),
            shutdown: Some(Arc::clone(shutdown)),
        },
        reg,
    ) {
        Ok(()) => {
            tracing::info!(agent = %config.name, "respawned");
            crate::event_log::log(home, "respawn", &config.name, "agent respawned");
            // #1441: registry is UUID-keyed; resolve the respawned name once.
            let respawned_id = crate::fleet::resolve_uuid(home, &config.name);
            {
                let r = reg.lock();
                if let Some(handle) = respawned_id.and_then(|id| r.get(&id)) {
                    let mut core = handle.core.lock();
                    if let Some(ref old_health) = saved_health {
                        core.health = old_health.clone();
                    }
                    core.health.respawn_ok();
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
            {
                // #1530/F1: snapshot the writer + crash reason under the
                // registry lock, then RELEASE it before the blocking PTY write.
                let snap = {
                    let r = reg.lock();
                    respawned_id.and_then(|id| r.get(&id)).map(|handle| {
                        let reason = handle.core.lock().health.crash_reason().to_string();
                        (agent::InjectTarget::from_handle(handle), reason)
                    })
                };
                if let Some((tgt, reason)) = snap {
                    let msg = format!(
                        "[system] Agent restarted due to {reason}. Previous context was lost.\r"
                    );
                    let _ = agent::write_to_pty(&tgt.pty_writer, msg.as_bytes());
                }
            }
            let rdir = run_dir(home);
            let n = config.name.clone();
            let n_err = n.clone();
            let reg2 = Arc::clone(reg);
            // fire-and-forget: respawn-time TUI server exits when the agent
            // is removed from the registry (socket-file removal in
            // delete_transaction).
            if let Err(e) = std::thread::Builder::new()
                .name(format!("{n}_tui_server"))
                .spawn(move || serve_agent_tui(&n, &rdir, &reg2))
            {
                tracing::warn!(agent = %n_err, error = %e, "failed to spawn TUI server");
            }
        }
        Err(e) => {
            tracing::warn!(agent = %config.name, error = %e, "respawn failed");
        }
    }
}
