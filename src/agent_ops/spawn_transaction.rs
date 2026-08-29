//! Shared spawn transaction primitives for runtime-created agents.

/// The submit key a spawned agent gets, derived from its effective backend.
///
/// One source for both places that need it: the live handle `spawn_one` builds,
/// and the `AgentConfig` the spawn transaction records. Deriving it twice is how
/// the two drift apart, and a config that disagrees with the running agent is
/// worse than no config at all — crash respawn would replay the wrong key.
pub(crate) fn preset_submit_key(backend: Option<&crate::backend::Backend>) -> &'static str {
    backend.map_or("\r", |b| b.preset().submit_key)
}

/// The per-`(home, name)` spawn lane.
///
/// Same key shape as the DELETING registry (`crate::agent::deleting`) and the
/// same "short global lock, then work on the `Arc`" acquisition as the write
/// actors' `WRITERS` map. Entries are not reaped: the map grows only to the
/// number of DISTINCT instance names this process has ever spawned, which is the
/// fleet's own order of magnitude, and reaping would need a second global
/// acquisition on every release to stay race-free.
type SpawnLane = std::sync::Arc<parking_lot::Mutex<()>>;
type SpawnLanes =
    parking_lot::Mutex<std::collections::HashMap<(std::path::PathBuf, String), SpawnLane>>;

fn spawn_lane(home: &std::path::Path, name: &str) -> SpawnLane {
    static LANES: std::sync::OnceLock<SpawnLanes> = std::sync::OnceLock::new();
    let lanes = LANES.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    let key = (
        dunce::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()),
        name.to_string(),
    );
    let mut map = lanes.lock();
    SpawnLane::clone(map.entry(key).or_default())
}

/// Test seam: do these two home spellings resolve to the SAME lane?
///
/// Pointer identity of the `Arc`, so the answer comes from the lane itself rather
/// than from restating the key-building code.
#[cfg(all(test, unix))]
pub(crate) fn lanes_are_the_same(a: &std::path::Path, b: &std::path::Path, name: &str) -> bool {
    SpawnLane::ptr_eq(&spawn_lane(a, name), &spawn_lane(b, name))
}

/// Record an agent's resolved configuration, then spawn it — the transaction
/// every post-boot spawn surface goes through.
///
/// #3417: `ctx.configs` used to be written only by the BOOT path, so every
/// runtime-created instance was absent from the map that the snapshot writer and
/// `crash_respawn` read. The snapshot degraded to a plausible `args: []`; crash
/// respawn simply refused to respawn — not a reporting nicety but a live gap.
///
/// The whole transaction runs inside a per-`(home, name)` lane, because the
/// alternatives do not survive a concurrent same-name spawn. Nothing else
/// serializes those: `spawn_instance`'s duplicate check reads the registry and
/// the actual registration happens later. Rolling back on a value comparison
/// cannot tell two identical configs apart, and consulting the registry after a
/// failure is still check-then-act — the winner can register between the check
/// and the restore, leaving a live agent with a stale or missing config, which is
/// the exact defect this work removes.
///
/// Inside the lane the order is load-bearing in both directions:
///
/// * The insert happens BEFORE the spawn, because a child can exit — and the
///   crash path can look this config up — before the spawn call returns.
/// * A failure restores the PREVIOUS value rather than deleting, so a failed
///   restart cannot strip the config its predecessor was described by.
/// * Success retains it. A child that starts and then exits immediately is not a
///   failed spawn; it is precisely the case crash respawn exists for.
///
/// Locks: the per-name lane guard is held across `spawn` and its file work; the
/// configs and registry locks are not held across `spawn`, and disk I/O does not
/// occur under either of those locks. The lane is taken only here, at the
/// outermost layer of a spawn, so nothing that `spawn` itself locks can be waiting
/// on it; and no surface enters it twice for one name on one thread (restart
/// deletes outside the lane, deployment and team spawn distinct names in
/// sequence).
pub(crate) fn spawn_one_recording_config(
    home: &std::path::Path,
    configs: &crate::api::ConfigRegistry,
    name: &str,
    config: crate::daemon::AgentConfig,
    spawn: impl FnOnce() -> anyhow::Result<crate::backend::SpawnMode>,
) -> anyhow::Result<crate::backend::SpawnMode> {
    let lane = spawn_lane(home, name);
    let _lane = lane.lock();
    let previous = configs.lock().insert(name.to_string(), config);
    let mut rollback = SpawnRollback {
        configs,
        name,
        previous,
        armed: true,
    };
    let outcome = spawn();
    if outcome.is_ok() {
        rollback.armed = false;
    }
    outcome
}

/// Undoes the transaction's insert unless the spawn committed.
///
/// A guard rather than an `Err` arm so the invariant survives a PANICKING spawn:
/// unwinding runs this, and a panic that left the attempted config behind would
/// describe an agent that may not exist.
struct SpawnRollback<'a> {
    configs: &'a crate::api::ConfigRegistry,
    name: &'a str,
    previous: Option<crate::daemon::AgentConfig>,
    armed: bool,
}

impl Drop for SpawnRollback<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut cfgs = self.configs.lock();
        // DELETE (`lifecycle::delete_transaction` step 6) and clean exit
        // (`handle_clean_exit`) are the removal authority, and both remove under
        // THIS lock. If our entry is gone, one of them retired the instance while
        // we were spawning, and restoring `previous` would resurrect a config for
        // something that has been deleted — handing crash respawn a dead agent.
        // Deletion therefore wins and the rollback stands down.
        //
        // Presence is a sound ownership token here, and only here: the lane
        // guarantees no other SPAWN can have written this key, so a present entry
        // is still ours. The test and the write share this one critical section,
        // so this is a compare-and-act, not the post-failure check-then-act that
        // the lane exists to remove.
        if !cfgs.contains_key(self.name) {
            return;
        }
        match self.previous.take() {
            Some(previous) => {
                cfgs.insert(self.name.to_string(), previous);
            }
            None => {
                cfgs.remove(self.name);
            }
        }
    }
}
