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

use crate::agent::crash_disposition::{ClaimToken, Claimant, CrashObservation};
use crate::agent::{self, AgentRegistry};
use crate::channel::NotifySeverity;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(test)]
use std::sync::mpsc::{Receiver, Sender};
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

use super::{run_dir, serve_agent_tui, AgentConfig, DaemonContext};

#[cfg(test)]
type TestWorkerGate = (Sender<()>, Receiver<()>, Sender<()>);

#[cfg(test)]
static TEST_WORKER_GATE: OnceLock<Mutex<Option<TestWorkerGate>>> = OnceLock::new();

#[cfg(test)]
fn install_test_worker_gate(gate: TestWorkerGate) {
    *TEST_WORKER_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("test worker gate lock") = Some(gate);
}

#[cfg(test)]
fn await_test_worker_gate() -> Option<Sender<()>> {
    let gate = TEST_WORKER_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("test worker gate lock")
        .take();
    let (entered, release, done) = gate?;
    let _ = entered.send(());
    let _ = release.recv();
    Some(done)
}

#[cfg(test)]
fn signal_test_worker_done(done: &Option<Sender<()>>) {
    if let Some(done) = done {
        let _ = done.clone().send(());
    }
}

/// Compatibility entry used by legacy tests and name-triggered internal call
/// sites. Production PTY events use [`handle_crash_observation`] directly.
#[allow(dead_code)]
pub(super) fn handle_crash_respawn(home: &Path, crashed_name: &str, ctx: &DaemonContext) {
    let Some(instance_id) = crate::fleet::resolve_uuid(home, crashed_name) else {
        tracing::warn!(agent = %crashed_name, "no fleet UUID, skipping respawn");
        return;
    };
    let observation = {
        let reg = agent::lock_registry(&ctx.registry);
        reg.get(&instance_id).map(|handle| CrashObservation {
            instance_id,
            generation: handle.generation,
            core: Arc::clone(&handle.core),
            deleted: Arc::clone(&handle.deleted),
            // Name-triggered compatibility entry is not a source publication;
            // the source event already carries its own shutdown Arc.
            owner_shutdown: None,
            name: handle.name.clone(),
        })
    };
    let Some(observation) = observation else {
        return;
    };
    handle_crash_observation(home, &observation, ctx);
}

pub(super) fn handle_crash_observation(
    home: &Path,
    observation: &CrashObservation,
    ctx: &DaemonContext,
) {
    let crashed_name = observation.name.as_str();
    let key = observation.key();
    let ledger = agent::crash_disposition::owner_ledger();
    if ledger.disposition(key).is_none() && !ledger.publish(observation.clone()) {
        return;
    }
    let Some(claim) = ledger.claim(key, Claimant::Crash) else {
        return;
    };
    if !ledger.mark_ready(claim) {
        return;
    }
    tracing::warn!(agent = %crashed_name, "crashed");
    crate::event_log::log(home, "crash", crashed_name, "agent crashed");

    let config = match ctx.configs.lock().get(crashed_name).cloned() {
        Some(c) => c,
        None => {
            tracing::debug!(agent = %crashed_name, "no config for respawn (likely deleted)");
            ledger.discard(key);
            return;
        }
    };

    let instance_id = observation.instance_id;

    // #1701: is the crashed agent its OWN team orchestrator? Resolved here (a
    // teams-file read) BEFORE taking the registry lock, so no file IO runs under
    // the lock (#1530 class). A self-orchestrator crash has no peer to relay an
    // inbox P0, so it escalates straight to the operator (below).
    // #1744-M7: 3-state. The crash path stays CONSERVATIVE — only a determinate
    // `Yes` fires the self-orch P0; `Unknown` (teams config unreadable) falls
    // back to the generic recent>=2 notify rather than firing the more-aggressive
    // leaderless page off an indeterminate read. (The no-peer hung/AuthError
    // paths fail the other way — they escalate on `Unknown`.)
    let self_orch = crate::teams::self_orch_status(home, crashed_name);

    enum RegistryOutcome {
        Deleted,
        Missing,
        Healthy { delay: std::time::Duration },
    }

    // Snapshot health and the deleted/missing outcome while holding the
    // registry guard. Ledger settlement happens after this guard drops.
    let registry_outcome = {
        let reg = agent::lock_registry(&ctx.registry);
        match reg.get(&instance_id) {
            Some(handle) => {
                // #1913: an INTENTIONAL delete must not be mistaken for a crash.
                // `delete_transaction` STORES `handle.deleted = true` (lifecycle.rs)
                // BEFORE it kills the backend process — but it removes the registry
                // and config entries only AFTER the kill + exit-wait. The kill is
                // observed here as an exit classified `Crash`, so without this gate
                // the crash-respawn loop races those removals and RESURRECTS the
                // just-deleted instance: it re-spawns the process and re-creates
                // `workspace/<name>`, which re-leaks the per-instance stores teardown
                // just cleaned (the intermittent residual root of the #1902–#1909
                // teardown class). Because `deleted` is Stored before the kill, this
                // Acquire load reliably observes `true` by the time the exit lands,
                // and once the registry entry IS removed `reg.get` returns `None`
                // (the `None` arm below) — so this check covers exactly the racy
                // window. Treat it as a clean exit: no respawn.
                if handle.deleted.load(std::sync::atomic::Ordering::Acquire) {
                    tracing::info!(
                        agent = %crashed_name,
                        "exit is an intentional delete (deleted flag set) — skipping crash-respawn"
                    );
                    RegistryOutcome::Deleted
                } else {
                    // Project the backoff without mutating health.  Retry
                    // accounting is committed only after the worker admits its
                    // exact-generation execution permit.
                    let delay = handle.core.lock().health.next_crash_delay();
                    RegistryOutcome::Healthy { delay }
                }
            }
            None => {
                tracing::warn!(agent = %crashed_name, "not in registry, skipping");
                RegistryOutcome::Missing
            }
        }
    };

    let delay = match registry_outcome {
        RegistryOutcome::Deleted | RegistryOutcome::Missing => {
            ledger.discard(key);
            return;
        }
        RegistryOutcome::Healthy { delay } => delay,
    };

    tracing::info!(agent = %crashed_name, ?delay, "respawning");

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
                None,
                &reg,
                tx,
                &shutdown_for_respawn,
                Some(claim),
                self_orch,
                instance_id,
            );
        })
    {
        ledger.discard(key);
        tracing::warn!(agent = %name_for_err, error = %e, "failed to spawn respawn thread");
    }
}

/// Advisory-channel backstop. A full or disconnected crash channel leaves the
/// exact observation Pending in the owner ledger; the per-tick watchdog calls
/// this sweep so recovery does not depend on delivery of that wake-up.
pub(crate) fn sweep_pending_dispositions(
    home: &Path,
    registry: &AgentRegistry,
    externals: &crate::agent::ExternalRegistry,
    configs: &crate::api::ConfigRegistry,
) {
    let pending = agent::crash_disposition::owner_ledger().pending();
    if pending.is_empty() {
        return;
    }
    let tx = agent::crash_disposition::owner_crash_wake()
        .unwrap_or_else(|| crossbeam_channel::unbounded().0);
    let ctx = DaemonContext {
        registry: Arc::clone(registry),
        externals: Arc::clone(externals),
        configs: Arc::clone(configs),
        crash_tx: tx,
        crash_rx: crossbeam_channel::never(),
        shutdown: Arc::new(AtomicBool::new(false)),
    };
    for observation in pending {
        handle_crash_observation(home, &observation, &ctx);
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
    // #1744-M6: every registered channel — a leaderless-orchestrator P0 must not
    // be dropped just because the fleet runs multiple channels.
    crate::channel::notify_all_escalation_channels(
        crashed_name,
        NotifySeverity::Error,
        &msg,
        false,
    );
}

/// #1744-H4: should the terminal-Failed self-orchestrator P0 fire? True iff the
/// agent will NOT respawn (max-retries Failed), it is a self-orchestrator
/// (fail-closed: `Yes`|`Unknown` fire, `No` skip — a leaderless death is too
/// costly to miss on an indeterminate teams read), and it has not already been
/// terminally paged (`failed_escalated`, persisted for cross-restart once-off).
#[cfg(test)]
fn should_fire_terminal_p0(
    should_respawn: bool,
    self_orch: crate::teams::SelfOrchStatus,
    failed_escalated: bool,
) -> bool {
    !should_respawn && self_orch != crate::teams::SelfOrchStatus::No && !failed_escalated
}

/// #1744-H4: terminal-Failed self-orchestrator P0 — fired exactly ONCE when a
/// self-orchestrator exhausts its respawn budget and will NOT be respawned. The
/// team is permanently leaderless until the operator intervenes. Distinct from
/// the per-crash [`notify_self_orch_crash`] in WORDING (permanent death, not
/// "until respawn") and TRIGGER (cooldown-EXEMPT once-off, latched on the
/// persisted `failed_escalated`). Routes through PR-A's
/// `notify_all_escalation_channels` (#1744-M6) so the page reaches every channel.
fn notify_self_orch_terminal(crashed_name: &str) {
    tracing::error!(
        agent = %crashed_name,
        "#1744-H4: self-orchestrator PERMANENTLY FAILED (respawn budget exhausted) — escalating terminal P0"
    );
    let msg = format!(
        "🛑 Self-orchestrator `{crashed_name}` has PERMANENTLY FAILED — it crashed past \
         its auto-retry budget and will NOT be respawned. Its team is leaderless and no \
         peer can relay this: manual operator intervention is required (restart / reassign \
         the orchestrator)."
    );
    crate::channel::notify_all_escalation_channels(
        crashed_name,
        NotifySeverity::Error,
        &msg,
        false,
    );
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
    // #1744-M6: every registered channel (multi-channel-safe).
    crate::channel::notify_all_escalation_channels(
        crashed_name,
        NotifySeverity::Error,
        &msg,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
fn respawn_agent_worker(
    home: &Path,
    config: AgentConfig,
    delay: std::time::Duration,
    saved_health: Option<crate::health::HealthTracker>,
    reg: &AgentRegistry,
    tx: crossbeam_channel::Sender<crate::agent::AgentExitEvent>,
    shutdown: &Arc<AtomicBool>,
    claim: Option<ClaimToken>,
    self_orch: crate::teams::SelfOrchStatus,
    instance_id: crate::types::InstanceId,
) {
    #[cfg(test)]
    let test_done = if claim.is_some() {
        await_test_worker_gate()
    } else {
        None
    };
    #[cfg(not(test))]
    let _test_done: Option<()> = None;
    #[cfg(test)]
    if test_done.is_none() {
        std::thread::sleep(delay);
    }
    #[cfg(not(test))]
    std::thread::sleep(delay);
    if shutdown.load(Ordering::Relaxed) {
        if let Some(token) = claim {
            agent::crash_disposition::owner_ledger().discard(token.key());
        }
        tracing::info!(agent = %config.name, "shutdown during respawn backoff, aborting");
        #[cfg(test)]
        signal_test_worker_done(&test_done);
        return;
    }
    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    if let Some(ref wd) = config.working_dir {
        let skills_filter: Option<Vec<String>> =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                .ok()
                .and_then(|c| c.instances.get(&config.name).and_then(|i| i.skills.clone()));
        let custom_skills_source: Option<std::path::PathBuf> =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                .ok()
                .and_then(|c| {
                    c.instances
                        .get(&config.name)
                        .and_then(|i| i.skills_path.clone())
                })
                .map(|p| crate::fleet::resolve::expand_tilde_path(&p));
        let backend_skill = config
            .backend
            .clone()
            .or_else(|| crate::backend::Backend::from_command(&config.backend_command))
            .and_then(|b| b.skill_dir_name());
        if let Err(e) = crate::skills::install_for_agent_backend_with_source(
            home,
            wd,
            skills_filter.as_deref(),
            backend_skill,
            custom_skills_source.as_deref(),
        ) {
            tracing::warn!(agent = %config.name, error = %e, "crash-respawn skills install failed");
        }
    }

    // Re-check shutdown after backoff/setup, then admit the exact-generation
    // permit immediately before the replacement spawn.  Until this point the
    // old core remains visibly Crashed rather than prematurely Restarting.
    if shutdown.load(Ordering::Relaxed) {
        if let Some(token) = claim {
            agent::crash_disposition::owner_ledger().discard(token.key());
        }
        tracing::info!(agent = %config.name, "shutdown before respawn execution admission, aborting");
        #[cfg(test)]
        signal_test_worker_done(&test_done);
        return;
    }
    let mut permit = match claim {
        Some(token) => {
            let Some(mut permit) = agent::crash_disposition::owner_ledger().begin_execute(token)
            else {
                tracing::info!(agent = %config.name, "exact-generation recovery was discarded before execution");
                #[cfg(test)]
                signal_test_worker_done(&test_done);
                return;
            };
            if !permit.admit_restarting() {
                tracing::info!(agent = %config.name, "execution permit could not admit Restarting");
                #[cfg(test)]
                signal_test_worker_done(&test_done);
                return;
            }
            Some(permit)
        }
        None => None,
    };
    let mut saved_health = saved_health;
    if permit.is_some() {
        let attempt = permit
            .as_mut()
            .and_then(|permit| permit.debit_attempt(self_orch));
        let Some(attempt) = attempt else {
            tracing::warn!(agent = %config.name, "exact-generation recovery permit was already debited");
            if let Some(permit) = permit.take() {
                let _ = agent::crash_disposition::owner_ledger().mark_failed(permit);
            }
            #[cfg(test)]
            signal_test_worker_done(&test_done);
            return;
        };
        let exact_core = permit
            .as_ref()
            .expect("permit remains after debit")
            .exact_core();
        saved_health = Some(exact_core.lock().health.clone());
        tracing::debug!(
            agent = %config.name,
            ?attempt.delay,
            "exact-generation crash attempt admitted"
        );
        crate::event_log::log(
            home,
            "crash_respawn_attempt",
            &config.name,
            "exact-generation recovery permit admitted one retry attempt",
        );
        crate::daemon::escalation_persist::persist(
            home,
            &config.name,
            &attempt.escalation_snapshot,
        );
        if attempt.fire_terminal_p0 {
            notify_self_orch_terminal(&config.name);
        } else if attempt.fire_self_orch_p0 {
            notify_self_orch_crash(&config.name, &instance_id, reg);
        } else if self_orch != crate::teams::SelfOrchStatus::Yes && attempt.should_notify {
            notify_crash(&config.name, &instance_id, reg);
        }
        if !attempt.should_respawn {
            tracing::warn!(agent = %config.name, "max retries exceeded, not respawning");
            if let Some(permit) = permit.take() {
                let _ = agent::crash_disposition::owner_ledger().mark_failed(permit);
            }
            #[cfg(test)]
            signal_test_worker_done(&test_done);
            return;
        }
    }
    match agent::spawn_agent(
        &agent::SpawnConfig {
            name: &config.name,
            backend: config.backend.as_ref(),
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
        Ok(_) => {
            if let Some(permit) = permit.take() {
                let _ = agent::crash_disposition::owner_ledger().mark_live(permit);
            }
            tracing::info!(agent = %config.name, "respawned");
            crate::event_log::log(home, "respawn", &config.name, "agent respawned");
            // #1441: registry is UUID-keyed; resolve the respawned name once.
            let respawned_id = crate::fleet::resolve_uuid(home, &config.name);
            {
                let r = reg.lock();
                if let Some(handle) = respawned_id.and_then(|id| r.get(&id)) {
                    let is_alive = handle
                        .child
                        .lock()
                        .process_id()
                        .map(crate::process::is_pid_alive)
                        .unwrap_or(false);
                    let mut core = handle.core.lock();
                    if let Some(ref old_health) = saved_health {
                        core.health = old_health.clone();
                    }
                    core.health.respawn_ok(is_alive);
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
            if let Some(permit) = permit.take() {
                let _ = agent::crash_disposition::owner_ledger().mark_failed(permit);
            }
            tracing::warn!(agent = %config.name, error = %e, "respawn failed");
            crate::event_log::log(
                home,
                "crash_respawn_failed",
                &config.name,
                &format!("error: {e}"),
            );
            let msg = format!("🛑 Agent `{}` crash-respawn failed: {}", config.name, e);
            crate::channel::notify_all_escalation_channels(
                &config.name,
                NotifySeverity::Error,
                &msg,
                false,
            );
            let respawned_id = crate::fleet::resolve_uuid(home, &config.name);
            if let Some(id) = respawned_id {
                let r = reg.lock();
                if let Some(handle) = r.get(&id) {
                    let mut core = handle.core.lock();
                    core.health.respawn_failed();
                }
            }
        }
    }
    #[cfg(test)]
    signal_test_worker_done(&test_done);
}

#[cfg(test)]
mod tests {
    use super::should_fire_terminal_p0;
    use crate::teams::SelfOrchStatus;

    /// #1744-H4: the terminal self-orch P0 fires for a Failed (no-respawn)
    /// self-orchestrator — fail-closed (Yes|Unknown), skipped for No / non-terminal,
    /// and exactly once (the persisted `failed_escalated` latch suppresses re-page,
    /// so a daemon restart doesn't re-page the same permanent death).
    #[test]
    fn terminal_p0_fires_for_failed_self_orch_once_1744_h4() {
        // Terminal + self-orch (fail-closed) + not yet paged → fire.
        assert!(should_fire_terminal_p0(false, SelfOrchStatus::Yes, false));
        assert!(
            should_fire_terminal_p0(false, SelfOrchStatus::Unknown, false),
            "fail-closed: Unknown must still fire the leaderless-death P0"
        );
        // Not a self-orchestrator → skip (keeps the generic crash notify).
        assert!(!should_fire_terminal_p0(false, SelfOrchStatus::No, false));
        // Still respawning (non-terminal) → not a terminal page.
        assert!(!should_fire_terminal_p0(true, SelfOrchStatus::Yes, false));
        // Once-off: already terminally paged → never re-page.
        assert!(
            !should_fire_terminal_p0(false, SelfOrchStatus::Yes, true),
            "#1744-H4 once-off: an already-paged terminal self-orch must not re-page"
        );
    }
}

/// #1913: the delete-vs-crash-respawn gate. `delete_transaction` Stores
/// `handle.deleted = true` BEFORE killing the backend; the resulting exit is
/// classified `Crash`, so `handle_crash_respawn` must honor the flag and skip
/// respawn — otherwise it RESURRECTS the just-deleted instance (re-spawns the
/// process + re-creates `workspace/<name>`, re-leaking teardown-cleaned stores;
/// the intermittent residual root of the #1902–#1909 teardown class).
///
/// These two tests prove the gate is PRECISE — it suppresses respawn ONLY for a
/// deleted handle, while a genuine crash (deleted=false) still enters the
/// respawn path (no crash-recovery regression). The observable is the handle's
/// `health.total_crashes`: `record_crash` runs (and bumps it) only AFTER the
/// gate, so `0` proves the gate fired and `1` proves it let a real crash through.
#[cfg(test)]
mod deleted_gate_tests_1913 {
    use super::{handle_crash_observation, handle_crash_respawn};
    use super::{AgentConfig, DaemonContext};
    use crate::agent::{AgentHandle, AgentRegistry};
    use crate::types::InstanceId;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;

    const VICTIM: &str = "victim";
    const VICTIM_UUID: &str = "11111111-2222-3333-4444-555555555555";

    /// Isolated `/tmp` home seeded with a fleet.yaml so `resolve_uuid(home,
    /// VICTIM)` → VICTIM_UUID (else `handle_crash_respawn` bails before the gate).
    fn tmp_home(tag: &str) -> PathBuf {
        static C: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agend-crashgate-{}-{}-{}",
            std::process::id(),
            tag,
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("fleet.yaml"),
            format!("instances:\n  {VICTIM}:\n    command: \"true\"\n    id: {VICTIM_UUID}\n"),
        )
        .expect("fleet write");
        dir
    }

    /// Registry handle for VICTIM pinned to VICTIM_UUID (so `resolve_uuid` and
    /// `reg.get` align), backed by a real already-exited `true` child PTY.
    fn make_handle(deleted: bool) -> AgentHandle {
        use portable_pty::{native_pty_system, PtySize};
        let generation = crate::agent::crash_disposition::owner_generation_source().next();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = portable_pty::CommandBuilder::new("true");
        cmd.cwd(std::env::temp_dir());
        let child = pair.slave.spawn_command(cmd).expect("spawn true");
        drop(pair.slave);
        let pty_writer: crate::agent::PtyWriter =
            Arc::new(Mutex::new(pair.master.take_writer().expect("writer")));
        let pty_master = Arc::new(Mutex::new(pair.master));
        let core = Arc::new(crate::sync_audit::CoreMutex::new(crate::agent::AgentCore {
            vterm: crate::vterm::VTerm::with_pty_writer(80, 24, Arc::clone(&pty_writer)),
            subscribers: Vec::new(),
            state: crate::state::StateTracker::new(None),
            health: crate::health::HealthTracker::new(),
            api_activity: crate::agent::ApiActivity::default(),
            observed_status: None,
        }));
        AgentHandle {
            id: InstanceId::parse(VICTIM_UUID).expect("uuid"),
            name: VICTIM.to_string().into(),
            declared_backend: None,
            backend_command: "true".to_string(),
            pty_writer,
            pty_master,
            published_state: crate::agent::published_state_of(&core),
            published_observed: crate::agent::published_observed_of(&core),
            core,
            child: Arc::new(Mutex::new(child)),
            submit_key: "\r".to_string(),
            inject_prefix: String::new(),
            typed_inject: false,
            spawned_at: std::time::Instant::now(),
            spawned_at_epoch_ms: 0,
            spawn_mode: crate::backend::SpawnMode::Fresh,
            generation,
            deleted: Arc::new(AtomicBool::new(deleted)),
        }
    }

    fn make_ctx(registry: AgentRegistry) -> DaemonContext {
        let mut configs = HashMap::new();
        configs.insert(
            VICTIM.to_string(),
            AgentConfig {
                name: VICTIM.to_string(),
                backend: None,
                backend_command: "true".to_string(),
                args: vec![],
                env: None,
                working_dir: None,
                submit_key: "\r".to_string(),
            },
        );
        let (crash_tx, crash_rx) = crossbeam_channel::unbounded();
        DaemonContext {
            registry,
            externals: Arc::new(Mutex::new(HashMap::new())),
            configs: Arc::new(Mutex::new(configs)),
            crash_tx,
            crash_rx,
            shutdown: Arc::new(AtomicBool::new(true)),
        }
    }

    fn worker_gate() -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        super::install_test_worker_gate((entered_tx, release_rx, done_tx));
        (entered_rx, release_tx, done_rx)
    }

    fn total_crashes(reg: &AgentRegistry) -> u32 {
        let id = InstanceId::parse(VICTIM_UUID).expect("valid uuid");
        let r = reg.lock();
        let handle = r.get(&id).expect("handle present");
        let core = handle.core.lock();
        core.health.total_crashes
    }

    /// (a) An intentional delete (deleted=true) must NOT respawn: the gate
    /// returns before `record_crash`, so the crash budget is untouched.
    #[test]
    #[serial_test::serial(crash_respawn_gate)]
    fn delete_does_not_respawn_1913() {
        let home = tmp_home("del");
        let reg: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let handle = make_handle(true);
        crate::agent::crash_disposition::owner_ledger()
            .register_generation(handle.id, handle.generation);
        reg.lock()
            .insert(InstanceId::parse(VICTIM_UUID).expect("valid uuid"), handle);
        let ctx = make_ctx(Arc::clone(&reg));

        handle_crash_respawn(&home, VICTIM, &ctx);

        assert_eq!(
            total_crashes(&reg),
            0,
            "#1913: a deleted handle must skip the respawn path entirely \
             (record_crash must not run) — the kill is a teardown, not a crash"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// (b) A genuine crash (deleted=false) MUST still respawn: the gate lets it
    /// through to `record_crash` (crash budget bumped) — proving the #1913 gate
    /// is precise and did NOT blanket-disable crash recovery.
    #[test]
    #[serial_test::serial(crash_respawn_gate)]
    fn real_crash_still_respawns_1913() {
        let home = tmp_home("crash");
        let reg: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let handle = make_handle(false);
        handle.core.lock().health.total_crashes = 3;
        crate::agent::crash_disposition::owner_ledger()
            .register_generation(handle.id, handle.generation);
        reg.lock()
            .insert(InstanceId::parse(VICTIM_UUID).expect("valid uuid"), handle);
        let ctx = DaemonContext {
            shutdown: Arc::new(AtomicBool::new(false)),
            ..make_ctx(Arc::clone(&reg))
        };
        ctx.configs
            .lock()
            .get_mut(VICTIM)
            .expect("victim config")
            .backend_command = "missing-real-crash-command".to_string();
        let (entered_rx, release_tx, done_rx) = worker_gate();

        handle_crash_respawn(&home, VICTIM, &ctx);

        let debit_deferred = total_crashes(&reg) == 3;
        entered_rx.recv().expect("respawn worker entered gate");
        release_tx.send(()).expect("release respawn worker");
        done_rx.recv().expect("respawn worker completed");
        assert!(
            debit_deferred,
            "RED: genuine crash debit must wait for admission; got total_crashes={} before gate release",
            total_crashes(&reg)
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Slice-3 RED: a replacement invalidated while the worker is in its
    /// backoff must not consume the crash budget or write a persisted count.
    #[test]
    #[serial_test::serial(crash_respawn_gate)]
    fn superseded_before_execution_does_not_debit_attempt_budget_slice3_red() {
        let home = tmp_home("superseded-before-execute");
        let reg: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let handle = make_handle(false);
        handle.core.lock().health.total_crashes = 3;
        let id = handle.id;
        let generation = handle.generation;
        reg.lock().insert(id, handle);
        let ctx = DaemonContext {
            shutdown: Arc::new(AtomicBool::new(false)),
            ..make_ctx(Arc::clone(&reg))
        };
        crate::agent::crash_disposition::owner_ledger().register_generation(id, generation);
        let observation = {
            let r = reg.lock();
            let h = r.get(&id).expect("handle");
            crate::agent::crash_disposition::CrashObservation {
                instance_id: id,
                generation,
                core: Arc::clone(&h.core),
                deleted: Arc::clone(&h.deleted),
                owner_shutdown: Some(Arc::clone(&ctx.shutdown)),
                name: h.name.clone(),
            }
        };
        let (entered_rx, release_tx, done_rx) = worker_gate();

        handle_crash_observation(&home, &observation, &ctx);
        let debit_deferred = total_crashes(&reg) == 3;

        entered_rx.recv().expect("respawn worker entered gate");
        let replacement = crate::agent::crash_disposition::owner_generation_source().next();
        crate::agent::crash_disposition::owner_ledger().register_generation(id, replacement);
        release_tx.send(()).expect("release respawn worker");
        done_rx.recv().expect("respawn worker completed");

        assert!(
            debit_deferred,
            "RED: debit must wait for execution admission; got total_crashes={} before gate release",
            total_crashes(&reg)
        );
        assert!(
            crate::daemon::escalation_persist::load_for(&home, VICTIM).is_none(),
            "superseded recovery must not persist an attempt"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Slice-3 RED: shutdown before exact execution admission is a rejected
    /// recovery, not an accepted crash attempt.
    #[test]
    #[serial_test::serial(crash_respawn_gate)]
    fn shutdown_before_execution_does_not_debit_attempt_budget_slice3_red() {
        let home = tmp_home("shutdown-before-execute");
        let reg: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let handle = make_handle(false);
        handle.core.lock().health.total_crashes = 3;
        let id = handle.id;
        let generation = handle.generation;
        reg.lock().insert(id, handle);
        let ctx = make_ctx(Arc::clone(&reg));
        crate::agent::crash_disposition::owner_ledger().register_generation(id, generation);
        let observation = {
            let r = reg.lock();
            let h = r.get(&id).expect("handle");
            crate::agent::crash_disposition::CrashObservation {
                instance_id: id,
                generation,
                core: Arc::clone(&h.core),
                deleted: Arc::clone(&h.deleted),
                owner_shutdown: None,
                name: h.name.clone(),
            }
        };
        let (entered_rx, release_tx, done_rx) = worker_gate();

        handle_crash_observation(&home, &observation, &ctx);
        let debit_deferred = total_crashes(&reg) == 3;
        entered_rx.recv().expect("respawn worker entered gate");
        release_tx.send(()).expect("release respawn worker");
        done_rx.recv().expect("respawn worker completed");
        assert!(
            debit_deferred,
            "RED: shutdown must reject before debit; got total_crashes={} before gate release",
            total_crashes(&reg)
        );
        assert!(crate::daemon::escalation_persist::load_for(&home, VICTIM).is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    /// Slice-3 RED: only an exact permit admitted for the old generation may
    /// consume one attempt and persist the resulting count/audit event.
    #[test]
    #[serial_test::serial(crash_respawn_gate)]
    fn exact_admission_debits_and_persists_once_slice3_red() {
        let home = tmp_home("exact-admission");
        let reg: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let handle = make_handle(false);
        handle.core.lock().health.total_crashes = 3;
        let id = handle.id;
        let generation = handle.generation;
        reg.lock().insert(id, handle);
        let ctx = make_ctx(Arc::clone(&reg));
        ctx.configs
            .lock()
            .get_mut(VICTIM)
            .expect("victim config")
            .backend_command = "missing-exact-attempt-command".to_string();
        crate::agent::crash_disposition::owner_ledger().register_generation(id, generation);
        let observation = {
            let r = reg.lock();
            let h = r.get(&id).expect("handle");
            crate::agent::crash_disposition::CrashObservation {
                instance_id: id,
                generation,
                core: Arc::clone(&h.core),
                deleted: Arc::clone(&h.deleted),
                owner_shutdown: Some(Arc::clone(&ctx.shutdown)),
                name: h.name.clone(),
            }
        };
        let ctx = DaemonContext {
            shutdown: Arc::new(AtomicBool::new(false)),
            ..ctx
        };
        let observation = crate::agent::crash_disposition::CrashObservation {
            owner_shutdown: Some(Arc::clone(&ctx.shutdown)),
            ..observation
        };
        let (entered_rx, release_tx, done_rx) = worker_gate();

        handle_crash_observation(&home, &observation, &ctx);
        let debit_deferred = total_crashes(&reg) == 3;
        entered_rx.recv().expect("respawn worker entered gate");
        release_tx.send(()).expect("release respawn worker");
        done_rx.recv().expect("respawn worker completed");
        assert!(
            debit_deferred,
            "debit is not raw-observation side effect; got total_crashes={} before gate release",
            total_crashes(&reg)
        );
        assert_eq!(total_crashes(&reg), 4);
        let persisted = crate::daemon::escalation_persist::load_for(&home, VICTIM)
            .expect("accepted attempt must persist escalation count");
        assert_eq!(persisted.total_crashes, 4);
        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert_eq!(
            log.matches("crash_respawn_attempt").count(),
            1,
            "one admitted permit must produce one accepted-attempt audit row"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_crash_respawn_failed_escalation() {
        let home = tmp_home("failed-spawn");
        let reg: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let handle = make_handle(false);
        let saved_health = handle.core.lock().health.clone();
        reg.lock()
            .insert(InstanceId::parse(VICTIM_UUID).expect("valid uuid"), handle);

        let config = AgentConfig {
            name: VICTIM.to_string(),
            backend: None,
            backend_command: "nonexistent-command-12345".to_string(),
            args: vec![],
            env: None,
            working_dir: None,
            submit_key: "\r".to_string(),
        };

        let (crash_tx, _crash_rx) = crossbeam_channel::unbounded();
        let shutdown = Arc::new(AtomicBool::new(false));

        // Call respawn_agent_worker directly and synchronously
        super::respawn_agent_worker(
            &home,
            config,
            std::time::Duration::ZERO,
            Some(saved_health),
            &reg,
            crash_tx,
            &shutdown,
            None,
            crate::teams::SelfOrchStatus::No,
            InstanceId::parse(VICTIM_UUID).expect("valid uuid"),
        );

        // Verify 1: event-log.jsonl should have a crash_respawn_failed record
        let log_content =
            std::fs::read_to_string(home.join("event-log.jsonl")).expect("event-log must exist");
        assert!(log_content.contains("crash_respawn_failed"));

        // Verify 2: health state in the registry should be HealthState::Failed
        {
            let r = reg.lock();
            let handle = r
                .get(&InstanceId::parse(VICTIM_UUID).expect("valid uuid"))
                .expect("handle must exist");
            let core = handle.core.lock();
            assert_eq!(core.health.state, crate::health::HealthState::Failed);
        }

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn missing_registry_after_ready_is_discarded() {
        let home = tmp_home("missing-registry");
        let handle = make_handle(false);
        let observation = crate::agent::crash_disposition::CrashObservation {
            // Keep this key unique so parallel tests cannot reuse the fixed
            // fleet UUID used by the legacy delete-gate fixtures.
            instance_id: InstanceId::new(),
            generation: handle.generation,
            core: Arc::clone(&handle.core),
            deleted: Arc::clone(&handle.deleted),
            owner_shutdown: None,
            name: handle.name.clone(),
        };
        let key = observation.key();
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let ctx = make_ctx(registry);

        handle_crash_observation(&home, &observation, &ctx);

        assert_eq!(
            crate::agent::crash_disposition::owner_ledger().disposition(key),
            Some(crate::agent::crash_disposition::Disposition::Discarded)
        );
        assert!(
            !crate::agent::crash_disposition::owner_ledger()
                .pending()
                .iter()
                .any(|pending| pending.key() == key),
            "a missing registry must not strand a Ready recovery as Pending"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
