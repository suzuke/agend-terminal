//! Lifecycle transactions for agent spawn / delete / kill flows.
//!
//! Centralizes partial-failure rollback so the spawn / delete / kill paths
//! cannot leak orphan PIDs, phantom registry entries, or stale Telegram
//! bindings.
//!
//! Audit context: Sprint 20 Track B audit (DAEMON.md F1/F2/F3/F5) +
//! Sprint 20.5 Track A peer-pass cross-validation (Telegram binding leak in F3).
//!
//! Two surfaces:
//! - [`SpawnRollback`] — RAII guard wrapped around `agent::spawn_agent`'s
//!   ordered mutations; arms on construction, disarms on `commit()`. On Drop
//!   while armed, undoes whatever steps had marked progress.
//! - [`delete_transaction`] — synchronous tear-down callable from both API
//!   `handle_delete` and app-mode `kill_agent`. Waits for child exit (bounded)
//!   before removing the registry entry, drops the Telegram binding, removes
//!   configs + IPC port + emits event log.

use crate::agent::AgentRegistry;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Maximum time we wait for a child to actually transition to exited after
/// kill before force-removing the registry entry. Bounded so a stuck child
/// doesn't freeze the delete API; force-fallback is logged.
pub const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll interval while waiting for child exit. Short enough to be responsive,
/// long enough to avoid spinning the CPU under contention.
const CHILD_EXIT_POLL: Duration = Duration::from_millis(50);

type ChildArc = Arc<Mutex<Box<dyn portable_pty::Child + Send>>>;

/// Wait up to [`CHILD_EXIT_TIMEOUT`] for the child to transition to exited.
/// Returns `true` if the child exited within the budget; `false` if the
/// timeout fired (caller should force-remove the registry entry anyway and log
/// a warning).
pub fn wait_for_child_exit(child: &ChildArc) -> bool {
    let deadline = std::time::Instant::now() + CHILD_EXIT_TIMEOUT;
    loop {
        {
            let mut guard = child.lock();
            if let Ok(Some(_status)) = guard.try_wait() {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(CHILD_EXIT_POLL);
    }
}

/// Drop `name`'s binding on every registered channel. Multi-channel-safe
/// (t-20260703164240502572-50899-11): `active_channel()` returns `None`
/// once 2+ channels are registered, which used to make this a silent
/// no-op in a telegram+discord fleet. No-op when no channel is registered
/// (e.g. app mode without Telegram init).
fn drop_active_binding(name: &str) {
    crate::channel::drop_binding_on_all_channels(name);
}

/// Tear down all daemon-side state for an agent: kill child + tree, wait for
/// exit, remove registry entry, drop active-channel binding, optionally remove
/// configs entry, remove IPC port, emit event log.
///
/// Called by both API `handle_delete` and app-mode `kill_agent` so the two
/// paths cannot drift in cleanup completeness (Sprint 20 F3 was that drift).
///
/// When `skip_exit_wait` is `true`, the kill signal is sent but
/// [`wait_for_child_exit`] is skipped — the OS reaps the child
/// asynchronously. Used by `restart_instance` (#1366) where the caller
/// spawns a fresh instance immediately and the 5 s synchronous wait is
/// unnecessary overhead.
///
/// Returns `true` if the cleanup observed the child exiting cleanly; `false`
/// if [`CHILD_EXIT_TIMEOUT`] fired and we force-removed anyway. When
/// `skip_exit_wait` is `true`, always returns `true` (optimistic).
pub fn delete_transaction(
    home: &Path,
    name: &str,
    registry: &AgentRegistry,
    configs: Option<&Arc<Mutex<HashMap<String, super::AgentConfig>>>>,
    skip_exit_wait: bool,
) -> bool {
    delete_transaction_expecting(home, name, registry, configs, skip_exit_wait, None)
}

/// #2764 R8 (codex correction 3): exact-generation stop. When `expected_id`
/// is pinned, the registry/child are addressed by THAT id throughout — never
/// re-resolved by name (an A→B replacement between the caller's validation
/// and this lookup must not retarget the stop at B). Name-keyed cleanup
/// (binding/config/port) runs only after the exact child's terminal
/// disposition AND a fresh generation check confirming the name still maps to
/// the pinned generation.
pub fn delete_transaction_expecting(
    home: &Path,
    name: &str,
    registry: &AgentRegistry,
    configs: Option<&Arc<Mutex<HashMap<String, super::AgentConfig>>>>,
    skip_exit_wait: bool,
    expected_id: Option<crate::types::InstanceId>,
) -> bool {
    // Step 1: snapshot the child handle while still holding registry entry,
    // then release the registry lock before issuing the kill so concurrent
    // listings aren't blocked while we wait for exit.
    // Also set the `deleted` flag so the reaper thread (which may still be
    // alive) knows not to spawn a shell fallback.
    // #1441: registry is UUID-keyed. Resolve the authoritative id from
    // fleet.yaml (same source as inbox). `None` means no managed entry — the
    // remove/wait steps below all no-op, matching the prior "name absent"
    // behaviour.
    let instance_id = match expected_id {
        // Exact-generation authority: never fall back to a name re-resolve —
        // that is exactly the A→B retarget window being closed.
        Some(id) => Some(id),
        None => crate::fleet::resolve_uuid(home, name),
    };
    let child_arc = instance_id.and_then(|id| {
        let reg = crate::agent::lock_registry(registry);
        if let Some(h) = reg.get(&id) {
            h.deleted.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        reg.get(&id).map(|h| Arc::clone(&h.child))
    });

    // #2764 R8 test seam: force one wait-timeout verdict (a real
    // SIGKILL-immune child is not constructible deterministically).
    #[cfg(test)]
    let forced_timeout = exit_wait_test_seam::take();
    #[cfg(not(test))]
    let forced_timeout = false;

    let waited_ok = if let Some(child_arc) = child_arc {
        // Step 2: kill process tree first (covers backend child trees like
        // kiro-cli's bun/mcp/acp), then PTY-side kill as fallback.
        {
            let mut child = child_arc.lock();
            if let Some(pid) = child.process_id() {
                crate::process::kill_process_tree(pid);
            }
            let _ = child.kill();
        }
        if skip_exit_wait {
            // #1366: caller opted out of the synchronous wait. The kill
            // signal has been sent; the OS will reap the child in the
            // background. Proceed to registry removal immediately.
            true
        } else if forced_timeout {
            false
        } else {
            // Step 3: synchronous wait for actual exit (Sprint 20 F2 fix —
            // previously delete returned before the OS had reaped the PID,
            // exposing PID re-use + concurrent-spawn collision races).
            wait_for_child_exit(&child_arc)
        }
    } else {
        // No registry entry; nothing to wait on.
        true
    };

    // #2764 R8 (codex blocker 2 at d54a3ab4): a wait TIMEOUT is an UNKNOWN
    // stop disposition — RETAIN authority (registry handle, config entry, IPC
    // port) so a retry re-kills and re-waits against the SAME evidence. The
    // old force-removal made the retry resolve nothing and vacuously report
    // stopped:true while the child was potentially alive. The `deleted` flag
    // set above stays (reaper shell-fallback suppressed); only terminal proof
    // (a later wait observing exit) releases the stores.
    if !waited_ok {
        crate::event_log::log(
            home,
            "delete",
            name,
            "delete: child kill timeout — authority RETAINED for retry (no store removed)",
        );
        tracing::warn!(
            agent = %name,
            timeout_secs = CHILD_EXIT_TIMEOUT.as_secs(),
            "delete_transaction: child did not exit within timeout — registry/config/port retained for retry"
        );
        return false;
    }

    // Step 4: registry remove (after child exit confirmed or timeout).
    // #P1-2607-followup (reviewer4, PR #2620): must go through
    // `remove_and_unregister`, not a bare `reg.remove`, so the removed
    // handle's `write_actor` registration (lazy-spawn: no thread for a
    // never-written writer) doesn't leak a stale fd-reuse bookkeeping entry.
    if let Some(id) = instance_id {
        crate::agent::remove_and_unregister(registry, &id);
    }

    // #2764 R8 (codex correction 3): the pinned child is terminally stopped,
    // but the NAME-keyed stores below (binding/config/port) may already belong
    // to a same-name replacement generation. Fresh generation check: skip the
    // name-keyed cleanup when the name no longer maps to the pinned id.
    if let Some(exp) = expected_id {
        if crate::fleet::resolve_uuid(home, name) != Some(exp) {
            crate::event_log::log(
                home,
                "delete",
                name,
                "delete: pinned child stopped, but the name now maps to a different                  generation — name-keyed cleanup skipped",
            );
            return true;
        }
    }

    // Step 5: drop active-channel binding (Sprint 20.5 cross-validation finding).
    drop_active_binding(name);

    // Step 6: configs cleanup (None when called from app-mode `kill_agent`,
    // which doesn't track an AgentConfig map — app fleet.yaml is the authority).
    if let Some(cfgs) = configs {
        cfgs.lock().remove(name);
    }

    // Step 7: IPC port cleanup.
    crate::ipc::remove_port(&super::run_dir(home), name);

    // Step 8: event log (the timeout arm returned above with authority
    // retained — reaching here means the child exited or nothing was waited on).
    crate::event_log::log(home, "delete", name, "delete: child exited cleanly");

    true
}

/// #2764 R8 test seam: force the NEXT delete_transaction wait to report
/// timeout (thread-local one-shot; nextest = process-per-test).
#[cfg(test)]
pub(crate) mod exit_wait_test_seam {
    use std::cell::Cell;
    thread_local! {
        static FORCE: Cell<bool> = const { Cell::new(false) };
    }
    pub(crate) fn force_timeout_once() {
        FORCE.with(|c| c.set(true));
    }
    pub(crate) fn take() -> bool {
        FORCE.with(|c| c.replace(false))
    }
}

/// RAII rollback guard for `agent::spawn_agent`'s ordered mutations.
///
/// Constructed early in spawn. As each mutation completes (`mark_child_spawned`,
/// `mark_registered`), the guard records what to undo. On `commit()`, the
/// guard disarms and Drop is a no-op. On Drop while armed (caller returned
/// Err), the guard rolls back in reverse order:
/// - if registered: remove from registry + drop active-channel binding
/// - if child spawned: kill process tree + best-effort PTY kill
///
/// Rollback does **not** synchronously wait for the child to exit — spawn-side
/// rollback is best-effort cleanup before reporting Err to the caller, where
/// blocking the caller would compound the failure.
pub struct SpawnRollback<'r> {
    name: String,
    registry: &'r AgentRegistry,
    child: Option<ChildArc>,
    /// #1441: authoritative UUID key for registry removal on rollback. Set by
    /// `mark_registered` (the insert site already resolved it).
    instance_id: Option<crate::types::InstanceId>,
    armed: bool,
}

impl<'r> SpawnRollback<'r> {
    pub fn new(name: &str, registry: &'r AgentRegistry) -> Self {
        Self {
            name: name.to_string(),
            registry,
            child: None,
            instance_id: None,
            armed: true,
        }
    }

    /// Record that the OS child has been spawned and stash its handle so the
    /// guard can kill it on rollback.
    pub fn mark_child_spawned(&mut self, child: ChildArc) {
        self.child = Some(child);
    }

    /// Record that the registry insert has happened, capturing the UUID key
    /// so rollback can remove the exact entry.
    pub fn mark_registered(&mut self, instance_id: crate::types::InstanceId) {
        self.instance_id = Some(instance_id);
    }

    /// Disarm the rollback. Caller invokes this on the success path.
    pub fn commit(mut self) {
        self.armed = false;
    }
}

impl<'r> Drop for SpawnRollback<'r> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Rollback in reverse insertion order so observers can't see a
        // half-cleaned state with a child but no registry entry, etc.
        // #P1-2607-followup (reviewer4, PR #2620): `remove_and_unregister`,
        // not a bare `reg.remove` — see `delete_transaction`'s comment above.
        if let Some(id) = self.instance_id {
            crate::agent::remove_and_unregister(self.registry, &id);
            drop_active_binding(&self.name);
        }
        if let Some(child_arc) = self.child.take() {
            let mut child = child_arc.lock();
            if let Some(pid) = child.process_id() {
                crate::process::kill_process_tree(pid);
            }
            let _ = child.kill();
        }
        tracing::warn!(
            agent = %self.name,
            "spawn_agent partial failure — rolled back to clean state"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;

    fn empty_registry() -> AgentRegistry {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-lifecycle-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn spawn_rollback_committed_does_not_remove_registry_entry() {
        // Pre-seed registry with an arbitrary handle to verify commit() leaves
        // the entry intact even when the guard is armed mid-flight.
        let reg = empty_registry();
        let mut guard = SpawnRollback::new("alpha", &reg);
        guard.mark_registered(crate::types::InstanceId::default());
        guard.commit();
        // Drop fires here; armed=false so registry untouched.
        // We pre-seeded nothing, so the registry should still be empty
        // (mark_registered is a recorder, not a mutator).
        assert!(reg.lock().is_empty());
    }

    #[test]
    fn spawn_rollback_armed_drop_removes_registry_entry() {
        // Insert a placeholder handle so we can observe the rollback removing it.
        let reg = empty_registry();
        let placeholder = make_placeholder_handle("beta");
        // #1441: registry is UUID-keyed — insert under the handle's own id and
        // hand the same id to the rollback recorder so Drop removes it.
        let beta_id = placeholder.id;
        reg.lock().insert(beta_id, placeholder);
        {
            let mut guard = SpawnRollback::new("beta", &reg);
            guard.mark_registered(beta_id);
            // No commit — Drop fires armed → registry entry removed.
        }
        assert!(reg.lock().is_empty());
    }

    #[test]
    fn delete_transaction_no_registry_entry_is_no_op_returns_true() {
        let home = tmp_home("delete-noop");
        let reg = empty_registry();
        // No insert → delete still cleans configs/ipc/event-log; returns true
        // (no child to wait on, so "exit observed" is vacuously true).
        let observed_exit = delete_transaction(&home, "ghost", &reg, None, false);
        assert!(
            observed_exit,
            "missing registry entry → wait is vacuous true"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #P1-2607-followup (reviewer4, PR #2620 REJECTED finding): a real
    /// agent's `pty_writer` is registered with `write_actor` at spawn time,
    /// but `write_actor::register` is lazy-spawn -- no dedicated thread
    /// exists for a writer that's never had a write attempted through it,
    /// so the weak-reference backstop inside that thread never runs for it.
    /// `delete_transaction` must be the one to unregister it (not just
    /// remove it from the agent registry), or a never-written agent's
    /// teardown leaks a stale write_actor entry forever. Pins that this
    /// actually happens, end-to-end through the real `spawn_agent` path.
    ///
    /// Unix-only: `write_actor` itself is `#[cfg(unix)]` (no PTY-fd-based
    /// registration concept applies on Windows), so there's nothing to
    /// assert here on that platform.
    #[test]
    #[cfg(unix)]
    fn delete_transaction_unregisters_never_written_writer_2620() {
        let home = tmp_home("delete-unreg");
        let reg = empty_registry();
        let cfg = crate::agent::SpawnConfig {
            name: "never-written",
            backend_command: "cat",
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        let id = crate::agent::spawn_agent(&cfg, &reg).expect("spawn");

        let pty_writer = {
            let guard = crate::agent::lock_registry(&reg);
            guard.get(&id).expect("just spawned").pty_writer.clone()
        };
        assert!(
            crate::agent::write_actor_is_registered(&pty_writer),
            "spawn_agent must register the writer with write_actor"
        );

        // `delete_transaction` resolves its registry id via
        // `fleet::resolve_uuid(home, name)`, which reads `home`'s
        // fleet.yaml -- write the minimal `name -> id` mapping `spawn_agent`
        // itself would have persisted had it been given this `home`.
        std::fs::write(
            home.join("fleet.yaml"),
            format!("instances:\n  never-written:\n    id: \"{}\"\n", id.full()),
        )
        .expect("write fleet.yaml");

        // Deliberately never write to it -- exercises the lazy-spawn gap
        // directly (no thread ever gets started for this writer).
        delete_transaction(&home, "never-written", &reg, None, true);

        assert!(
            !crate::agent::write_actor_is_registered(&pty_writer),
            "delete_transaction must unregister a never-written writer's write_actor \
             registration, not just remove it from the agent registry"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Build a minimally-valid AgentHandle whose `child` is a real (already-exited)
    /// process so test assertions about cleanup don't depend on a live PTY.
    fn make_placeholder_handle(name: &str) -> crate::agent::AgentHandle {
        use portable_pty::{native_pty_system, PtySize};
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = portable_pty::CommandBuilder::new("true");
        cmd.cwd(std::env::temp_dir());
        let child = pair.slave.spawn_command(cmd).expect("spawn 'true'");
        drop(pair.slave);
        let pty_writer: crate::agent::PtyWriter =
            Arc::new(Mutex::new(pair.master.take_writer().expect("take_writer")));
        let pty_master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>> =
            Arc::new(Mutex::new(pair.master));
        let core = Arc::new(crate::sync_audit::CoreMutex::new(crate::agent::AgentCore {
            vterm: crate::vterm::VTerm::with_pty_writer(80, 24, Arc::clone(&pty_writer)),
            subscribers: Vec::new(),
            state: crate::state::StateTracker::new(None),
            health: crate::health::HealthTracker::new(),
            api_activity: crate::agent::ApiActivity::default(),
            observed_status: None,
        }));
        crate::agent::AgentHandle {
            id: crate::types::InstanceId::default(),
            name: name.to_string().into(),
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
            deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// #2764 R8 (codex blocker 2 at d54a3ab4): a wait TIMEOUT is an UNKNOWN
    /// disposition — the registry handle, config entry and IPC port are
    /// RETAINED so a retry re-kills and re-waits against the SAME evidence.
    /// Pre-R8 the timeout force-removed everything, making the retry resolve
    /// nothing and vacuously report stopped=true while the child could live.
    #[test]
    fn delete_timeout_retains_authority_then_retry_succeeds_2764_r8() {
        let home = tmp_home("delete-timeout-retry");
        let reg = empty_registry();
        let handle = make_placeholder_handle("stuck");
        let id = handle.id;
        reg.lock().insert(id, handle);
        std::fs::write(
            home.join("fleet.yaml"),
            format!("instances:\n  stuck:\n    id: \"{}\"\n", id.full()),
        )
        .expect("write fleet.yaml");
        let rdir = crate::daemon::run_dir(&home);
        std::fs::create_dir_all(&rdir).expect("mk run dir");
        let port = crate::ipc::port_path(&rdir, "stuck");
        std::fs::write(&port, "1").expect("seed port file");

        exit_wait_test_seam::force_timeout_once();
        let first = delete_transaction(&home, "stuck", &reg, None, false);
        assert!(!first, "forced timeout must report stopped=false");
        assert!(
            reg.lock().contains_key(&id),
            "registry handle must be RETAINED on timeout (retry authority)"
        );
        assert!(port.exists(), "IPC port must be retained on timeout");

        // S1: the PINNED retry targets the SAME retained child handle.
        let second = delete_transaction_expecting(&home, "stuck", &reg, None, false, Some(id));
        assert!(
            second,
            "pinned retry with the retained handle must observe the exit"
        );
        assert!(
            !reg.lock().contains_key(&id),
            "retry removes the registry entry after terminal proof"
        );
        assert!(
            !port.exists(),
            "retry removes the port after terminal proof"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2764 R8 (codex correction 3): a PINNED stop addresses the registry by
    /// the EXACT expected id — an A→B same-name replacement between the
    /// caller's validation and the stop lookup must leave B's child, config
    /// and port untouched. A itself is terminally absent (not registered), so
    /// the stop verdict is true, and the fresh generation check skips ALL
    /// name-keyed cleanup because the name now belongs to B.
    #[test]
    fn pinned_stop_never_retargets_replacement_generation_2764_r8() {
        let home = tmp_home("pinned-no-retarget");
        let reg = empty_registry();
        // B is the live replacement: registered, fleet-mapped, port published.
        let b_handle = make_placeholder_handle("swapped");
        let b_id = b_handle.id;
        reg.lock().insert(b_id, b_handle);
        std::fs::write(
            home.join("fleet.yaml"),
            format!("instances:\n  swapped:\n    id: \"{}\"\n", b_id.full()),
        )
        .expect("write fleet.yaml");
        let rdir = crate::daemon::run_dir(&home);
        std::fs::create_dir_all(&rdir).expect("mk run dir");
        let port = crate::ipc::port_path(&rdir, "swapped");
        std::fs::write(&port, "1").expect("seed port");

        // A is the stale pinned generation (never registered here).
        let a_id = crate::types::InstanceId::new();
        let verdict = delete_transaction_expecting(&home, "swapped", &reg, None, false, Some(a_id));

        assert!(
            verdict,
            "A is terminally absent from the registry — its stop verdict is true"
        );
        assert!(
            reg.lock().contains_key(&b_id),
            "the replacement B's child handle must be untouched"
        );
        assert!(
            port.exists(),
            "the replacement B's port (name-keyed) must be untouched"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn delete_transaction_with_exited_child_returns_true() {
        let home = tmp_home("delete-exited");
        let reg = empty_registry();
        let handle = make_placeholder_handle("gamma");
        // #1441: delete_transaction resolves the name via fleet.yaml; seed the
        // entry with the handle's own id so the resolved key hits this entry.
        let gamma_id = handle.id;
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!("instances:\n  gamma:\n    id: {}\n", gamma_id.full()),
        )
        .ok();
        reg.lock().insert(gamma_id, handle);
        // `true` exits immediately, so wait_for_child_exit should observe
        // the exit on the first try_wait.
        let observed_exit = delete_transaction(&home, "gamma", &reg, None, false);
        assert!(
            observed_exit,
            "exited child must be observed within timeout"
        );
        assert!(
            reg.lock().is_empty(),
            "registry entry must be removed after delete"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
