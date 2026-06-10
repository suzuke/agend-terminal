//! #1744-H2: durable per-agent escalation state.
//!
//! [`crate::health::HealthTracker`]'s escalation fields (crash budget, the two
//! per-class notify cooldowns, and the Hung confirm-window anchor) live in
//! memory and are lost on a daemon restart. That reset is the root cause of the
//! #1744-H2 P0-reachability gaps: a restart re-zeroes the crash budget (infinite
//! respawn that never reaches `Failed`), the confirm-window (delayed/missed
//! self-orch Hung P0), and the cooldowns (duplicate P0 for an already-paged
//! agent).
//!
//! This module persists a small [`PersistedEscalation`] snapshot per agent to a
//! single combined, versioned store keyed by agent name, written through the
//! `store` toolkit (flock-guarded atomic RMW). The daemon writes at the crash /
//! hung chokepoints and re-applies on boot/agent-register. All timestamps are
//! wall-clock epoch-ms — never a monotonic `Instant` (meaningless across a
//! process restart). See [`crate::health::HealthTracker::escalation_snapshot`] /
//! [`crate::health::HealthTracker::rehydrate_escalation`] for the conversion.

use crate::health::PersistedEscalation;
use crate::store::{self, SchemaVersioned};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

const STORE_FILE: &str = "health_escalation.json";
const CURRENT_VERSION: u32 = 1;

/// Combined on-disk store: every agent's escalation snapshot keyed by name.
/// Name-keyed (not UUID) to match the per-agent maps the daemon already keys by
/// name; the delete path ([`remove`]) clears the entry so a reused name never
/// rehydrates a prior occupant's state (the #1680 stale-state lesson).
#[derive(Debug, Default, Serialize, Deserialize)]
struct EscalationStore {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    agents: HashMap<String, PersistedEscalation>,
}

impl SchemaVersioned for EscalationStore {
    const CURRENT: u32 = CURRENT_VERSION;
    fn version_mut(&mut self) -> &mut u32 {
        &mut self.schema_version
    }
}

fn store_file(home: &Path) -> std::path::PathBuf {
    store::store_path(home, STORE_FILE)
}

/// Once-per-boot latch for the operator-visible persist-failure event. The
/// hang-detection tick calls `persist` repeatedly, so a sustained failure
/// (disk full / permissions) would otherwise spam one event per tick; one
/// event per daemon boot is the right cadence (and the latch resetting on
/// restart is exactly when re-alerting is wanted — the failure either healed
/// or is still there).
static PERSIST_FAILURE_EVENT_EMITTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Persist (insert/replace) one agent's escalation snapshot. Best-effort: a
/// write failure must never block the crash/hung hot path — but it is NOT
/// silent (Phase-A hardening): the crash budget + cooldown (#1744-H2) are the
/// guard against a respawn storm, and losing this write means a daemon
/// restart rehydrates a zeroed budget and re-allows crashes it should have
/// blocked. Failures log at ERROR (every occurrence — greppable during a
/// disk-full episode) and emit one operator-visible `event_log` entry per
/// boot.
pub(crate) fn persist(home: &Path, name: &str, snapshot: &PersistedEscalation) {
    let path = store_file(home);
    if let Err(e) = store::mutate_versioned::<EscalationStore, _, _>(&path, |s| {
        s.agents.insert(name.to_string(), snapshot.clone());
        Ok(())
    }) {
        tracing::error!(
            agent = %name,
            error = %e,
            "escalation_persist: write FAILED — crash budget/cooldown will NOT survive a daemon restart (respawn-storm guard disarmed across restarts until this heals)"
        );
        if !PERSIST_FAILURE_EVENT_EMITTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            crate::event_log::log(
                home,
                "escalation_persist_failed",
                name,
                &format!(
                    "crash-budget/cooldown persist failed (state will not survive a daemon \
                     restart): {e}. Further failures this boot log at error level only."
                ),
            );
        }
    }
}

/// Load the persisted snapshot for one agent (boot/agent-register rehydrate).
/// Returns `None` when there is no store yet or no entry for the name.
pub(crate) fn load_for(home: &Path, name: &str) -> Option<PersistedEscalation> {
    let path = store_file(home);
    let store: EscalationStore = store::load_versioned(&path, CURRENT_VERSION);
    store.agents.get(name).cloned()
}

/// Drop one agent's persisted entry (called on agent delete) so a later agent
/// reusing the name does not rehydrate stale escalation state.
pub(crate) fn remove(home: &Path, name: &str) {
    let path = store_file(home);
    if !path.exists() {
        return;
    }
    if let Err(e) = store::mutate_versioned::<EscalationStore, _, _>(&path, |s| {
        s.agents.remove(name);
        Ok(())
    }) {
        tracing::warn!(agent = %name, error = %e, "escalation_persist: remove failed");
    }
}

/// #1744-PR-B (latch-scope): clear ONLY the terminal-Failed once-off latch
/// (`failed_escalated`) for one agent, preserving the crash budget / cooldowns.
/// Called at operator-initiated recovery boundaries (start / restart / replace):
/// once the operator has intervened, a fresh terminal death must re-page — without
/// this the persisted latch (rehydrated onto the re-spawned tracker) would silence
/// the new death. A daemon restart does NOT route through these RPC handlers, so
/// the latch survives a plain restart (the same un-recovered death is not re-paged).
pub(crate) fn clear_failed_escalated(home: &Path, name: &str) {
    let path = store_file(home);
    if !path.exists() {
        return;
    }
    if let Err(e) = store::mutate_versioned::<EscalationStore, _, _>(&path, |s| {
        if let Some(entry) = s.agents.get_mut(name) {
            entry.failed_escalated = false;
        }
        Ok(())
    }) {
        tracing::warn!(agent = %name, error = %e, "escalation_persist: clear_failed_escalated failed");
    }
}

/// #1870-H2: clear ONLY the `hung_since` anchor for one agent in place,
/// preserving the crash budget / cooldowns / `failed_escalated` latch. Used by
/// the `left_hung` persist loop when an agent has a persisted record but has
/// already left the registry (crashed / deleted between the existence check and
/// the registry read): it has left Hung AND is gone, so its `hung_since` must
/// NOT survive to rehydrate a stale anchor on the next restart (which would fire
/// a false Hung P0 on the first post-restart tick). No-op when no store / no
/// entry exists. Distinct from `persist` (which needs a live snapshot) precisely
/// because here the agent is no longer in the registry.
pub(crate) fn clear_hung_since(home: &Path, name: &str) {
    let path = store_file(home);
    if !path.exists() {
        return;
    }
    if let Err(e) = store::mutate_versioned::<EscalationStore, _, _>(&path, |s| {
        if let Some(entry) = s.agents.get_mut(name) {
            entry.hung_since_epoch_ms = None;
        }
        Ok(())
    }) {
        tracing::warn!(agent = %name, error = %e, "escalation_persist: clear_hung_since failed");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!(
            "agend-escpersist-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn persist_load_remove_round_trip() {
        let home = tmp_home("rt");
        assert!(load_for(&home, "a").is_none(), "no store yet → None");

        let snap = PersistedEscalation {
            total_crashes: 4,
            crash_times_epoch_ms: vec![111, 222],
            last_crash_notification_epoch_ms: Some(333),
            last_hung_notification_epoch_ms: None,
            hung_since_epoch_ms: Some(444),
            failed_escalated: true,
        };
        persist(&home, "a", &snap);
        // A second agent's entry must not clobber the first.
        persist(&home, "b", &PersistedEscalation::default());

        assert_eq!(load_for(&home, "a"), Some(snap));
        assert_eq!(load_for(&home, "b"), Some(PersistedEscalation::default()));

        remove(&home, "a");
        assert!(load_for(&home, "a").is_none(), "removed entry → None");
        assert!(load_for(&home, "b").is_some(), "remove is per-agent");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn remove_on_missing_store_is_noop() {
        let home = tmp_home("rm-missing");
        remove(&home, "ghost"); // must not panic / create the file
        assert!(load_for(&home, "ghost").is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn clear_failed_escalated_resets_latch_preserves_budget_1744_prb() {
        // #1744-PR-B latch-scope: operator-initiated recovery clears ONLY the
        // terminal once-off latch (so a fresh terminal death re-pages), preserving
        // the crash budget / cooldowns. A plain daemon restart (rehydrate) does NOT
        // route through this, so the latch survives there.
        let home = tmp_home("clear-latch");
        persist(
            &home,
            "orch",
            &PersistedEscalation {
                total_crashes: 7,
                crash_times_epoch_ms: vec![100],
                last_crash_notification_epoch_ms: Some(200),
                last_hung_notification_epoch_ms: None,
                hung_since_epoch_ms: None,
                failed_escalated: true,
            },
        );

        clear_failed_escalated(&home, "orch");

        let after = load_for(&home, "orch").expect("entry survives the clear");
        assert!(
            !after.failed_escalated,
            "operator recovery must reset the terminal once-off latch"
        );
        assert_eq!(
            after.total_crashes, 7,
            "the crash budget must be preserved across an operator recovery"
        );
        assert_eq!(
            after.last_crash_notification_epoch_ms,
            Some(200),
            "the crash cooldown must be preserved"
        );

        // No-op on an unknown agent / a missing store (no panic, no file creation).
        clear_failed_escalated(&home, "ghost");
        let empty = tmp_home("clear-empty");
        clear_failed_escalated(&empty, "anyone");
        assert!(load_for(&empty, "anyone").is_none());

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&empty).ok();
    }

    /// §3.9 #1870-H2: `clear_hung_since` clears ONLY the `hung_since` anchor,
    /// preserving the crash budget / cooldowns AND the `failed_escalated` latch —
    /// it must not over-clear (the "don't wipe a still-Hung agent's other state"
    /// guard). After the clear, a simulated restart (`load_for`) sees no anchor →
    /// no false Hung re-escalation.
    #[test]
    fn clear_hung_since_clears_only_anchor_preserves_rest_1870_h2() {
        let home = tmp_home("clear-hung");
        persist(
            &home,
            "orch",
            &PersistedEscalation {
                total_crashes: 5,
                crash_times_epoch_ms: vec![100],
                last_crash_notification_epoch_ms: Some(200),
                last_hung_notification_epoch_ms: Some(300),
                hung_since_epoch_ms: Some(444),
                failed_escalated: true,
            },
        );

        clear_hung_since(&home, "orch");

        let after = load_for(&home, "orch").expect("entry survives the clear");
        assert!(
            after.hung_since_epoch_ms.is_none(),
            "#1870-H2: the stale hung anchor must be cleared (so a restart can't rehydrate it)"
        );
        assert!(
            after.failed_escalated,
            "#1870-H2: clearing the hung anchor must NOT touch the failed_escalated latch"
        );
        assert_eq!(after.total_crashes, 5, "crash budget must be preserved");
        assert_eq!(
            after.last_hung_notification_epoch_ms,
            Some(300),
            "the hung cooldown must be preserved"
        );

        // No-op on unknown agent / missing store (no panic, no file creation).
        clear_hung_since(&home, "ghost");
        let empty = tmp_home("clear-hung-empty");
        clear_hung_since(&empty, "anyone");
        assert!(load_for(&empty, "anyone").is_none());

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&empty).ok();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod persist_failure_tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!(
            "agend-escfail-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    /// Phase-A hardening regression: a persist write failure must SURFACE —
    /// an operator-visible `escalation_persist_failed` event-log entry (once
    /// per boot; the error-level tracing fires every occurrence). Failure is
    /// injected portably by occupying the store FILE path with a DIRECTORY,
    /// so the versioned write cannot land.
    #[test]
    fn persist_failure_emits_operator_event_once_1744h2() {
        let home = tmp_home("event");
        // Occupy the store file path with a directory → every write fails.
        std::fs::create_dir_all(store_file(&home)).unwrap();

        // Latch is process-global: reset for THIS test (tests in one binary
        // share statics; swap-back keeps other tests honest).
        PERSIST_FAILURE_EVENT_EMITTED.store(false, std::sync::atomic::Ordering::Relaxed);

        let snap = PersistedEscalation::default();
        persist(&home, "crashy", &snap);

        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("escalation_persist_failed") && log.contains("crashy"),
            "persist failure must emit the operator-visible event, got: {log}"
        );

        // Second failure in the same boot: error-log only, no duplicate event.
        persist(&home, "crashy", &snap);
        let log2 = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert_eq!(
            log2.matches("escalation_persist_failed").count(),
            1,
            "the event is once-per-boot latched (per-tick callers must not spam)"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
