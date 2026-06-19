//! #2158 item 2 (lead decision (b)): hourly workspace-boundary sweep. Detects
//! stray daemon-managed worktrees via [`crate::workspace_boundary::detect_violations`]
//! and emits EDGE-TRIGGERED fleet event-log entries: exactly one `appear` when a
//! violation first shows up and one `resolve` when it clears — NEVER one-per-hour
//! while a violation stands (the noise-safety core, [[system_noise_reduction]]).
//!
//! The seen-set lives on the handler so the appear/resolve edges survive across
//! hourly ticks within a daemon process. Restart-survival (a persisted marker) is
//! a deliberate follow-up — a single re-`appear` per restart is acceptable.
//! Best-effort: a detect/emit failure logs and continues; it never breaks the
//! tick (the per-tick panic guard isolates it further).

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

pub(crate) struct WorkspaceBoundarySweepHandler {
    /// Cadence + boot-grace (suppresses the first sweep within the grace window
    /// of daemon boot, so a backlog of pre-restart strays doesn't all `appear` at
    /// once before the fleet settles).
    gate: crate::daemon::cadence_gate::CadenceGate,
    /// Violation identities seen on the previous sweep — drives appear/resolve
    /// edge detection. In-memory; survives ticks within a process.
    seen: Arc<Mutex<HashSet<String>>>,
}

impl WorkspaceBoundarySweepHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_with_boot_grace(
                every_n_ticks,
                super::NOTIFICATION_BOOT_GRACE,
            ),
            seen: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    #[cfg(test)]
    fn new_at(every_n_ticks: u64, created_at: std::time::Instant) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_with_boot_grace_at(
                every_n_ticks,
                created_at,
                super::NOTIFICATION_BOOT_GRACE,
            ),
            seen: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// The edge-trigger core, split out so tests drive it without the cadence
    /// gate. Emits `appear` for newly-seen identities + `resolve` for gone ones,
    /// then replaces the seen-set with the current one. Returns (appeared,
    /// resolved) for assertions.
    fn sweep_once(&self, home: &Path) -> (usize, usize) {
        let current: std::collections::HashMap<String, crate::workspace_boundary::Violation> =
            crate::workspace_boundary::detect_violations(home)
                .into_iter()
                .map(|v| (v.identity(), v))
                .collect();
        let mut seen = self.seen.lock();
        let mut appeared = 0;
        for (id, v) in &current {
            if !seen.contains(id) {
                appeared += 1;
                crate::event_log::log(
                    home,
                    "workspace_violation_appear",
                    v.agent.as_deref().unwrap_or("-"),
                    &format!("kind={} path={}", v.kind.as_str(), v.path.display()),
                );
            }
        }
        let gone: Vec<String> = seen
            .iter()
            .filter(|id| !current.contains_key(*id))
            .cloned()
            .collect();
        let resolved = gone.len();
        for id in &gone {
            crate::event_log::log(
                home,
                "workspace_violation_resolve",
                "-",
                &format!("id={id}"),
            );
        }
        *seen = current.into_keys().collect();
        (appeared, resolved)
    }
}

impl PerTickHandler for WorkspaceBoundarySweepHandler {
    fn name(&self) -> &'static str {
        "workspace_boundary_sweep"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        self.sweep_once(ctx.home);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-wsb-sweep-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn stray_worktree(home: &Path, agent: &str, branch: &str) -> PathBuf {
        let wt = crate::worktree_pool::daemon_managed_worktree_root(home)
            .join(agent)
            .join(branch);
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".agend-managed"), "").unwrap();
        wt
    }

    fn appear_count(home: &Path) -> usize {
        std::fs::read_to_string(home.join("event-log.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("workspace_violation_appear"))
            .count()
    }

    /// NOISE-SAFETY CORE: a standing violation across N sweeps emits exactly ONE
    /// `appear` (not N×) — the in-memory seen-set dedups subsequent sweeps.
    #[test]
    fn standing_violation_appears_once_across_n_sweeps() {
        let home = tmp_home("standing");
        let h = WorkspaceBoundarySweepHandler::new(1);
        stray_worktree(&home, "ghost", "feat/x");

        assert_eq!(h.sweep_once(&home), (1, 0), "first sweep: appear once");
        assert_eq!(
            h.sweep_once(&home),
            (0, 0),
            "second sweep: silent (standing)"
        );
        assert_eq!(h.sweep_once(&home), (0, 0), "third sweep: still silent");
        assert_eq!(
            appear_count(&home),
            1,
            "exactly ONE appear event for a standing violation across 3 sweeps"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// A resolved violation emits exactly ONE `resolve`, then stays silent.
    #[test]
    fn resolved_violation_emits_resolve_once() {
        let home = tmp_home("resolve");
        let h = WorkspaceBoundarySweepHandler::new(1);
        let wt = stray_worktree(&home, "ghost", "feat/x");

        assert_eq!(h.sweep_once(&home), (1, 0), "appear");
        std::fs::remove_dir_all(&wt).unwrap(); // violation clears
        assert_eq!(h.sweep_once(&home), (0, 1), "resolve once");
        assert_eq!(h.sweep_once(&home), (0, 0), "silent after resolve");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Boot-grace suppresses the first sweep (the `run` gate), so a stale backlog
    /// doesn't all-`appear` before the fleet settles; past grace it fires.
    #[test]
    fn boot_grace_suppresses_then_fires() {
        let fresh = WorkspaceBoundarySweepHandler::new(30); // created ≈ now → in grace
        assert!(!fresh.gate.fire(), "in boot-grace → suppressed");
        let past = std::time::Instant::now()
            - super::super::NOTIFICATION_BOOT_GRACE
            - std::time::Duration::from_secs(1);
        let aged = WorkspaceBoundarySweepHandler::new_at(30, past);
        assert!(aged.gate.fire(), "after grace, first tick fires");
    }

    #[test]
    fn name_matches_module() {
        assert_eq!(
            WorkspaceBoundarySweepHandler::new(360).name(),
            "workspace_boundary_sweep"
        );
    }
}
