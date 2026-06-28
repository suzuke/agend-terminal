//! #1488 boot-time orphan sweep.
//!
//! The cascade cleanup in `full_delete_instance` only runs for instances
//! deleted *after* that fix shipped. Instances deleted earlier (or via any
//! path that bypassed the cascade) left orphaned bindings behind: schedules
//! that fire into the cron self-IPC fallback (this morning's deadlock
//! trigger), dispatch_tracking entries that `sweep_stuck` re-warns forever
//! (the empirical ~81 "dispatch stuck check" messages), and CI watches whose
//! subscriber / `next_after_ci` point at a deleted agent.
//!
//! This boot sweep GCs those pre-existing orphans once at startup. It REUSES
//! the per-instance cleanup functions from the delete path so the two share
//! identical logic — no second implementation to drift. Policy matches the
//! cascade (lead decision, #1488):
//! - **schedules**: disabled + marked orphaned, never deleted (the operator
//!   may re-target a still-useful cron — e.g. an AI-Scout report whose backend
//!   was swapped). Idempotent across reboots.
//! - **dispatch_tracking / ci_watch**: GC'd (transient, no re-target value).

use std::collections::HashSet;
use std::path::Path;

/// Run the boot-time orphan sweep. Best-effort: every sub-step swallows its
/// own errors (matching the rest of the boot path) so a single failure can't
/// abort daemon startup.
pub fn run(home: &Path) {
    let known: HashSet<String> =
        match crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) {
            Ok(c) => c.instances.keys().cloned().collect(),
            // No fleet.yaml / parse error → can't tell ghost from live, so do
            // nothing rather than risk disabling every schedule.
            Err(_) => return,
        };

    let mut schedules_orphaned = 0usize;
    let mut dispatches_gced = 0usize;
    let mut watches_scrubbed = 0usize;

    // schedules: distinct enabled targets that no longer exist.
    let mut sched_targets: Vec<String> = crate::schedules::load(home)
        .schedules
        .iter()
        .filter(|s| s.enabled && !known.contains(&s.target))
        .map(|s| s.target.clone())
        .collect();
    sched_targets.sort();
    sched_targets.dedup();
    for target in sched_targets {
        schedules_orphaned += crate::schedules::orphan_schedules_for_target(home, &target);
    }

    // dispatch_tracking: active targets that no longer exist.
    for target in crate::dispatch_tracking::active_target_names(home) {
        if !known.contains(&target) {
            dispatches_gced += crate::dispatch_tracking::cleanup_for_instance(home, &target);
        }
    }

    // ci_watch: subscribers / next_after_ci pointing at gone instances.
    for name in unknown_watch_instances(home, &known) {
        watches_scrubbed += crate::daemon::ci_watch::cleanup_watches_for_instance(home, &name);
    }

    // dispatch_tracking terminal rows (completed/orphaned) for ANY target —
    // including LIVE ones whose dispatches completed or were given up. The
    // dead-target loop above only catches gone instances; this clears the
    // accumulated terminal backlog (e.g. the completed/orphaned rows behind the
    // flood) at boot rather than waiting for the 30-day TTL.
    let tracking_gced = crate::dispatch_tracking::sweep_terminal_entries(home);

    if schedules_orphaned + dispatches_gced + watches_scrubbed + tracking_gced > 0 {
        tracing::info!(
            schedules_orphaned,
            dispatches_gced,
            tracking_gced,
            watches_scrubbed,
            "#1488 boot orphan sweep: cleaned stale bindings of deleted instances"
        );
    }
}

/// Distinct instance names referenced by any CI watch (as a subscriber or as
/// `next_after_ci`) that are NOT in the known-instance set.
fn unknown_watch_instances(home: &Path, known: &HashSet<String>) -> Vec<String> {
    let dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(watch) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|c| serde_json::from_str::<crate::daemon::ci_watch::WatchState>(&c).ok())
            else {
                continue;
            };
            for sub in watch.subscriber_names() {
                if !known.contains(&sub) {
                    names.push(sub);
                }
            }
            for next in watch.next_after_ci_targets() {
                if !known.contains(&next) {
                    names.push(next);
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-1488-sweep-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn boot_sweep_cleans_orphans_and_preserves_known() {
        let home = tmp_home("boot");
        // fleet.yaml knows only "alive".
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  alive:\n    backend: claude\n",
        )
        .unwrap();
        // schedules: one targets the gone "ghost", one targets "alive".
        std::fs::write(
            home.join("schedules.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 2,
                "schedules": [
                    {"id": "s-ghost", "message": "m", "target": "ghost",
                     "trigger": {"kind": "cron", "expr": "0 9 * * *"}, "enabled": true,
                     "timezone": "UTC", "created_at": "2026-01-01T00:00:00Z",
                     "updated_at": "2026-01-01T00:00:00Z", "run_history": []},
                    {"id": "s-alive", "message": "m", "target": "alive",
                     "trigger": {"kind": "cron", "expr": "0 9 * * *"}, "enabled": true,
                     "timezone": "UTC", "created_at": "2026-01-01T00:00:00Z",
                     "updated_at": "2026-01-01T00:00:00Z", "run_history": []}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        // dispatch_tracking: pending entries to ghost and to alive.
        crate::dispatch_tracking::track_dispatch(
            &home,
            crate::dispatch_tracking::DispatchEntry {
                task_id: Some("t-g".into()),
                from: "lead".into(),
                to: "ghost".into(),
                from_id: None,
                to_id: None,
                delegated_at: chrono::Utc::now().to_rfc3339(),
                status: "pending".into(),
            },
        );
        crate::dispatch_tracking::track_dispatch(
            &home,
            crate::dispatch_tracking::DispatchEntry {
                task_id: Some("t-a".into()),
                from: "lead".into(),
                to: "alive".into(),
                from_id: None,
                to_id: None,
                delegated_at: chrono::Utc::now().to_rfc3339(),
                status: "pending".into(),
            },
        );
        // ci_watch: a watch with a ghost subscriber, one with alive.
        let dir = crate::daemon::ci_watch::ci_watches_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        for who in ["ghost", "alive"] {
            std::fs::write(
                dir.join(format!("{who}.json")),
                serde_json::to_string_pretty(&serde_json::json!({
                    "repo": "o/r",
                    "branch": who,
                    "subscribers": [{"instance": who}],
                }))
                .unwrap(),
            )
            .unwrap();
        }

        run(&home);

        // schedules: ghost disabled+marked, alive untouched.
        let scheds = crate::schedules::load(&home);
        let ghost_s = scheds.schedules.iter().find(|s| s.id == "s-ghost").unwrap();
        assert!(!ghost_s.enabled, "ghost-targeting schedule disabled");
        assert!(ghost_s
            .run_history
            .last()
            .is_some_and(|r| r.status.contains("orphaned")));
        let alive_s = scheds.schedules.iter().find(|s| s.id == "s-alive").unwrap();
        assert!(alive_s.enabled, "alive-targeting schedule preserved");

        // dispatch_tracking: ghost entry GC'd, alive entry kept.
        let store: serde_json::Value =
            crate::store::load(&crate::store::store_path(&home, "dispatch_tracking.json"));
        let tos: Vec<&str> = store["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["to"].as_str())
            .collect();
        assert_eq!(tos, vec!["alive"], "only alive dispatch survives: {tos:?}");

        // ci_watch: ghost watch removed (no survivors), alive watch intact.
        assert!(
            !dir.join("ghost.json").exists(),
            "ghost-only watch must be removed"
        );
        assert!(dir.join("alive.json").exists(), "alive watch must remain");

        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn boot_sweep_noop_without_fleet_yaml() {
        // No fleet.yaml → can't distinguish ghost from live → must do nothing
        // (never disable schedules on an unreadable fleet).
        let home = tmp_home("nofleet");
        std::fs::write(
            home.join("schedules.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 2,
                "schedules": [{"id": "s", "message": "m", "target": "whoever",
                    "trigger": {"kind": "cron", "expr": "0 9 * * *"}, "enabled": true,
                    "timezone": "UTC", "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:00Z", "run_history": []}]
            }))
            .unwrap(),
        )
        .unwrap();
        run(&home);
        assert!(
            crate::schedules::load(&home).schedules[0].enabled,
            "no fleet.yaml → schedule must stay enabled (no destructive sweep)"
        );
        std::fs::remove_dir_all(home).ok();
    }
}
