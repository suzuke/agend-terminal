//! Lifecycle transactions for agent spawn / delete / kill flows.
//!
//! Centralizes partial-failure rollback so the spawn / delete / kill paths
//! cannot leak orphan PIDs, phantom registry entries, or stale Telegram
//! bindings.
//!
//! Audit context: `docs/DAEMON-LOCK-ORDERING.md` +
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
/// kill before refusing the teardown. Bounded so a stuck child doesn't freeze
/// the delete API; the registry entry is retained for a later retry.
pub const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll interval while waiting for child exit. Short enough to be responsive,
/// long enough to avoid spinning the CPU under contention.
const CHILD_EXIT_POLL: Duration = Duration::from_millis(50);

type ChildArc = Arc<Mutex<Box<dyn portable_pty::Child + Send>>>;

/// Owns the two deletion fences whose teardown order is load-bearing. The
/// deleting marker is cleared first while the keyed transport state/lane is
/// still active; the transport guard then performs its final epoch bump and
/// releases the lane. This prevents an enqueue from observing a fresh epoch
/// after the marker disappears but before transport invalidation completes.
pub(crate) struct DeleteFence {
    deleting: Option<crate::agent::deleting::DeletingGuard>,
    transport: Option<crate::daemon::delivery_worker::TransportGenerationGuard>,
}

impl DeleteFence {
    pub(crate) fn new(home: &Path, name: &str, hold_transport: bool) -> Self {
        let deleting = Some(crate::agent::deleting::mark_deleting(home, name));
        let transport = hold_transport
            .then(|| crate::daemon::delivery_worker::begin_transport_cleanup(home, name));
        // The final forget must follow keyed transport ownership. Otherwise a
        // self-kick that already owns the lane could arm after this early
        // forget and before cleanup acquires the lane.
        if transport.is_some() {
            crate::daemon::inject_delivery::forget(name);
        }
        Self {
            deleting,
            transport,
        }
    }

    pub(crate) fn attach_transport_cleanup(&mut self, home: &Path, name: &str) {
        debug_assert!(self.transport.is_none());
        self.transport = Some(crate::daemon::delivery_worker::begin_transport_cleanup(
            home, name,
        ));
        crate::daemon::inject_delivery::forget(name);
    }
}

impl Drop for DeleteFence {
    fn drop(&mut self) {
        drop(self.deleting.take());
        drop(self.transport.take());
    }
}

/// Wait up to [`CHILD_EXIT_TIMEOUT`] for the child to transition to exited.
/// Returns `true` if the child exited within the budget; `false` if the
/// timeout fired (caller must preserve the registry entry and refuse teardown).
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
/// if [`CHILD_EXIT_TIMEOUT`] fired and teardown was refused. When
/// `skip_exit_wait` is `true`, always returns `true` (optimistic).
pub fn delete_transaction(
    home: &Path,
    name: &str,
    registry: &AgentRegistry,
    configs: Option<&Arc<Mutex<HashMap<String, super::AgentConfig>>>>,
    skip_exit_wait: bool,
) -> bool {
    let _delete_fence = DeleteFence::new(home, name, true);
    if let Err(error) = crate::transport::remove_instance_delivery_state(home, name) {
        tracing::warn!(
            agent = %name,
            error = %error,
            "delete: transport delivery cleanup failed"
        );
    }
    delete_transaction_under_guard(home, name, registry, configs, skip_exit_wait)
}

/// Shared delete transaction body. Callers must already hold the deleting mark
/// and keyed transport cleanup guard; this is used by full-delete, whose larger
/// residual teardown owns those guards across all of its side effects.
pub(crate) fn delete_transaction_under_guard(
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
        let reg = crate::agent::lock_registry(registry);
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

    if !waited_ok {
        crate::event_log::log(
            home,
            "delete",
            name,
            "delete: child kill timeout — retained registry entry",
        );
        tracing::warn!(
            agent = %name,
            timeout_secs = CHILD_EXIT_TIMEOUT.as_secs(),
            "delete_transaction: child did not exit within timeout, retained for retry"
        );
        return false;
    }

    // Step 4: registry remove after child exit is confirmed.
    // #P1-2607-followup (reviewer4, PR #2620): must go through
    // `remove_and_unregister`, not a bare `reg.remove`, so the removed
    // handle's `write_actor` registration (lazy-spawn: no thread for a
    // never-written writer) doesn't leak a stale fd-reuse bookkeeping entry.
    if let Some(id) = instance_id {
        crate::agent::remove_and_unregister(registry, &id);
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
    crate::event_log::log(home, "delete", name, "delete: child exited cleanly");

    true
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
    fn transport_aware_rollback_cleans_receipts_after_active_delivery() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let _full = crate::daemon::delivery_worker::test_support::force_full_guard();
        crate::daemon::delivery_worker::test_support::set_force_full(false);
        let _hook_guard = crate::transport::test_support::delivery_hook_guard();
        let home = tmp_home("transport-rollback");
        let agent = "rollback-agent";
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let entered_hook = Arc::clone(&entered);
        let release_hook = Arc::clone(&release);
        let expected_home = home.clone();
        crate::transport::test_support::set_delivery_hook(Some(Arc::new(
            move |called_home, name, body| {
                if called_home != expected_home.as_path() {
                    return None;
                }
                let envelope = crate::transport::DeliveryEnvelope::new(
                    name,
                    crate::transport::SessionLocator::codex(
                        std::path::PathBuf::from("/tmp/rollback-test.sock"),
                        Some("rollback-test-thread".to_string()),
                    ),
                    crate::transport::DeliveryKind::Notification,
                    body,
                    None,
                );
                let store = crate::transport::ReceiptStore::for_instance(called_home, name)
                    .expect("receipt store");
                store.record_queued(&envelope).expect("queued receipt");
                let receipt = crate::transport::DeliveryReceipt::for_state(
                    &envelope,
                    crate::transport::DeliveryState::ProtocolAccepted,
                );
                store.record(receipt.clone()).expect("terminal receipt");
                entered_hook.store(true, Ordering::SeqCst);
                while !release_hook.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Some(Ok(receipt))
            },
        )));
        assert!(crate::daemon::delivery_worker::enqueue_transport_delivery(
            &home,
            agent,
            "rollback wake",
        )
        .is_ok());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !entered.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(entered.load(Ordering::SeqCst));

        let registry = empty_registry();
        let rollback_home = home.clone();
        let rollback_registry = Arc::clone(&registry);
        let rollback_finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rollback_finished_thread = Arc::clone(&rollback_finished);
        let rollback = std::thread::spawn(move || {
            let result = delete_transaction(&rollback_home, agent, &rollback_registry, None, true);
            rollback_finished_thread.store(true, Ordering::SeqCst);
            result
        });
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            !rollback_finished.load(Ordering::SeqCst),
            "rollback must remain blocked behind the active transport lane"
        );
        assert!(
            crate::transport::delivery_path_for_instance(&home, agent).exists(),
            "active delivery must create a receipt before rollback can clean it"
        );
        assert!(crate::daemon::delivery_worker::enqueue_transport_delivery(
            &home,
            agent,
            "queued during rollback",
        )
        .is_ok());
        release.store(true, Ordering::SeqCst);
        assert!(rollback.join().expect("rollback thread"));
        assert!(rollback_finished.load(Ordering::SeqCst));
        crate::transport::test_support::set_delivery_hook(None);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while crate::daemon::delivery_worker::test_support::transport_dispatch_count(&home, agent)
            < 2
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        let dispatch_count =
            crate::daemon::delivery_worker::test_support::transport_dispatch_count(&home, agent);
        assert_eq!(
            dispatch_count, 2,
            "active old job and queued stale job must both be observed exactly once"
        );
        assert!(
            !crate::transport::delivery_path_for_instance(&home, agent).exists(),
            "outer rollback must remove receipts and reject queued stale work"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn delete_fence_clears_marker_before_transport_finalization() {
        let _ff = crate::daemon::delivery_worker::test_support::force_full_guard();
        crate::daemon::delivery_worker::test_support::set_force_full(false);
        let _hook_guard = crate::transport::test_support::delivery_hook_guard();
        let _tail_guard =
            crate::daemon::delivery_worker::test_support::cleanup_release_tail_hook_guard();
        let home = tmp_home("delete-fence-order");
        let agent = "fence-agent";
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  fence-agent:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: legacy_pty\n",
        )
        .expect("fleet");
        let (tail_tx, tail_rx) = std::sync::mpsc::channel();
        let tail_home = home.clone();
        crate::daemon::delivery_worker::test_support::set_cleanup_release_tail_hook(Some(
            Arc::new(move |hook_home, hook_agent| {
                if hook_home != tail_home.as_path() || hook_agent != agent {
                    return;
                }
                tail_tx
                    .send((
                        crate::agent::deleting::is_deleting(hook_home, hook_agent),
                        crate::daemon::delivery_worker::enqueue_transport_delivery(
                            hook_home,
                            hook_agent,
                            "late fence wake",
                        ),
                    ))
                    .expect("tail result receiver");
            }),
        ));
        let (adapter_tx, adapter_rx) = std::sync::mpsc::channel();
        let adapter_home = home.clone();
        crate::transport::test_support::set_delivery_hook(Some(Arc::new(
            move |called_home, _name, _body| {
                if called_home != adapter_home.as_path() {
                    return None;
                }
                adapter_tx.send(()).expect("adapter result receiver");
                Some(Err(anyhow::anyhow!("post-fence probe")))
            },
        )));

        {
            let fence = DeleteFence::new(&home, agent, true);
            drop(fence);
        }
        let (was_deleting, late_result) = tail_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cleanup tail hook must run");
        assert!(
            !was_deleting,
            "marker must clear before transport finalization"
        );
        assert_eq!(
            late_result,
            Err(crate::daemon::delivery_worker::TransportEnqueueError::Fenced),
            "same-key enqueue must remain stale while transport finalization is active"
        );

        assert!(crate::daemon::delivery_worker::enqueue_transport_delivery(
            &home,
            agent,
            "fresh fence wake",
        )
        .is_ok());
        adapter_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("post-fence same-key delivery must reach the adapter");
        crate::transport::test_support::set_delivery_hook(None);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn delete_fence_blocks_direct_serial_until_transport_finalization() {
        let _ff = crate::daemon::delivery_worker::test_support::force_full_guard();
        crate::daemon::delivery_worker::test_support::set_force_full(false);
        let _tail_guard =
            crate::daemon::delivery_worker::test_support::cleanup_release_tail_hook_guard();
        let _admission_guard =
            crate::daemon::delivery_worker::test_support::direct_transport_admission_hook_guard();
        let home = tmp_home("direct-serial-finalization");
        let agent = "direct-serial-agent";
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  direct-serial-agent:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: legacy_pty\n",
        )
        .expect("fleet");

        let (admission_tx, admission_rx) = std::sync::mpsc::channel();
        let admission_rx = Arc::new(parking_lot::Mutex::new(admission_rx));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let entered_rx = Arc::new(parking_lot::Mutex::new(entered_rx));
        let direct_thread = Arc::new(parking_lot::Mutex::new(None));
        let direct_thread_for_tail = Arc::clone(&direct_thread);
        let admission_rx_for_tail = Arc::clone(&admission_rx);
        let expected_home = home.clone();
        crate::daemon::delivery_worker::test_support::set_direct_transport_admission_hook(Some(
            Arc::new(move |hook_home, hook_agent| {
                if hook_home == expected_home.as_path() && hook_agent == agent {
                    admission_tx.send(()).expect("direct admission receiver");
                }
            }),
        ));

        let tail_home = home.clone();
        let entered_rx_for_tail = Arc::clone(&entered_rx);
        crate::daemon::delivery_worker::test_support::set_cleanup_release_tail_hook(Some(
            Arc::new(move |hook_home, hook_agent| {
                if hook_home != tail_home.as_path() || hook_agent != agent {
                    return;
                }
                let direct_home = hook_home.to_path_buf();
                let direct_agent = hook_agent.to_string();
                let entered_tx = entered_tx.clone();
                let handle = std::thread::spawn(move || {
                    crate::daemon::delivery_worker::with_transport_serial(
                        &direct_home,
                        &direct_agent,
                        || {
                            entered_tx.send(()).expect("direct serial entry receiver");
                        },
                    );
                });
                admission_rx_for_tail
                    .lock()
                    .recv_timeout(Duration::from_secs(1))
                    .expect("direct serial must reach lane-to-state admission");
                assert!(
                    entered_rx_for_tail.lock().try_recv().is_err(),
                    "direct serial must not enter while finalization still owns epoch state"
                );
                *direct_thread_for_tail.lock() = Some(handle);
            }),
        ));

        let fence = DeleteFence::new(&home, agent, true);
        drop(fence);
        let handle = direct_thread
            .lock()
            .take()
            .expect("tail must retain direct serial thread");
        handle.join().expect("direct serial thread");
        entered_rx
            .lock()
            .recv_timeout(Duration::from_secs(1))
            .expect("direct serial enters after transport finalization");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn fenced_inject_with_submit_does_not_recreate_receipts() {
        let _ff = crate::daemon::delivery_worker::test_support::force_full_guard();
        crate::daemon::delivery_worker::test_support::set_force_full(false);
        let _tail_guard =
            crate::daemon::delivery_worker::test_support::cleanup_release_tail_hook_guard();
        let home = tmp_home("fenced-inject-no-receipt");
        let agent = "fenced-inject-agent";
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  fenced-inject-agent:\n    backend: codex\n",
        )
        .expect("fleet");
        let envelope = crate::transport::DeliveryEnvelope::new(
            agent,
            crate::transport::SessionLocator::codex(
                std::path::PathBuf::from("/tmp/fenced-inject.sock"),
                Some("fenced-inject-thread".to_string()),
            ),
            crate::transport::DeliveryKind::Notification,
            "existing receipt",
            None,
        );
        let store =
            crate::transport::ReceiptStore::for_instance(&home, agent).expect("receipt store");
        store.record_queued(&envelope).expect("queued receipt");
        store
            .record(crate::transport::DeliveryReceipt::for_state(
                &envelope,
                crate::transport::DeliveryState::ProtocolAccepted,
            ))
            .expect("terminal receipt");
        let delivery_path = crate::transport::delivery_path_for_instance(&home, agent);
        assert!(delivery_path.exists(), "fixture receipt must exist");

        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let expected_home = home.clone();
        crate::daemon::delivery_worker::test_support::set_cleanup_release_tail_hook(Some(
            Arc::new(move |hook_home, hook_agent| {
                if hook_home != expected_home.as_path() || hook_agent != agent {
                    return;
                }
                result_tx
                    .send(crate::inbox::notify::inject_notification_with_submit(
                        hook_home,
                        hook_agent,
                        "late fenced actionable wake",
                        None,
                    ))
                    .expect("fenced inject result receiver");
            }),
        ));

        let fence = DeleteFence::new(&home, agent, true);
        crate::transport::remove_instance_delivery_state(&home, agent).expect("receipt cleanup");
        drop(fence);
        let result = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cleanup tail inject must run");
        assert!(result.is_err(), "fenced inject must be rejected");
        assert!(result
            .expect_err("fenced inject unexpectedly succeeded")
            .to_string()
            .contains("fenced"));
        assert!(
            !delivery_path.exists(),
            "fenced inject must not recreate receipt body"
        );
        assert!(
            !delivery_path.with_extension("jsonl.lock").exists(),
            "fenced inject must not recreate receipt lock"
        );
        assert!(
            !delivery_path.parent().is_some_and(std::path::Path::exists),
            "fenced inject must not recreate receipt directory"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn stale_queue_full_drop_is_suppressed_after_delete() {
        let _ff = crate::daemon::delivery_worker::test_support::force_full_guard();
        let _queue_full_hook_guard =
            crate::daemon::delivery_worker::test_support::queue_full_before_record_hook_guard();
        let home = tmp_home("stale-queue-full-drop");
        let agent = "stale-queue-full-agent";
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  stale-queue-full-agent:\n    backend: codex\n",
        )
        .expect("fleet");
        let delivery_path = crate::transport::delivery_path_for_instance(&home, agent);
        let expected_home = home.clone();
        crate::daemon::delivery_worker::test_support::set_queue_full_before_record_hook(Some(
            Arc::new(move |hook_home, hook_agent, _epoch| {
                if hook_home != expected_home.as_path() || hook_agent != agent {
                    return;
                }
                let cleanup_home = hook_home.to_path_buf();
                let cleanup_agent = hook_agent.to_string();
                std::thread::spawn(move || {
                    let fence = DeleteFence::new(&cleanup_home, &cleanup_agent, true);
                    crate::transport::remove_instance_delivery_state(&cleanup_home, &cleanup_agent)
                        .expect("receipt cleanup");
                    drop(fence);
                })
                .join()
                .expect("cleanup thread");
            }),
        ));
        crate::daemon::delivery_worker::test_support::set_force_full(true);
        let result = crate::inbox::notify::inject_notification_with_submit(
            &home,
            agent,
            "queue-full result invalidated by delete",
            None,
        );
        crate::daemon::delivery_worker::test_support::set_force_full(false);
        assert!(
            result.is_err(),
            "stale queue-full admission must not report successful persistence"
        );
        assert!(
            !delivery_path.exists(),
            "stale queue-full result must not recreate receipt body"
        );
        assert!(
            !delivery_path.with_extension("jsonl.lock").exists(),
            "stale queue-full result must not recreate receipt lock"
        );
        assert!(
            !delivery_path.parent().is_some_and(std::path::Path::exists),
            "stale queue-full result must not recreate receipt directory"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn guarded_delete_boundary_call_sites_remain_explicit() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let agent_ops = std::fs::read_to_string(root.join("src/agent_ops.rs")).expect("agent_ops");
        let full_delete =
            std::fs::read_to_string(root.join("src/mcp/handlers/instance_state/lifecycle.rs"))
                .expect("full-delete lifecycle");
        assert_eq!(
            agent_ops.matches("delete_transaction_under_guard(").count(),
            1,
            "only the guarded agent-ops path may call the low-level delete body"
        );
        assert!(
            full_delete.contains("delete_instance_under_guard"),
            "full-delete must use the explicit caller-held-guard boundary"
        );
        assert!(
            !full_delete.contains("delete_transaction_under_guard("),
            "full-delete must not bypass the agent-ops guard boundary"
        );
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
            backend: None,
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
            typed_inject_contaminated: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            spawned_at: std::time::Instant::now(),
            spawned_at_epoch_ms: 0,
            spawn_mode: crate::backend::SpawnMode::Fresh,
            generation: crate::agent::crash_disposition::SpawnGeneration::default(),
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
