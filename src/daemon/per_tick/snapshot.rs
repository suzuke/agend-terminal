//! Snapshot rotation: serialize fleet state to `<home>/snapshot.json`,
//! but skip the disk write when the serialized form is byte-identical to
//! the previous tick's. Extracted verbatim from `src/daemon/mod.rs:644-680`
//! (pre-#694 BLOCK 1) — same lock-acquisition order, same skip semantics.

use super::{PerTickHandler, TickContext};
use crate::agent;
use parking_lot::Mutex;

/// Owns the `last_snapshot_json` string that used to be a loop-local in
/// `run_core`. `Mutex` (not `RefCell`) because `PerTickHandler: Send + Sync`.
pub(crate) struct SnapshotRotationHandler {
    last_snapshot_json: Mutex<String>,
}

impl SnapshotRotationHandler {
    pub(crate) fn new() -> Self {
        Self {
            last_snapshot_json: Mutex::new(String::new()),
        }
    }
}

impl PerTickHandler for SnapshotRotationHandler {
    fn name(&self) -> &'static str {
        "snapshot_rotation"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        // Lock-acquisition order (docs/DAEMON-LOCK-ORDERING.md Rule 1,
        // top-down): registry (L0) → configs (L0) → per-agent core (L1,
        // briefly inside the map closure). Identical to the pre-extraction
        // inline block.
        // #941: holder-tracking wrapper for thread-dump observability.
        let reg = agent::lock_registry_tracked(ctx.registry, "snapshot_rotation");
        let cfgs = ctx.configs.lock();
        let snapshots: Vec<_> = reg
            .values()
            .map(|handle| {
                let (agent_state, health_state) = {
                    let c = handle.core.lock();
                    (
                        c.state.get_state().display_name().to_string(),
                        c.health.state.display_name().to_string(),
                    )
                };
                let cfg = cfgs.get(handle.name.as_str());
                crate::snapshot::AgentSnapshot {
                    name: handle.name.to_string(),
                    backend_command: handle.backend_command.clone(),
                    args: cfg.map(|c| c.args.clone()).unwrap_or_default(),
                    working_dir: cfg
                        .and_then(|c| c.working_dir.as_ref())
                        .map(|p| p.display().to_string()),
                    submit_key: handle.submit_key.clone(),
                    health_state,
                    agent_state,
                }
            })
            .collect();
        drop(cfgs);
        drop(reg);

        let new_json = serde_json::to_string(&snapshots).unwrap_or_default();
        let mut last = self.last_snapshot_json.lock();
        if *last != new_json {
            crate::snapshot::save(ctx.home, &snapshots);
            *last = new_json;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::agent::{AgentRegistry, ExternalRegistry};
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-snap-handler-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Empty registry round-trip — second `run` with no state change must
    /// not rewrite the file (the dedup guard is the whole point of the
    /// pre-extraction `if last_snapshot_json != new_json` check).
    #[test]
    fn snapshot_handler_dedupes_unchanged_state() {
        let home = tmp_home("dedupe");
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        let h = SnapshotRotationHandler::new();
        h.run(&ctx);
        let snapshot_path = home.join("snapshot.json");
        assert!(
            snapshot_path.exists(),
            "first run must create snapshot.json"
        );
        let mtime1 = std::fs::metadata(&snapshot_path)
            .unwrap()
            .modified()
            .unwrap();

        // Sleep past filesystem mtime granularity so a real write would
        // be observable.
        std::thread::sleep(std::time::Duration::from_millis(50));
        h.run(&ctx);
        let mtime2 = std::fs::metadata(&snapshot_path)
            .unwrap()
            .modified()
            .unwrap();

        assert_eq!(
            mtime1, mtime2,
            "second run with unchanged state must skip the on-disk write"
        );
    }
}
