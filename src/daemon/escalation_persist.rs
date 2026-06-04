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

/// Persist (insert/replace) one agent's escalation snapshot. Best-effort: a
/// write failure logs and is swallowed — escalation persistence must never block
/// the crash/hung hot path.
pub(crate) fn persist(home: &Path, name: &str, snapshot: &PersistedEscalation) {
    let path = store_file(home);
    if let Err(e) = store::mutate_versioned::<EscalationStore, _, _>(&path, |s| {
        s.agents.insert(name.to_string(), snapshot.clone());
        Ok(())
    }) {
        tracing::warn!(agent = %name, error = %e, "escalation_persist: write failed");
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
}
