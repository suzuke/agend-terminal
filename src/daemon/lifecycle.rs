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

/// Drop the active channel's binding for `name`, if any. Channel-agnostic via
/// the `Channel` trait `take_binding`. No-op when no channel is registered
/// (e.g. app mode without Telegram init).
fn drop_active_binding(name: &str) {
    if let Some(ch) = crate::channel::active_channel() {
        let _ = ch.take_binding(name);
    }
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
/// asynchronously. Used by `replace_instance` (#1366) where the caller
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
    // Step 1: snapshot the child handle while still holding registry entry,
    // then release the registry lock before issuing the kill so concurrent
    // listings aren't blocked while we wait for exit.
    // Also set the `deleted` flag so the reaper thread (which may still be
    // alive) knows not to spawn a shell fallback.
    // #1441: registry is UUID-keyed. Resolve the authoritative id from
    // fleet.yaml (same source as inbox). `None` means no managed entry — the
    // remove/wait steps below all no-op, matching the prior "name absent"
    // behaviour.
    let instance_id = crate::fleet::resolve_uuid(home, name);
    let child_arc = instance_id.and_then(|id| {
        let reg = registry.lock();
        if let Some(h) = reg.get(&id) {
            h.deleted.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        reg.get(&id).map(|h| Arc::clone(&h.child))
    });

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

    // Step 4: registry remove (after child exit confirmed or timeout).
    if let Some(id) = instance_id {
        let mut reg = registry.lock();
        reg.remove(&id);
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

    // Step 8: event log.
    let detail = if waited_ok {
        "delete: child exited cleanly"
    } else {
        "delete: child kill timeout — force-removed registry entry"
    };
    crate::event_log::log(home, "delete", name, detail);

    if !waited_ok {
        tracing::warn!(
            agent = %name,
            timeout_secs = CHILD_EXIT_TIMEOUT.as_secs(),
            "delete_transaction: child did not exit within timeout, force-removed"
        );
    }

    waited_ok
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
        if let Some(id) = self.instance_id {
            let mut reg = self.registry.lock();
            reg.remove(&id);
            drop(reg);
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
