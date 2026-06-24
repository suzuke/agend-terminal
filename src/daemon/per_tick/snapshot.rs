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
                let (agent_state, health_state, silent_secs, output_silent_secs) = {
                    let c = handle.core.lock();
                    // #2413 (B): the OPERATED state — what dispatch_idle / inbox /
                    // handoff / reply deciders read via snapshot.json. Promote the raw
                    // screen heuristic to the Shadow Observer's HIGH-CONFIDENCE
                    // correction (the SAME shared gate the pane badge uses, so badge and
                    // dispatch can never diverge — #1493 class) when default-ON and a
                    // correction applies; else the raw heuristic. `AGEND_OBSERVED_DISPATCH=0`
                    // (or the observer kill-switch) → raw, byte-identical to pre-#2413.
                    // NEVER writes `State::current` — the cycle-proof invariant (the
                    // reducer's screen input stays vterm-only, so a promoted state can't
                    // feed back into classification). Supersedes the #1523
                    // `authoritative_state` claude-hook-only POC (multi-backend now).
                    let raw = c.state.get_state();
                    let agent_state = if crate::daemon::shadow::operated_dispatch_enabled() {
                        c.observed_status
                            .as_ref()
                            .and_then(|s| crate::daemon::shadow::gate::gated_override(raw, s))
                            .unwrap_or(raw)
                    } else {
                        raw
                    };
                    (
                        agent_state.display_name().to_string(),
                        c.health.state.display_name().to_string(),
                        // #1694②: productive-silence for the dispatch-idle
                        // silence-clock (marker/heartbeat-gated, spinner-resistant).
                        c.state.productive_silence().as_secs() as i64,
                        // #1961 phase-2: raw pane-change silence (screen-hash
                        // delta, classification-free) for the dispatch-idle
                        // pane-activity suppress.
                        c.state.output_silence().as_secs() as i64,
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
                    silent_secs,
                    output_silent_secs,
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

    /// #2413 (B): the operated `agent_state` written to `snapshot.json` (what dispatch_idle
    /// / inbox / handoff / reply read) is the GATED `observed_status` promotion when
    /// enabled (default-ON), and the raw screen heuristic when the `AGEND_OBSERVED_DISPATCH=0`
    /// kill-switch is set (byte-identical to pre-#2413). Drives the REAL handler through
    /// `crate::snapshot::load`. `#[cfg(unix)]` — `mk_test_handle` is unix-only.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(shadow_observer)]
    fn operated_state_promotes_observed_or_falls_back_on_kill_switch() {
        use crate::daemon::shadow::evidence::{Authority, Confidence};
        use crate::daemon::shadow::reducer::{ObservedState, ObservedStatus};

        struct EnvGuard(&'static str, Option<String>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.1 {
                    Some(v) => std::env::set_var(self.0, v),
                    None => std::env::remove_var(self.0),
                }
            }
        }
        let _g1 = EnvGuard(
            "AGEND_SHADOW_OBSERVER",
            std::env::var("AGEND_SHADOW_OBSERVER").ok(),
        );
        let _g2 = EnvGuard(
            "AGEND_OBSERVED_DISPATCH",
            std::env::var("AGEND_OBSERVED_DISPATCH").ok(),
        );

        let home = tmp_home("operated");
        let id = crate::types::InstanceId::default();
        let handle = crate::agent::mk_test_handle("opagent", id);
        // Raw screen state stays Idle (StateTracker::new default); attach a high-confidence
        // Active correction (the mid-API false-idle shape).
        handle.core.lock().observed_status = Some(ObservedStatus {
            state: ObservedState::Active,
            authority: Authority::Hook,
            confidence: Confidence::Strong,
            evidence: vec![],
            since_ms: 0,
        });
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::from([(id, handle)])));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        let agent_state = |home: &std::path::Path| -> String {
            crate::snapshot::load(home)
                .unwrap()
                .agents
                .into_iter()
                .find(|a| a.name == "opagent")
                .unwrap()
                .agent_state
        };

        // Default-ON: the high-confidence false-idle correction is promoted → "thinking".
        std::env::set_var("AGEND_SHADOW_OBSERVER", "1");
        std::env::remove_var("AGEND_OBSERVED_DISPATCH");
        SnapshotRotationHandler::new().run(&ctx);
        assert_eq!(
            agent_state(&home),
            "thinking",
            "operated state promotes the high-confidence observed correction"
        );

        // Kill-switch: raw heuristic only → "idle" (byte-identical to pre-#2413). A fresh
        // handler avoids the unchanged-state dedup skip.
        std::env::set_var("AGEND_OBSERVED_DISPATCH", "0");
        SnapshotRotationHandler::new().run(&ctx);
        assert_eq!(
            agent_state(&home),
            "idle",
            "AGEND_OBSERVED_DISPATCH=0 falls back to the raw heuristic"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
