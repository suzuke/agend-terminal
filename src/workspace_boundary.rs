//! #2158 item 2 (lead decision (b), 2026-06-19): detect stray daemon-managed
//! worktrees — a marker-managed worktree dir whose agent is neither live nor
//! bound, i.e. an orphan GC could not reclaim (e.g. a dirty worktree it
//! backs-up-but-leaves). Surfaced via an edge-triggered fleet event-log (exactly
//! appear + resolve per violation, NOT per-hour) + the fleet health summary —
//! NOT a per-agent BlockedReason slot (lead's clobber decision: a hygiene reason
//! must not clobber an execution reason like rate-limit / hang / crash).
//!
//! SCOPE NOTE: the original #2158 item-2 also named "cross-workspace drift", but
//! that half is deliberately NOT done here. An at-rest hourly sweep has no view
//! of an agent's live cwd, so it cannot replicate the shim's
//! `is_workspace_clone_drift` cwd gate; the naive at-rest predicate
//! (`workspace/<agent>` foreign to the bound worktree) false-positives on EVERY
//! agent, because `workspace/<agent>` is normally a SEPARATE clone from the
//! canonical worktree the agent is bound to. Active-cwd drift is already covered
//! by the #2290 shim-side bypass audit.

use std::path::{Path, PathBuf};

/// A detected workspace-boundary violation kind. (Currently one kind; the enum
/// keeps the surface ready for future kinds without reshaping callers.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViolationKind {
    /// A daemon-managed worktree whose agent is neither live nor bound.
    StrayWorktree,
}

impl ViolationKind {
    /// Stable string form — the identity prefix + event-log greppability anchor.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ViolationKind::StrayWorktree => "stray_worktree",
        }
    }
}

/// A single detected violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Violation {
    pub(crate) kind: ViolationKind,
    /// The agent the offending dir is filed under (None when unresolvable).
    pub(crate) agent: Option<String>,
    /// Canonical path of the offending worktree dir.
    pub(crate) path: PathBuf,
}

impl Violation {
    /// Stable identity for edge-trigger dedup + event-log correlation: kind +
    /// canonical path. The path alone is unique; the kind prefix keeps it
    /// unambiguous if more kinds are ever added.
    pub(crate) fn identity(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.path.display())
    }
}

/// Scan for current workspace-boundary violations. Pure, best-effort, no
/// mutation. The SINGLE source of truth shared by the hourly sweep handler
/// (which edge-triggers appear/resolve events off it) and the fleet health
/// summary (which reports the live count) — so the two never disagree.
pub(crate) fn detect_violations(home: &Path) -> Vec<Violation> {
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    // Liveness signals, mirroring `gc_candidates`: a managed worktree is
    // legitimate while its agent is in the live-agent set OR holds a binding
    // (covers an idle-but-running agent + a future-version binding via
    // `present_including_future`). A stray = NEITHER signal present.
    let live: std::collections::HashSet<String> = crate::runtime::list_agents_with_fallback(home)
        .into_iter()
        .collect();
    let mut out = Vec::new();
    // Reuse gc_run's enumeration (the SINGLE marker-walk + workspace-gitlink
    // impl) rather than a raw "dir exists" scan — so anything non-managed is
    // never mistaken for a stray, and the two stay in lockstep.
    for wt in crate::worktree_pool::fs_managed_worktrees(home) {
        let agent = crate::worktree_pool::agent_from_layout(home, &wt);
        let bound = agent
            .as_deref()
            .is_some_and(|a| crate::binding::present_including_future(home, a));
        let alive = agent.as_deref().is_some_and(|a| live.contains(a));
        if !bound && !alive {
            out.push(Violation {
                kind: ViolationKind::StrayWorktree,
                agent,
                path: canon(&wt),
            });
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-wsb-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Create a marker-managed worktree at `worktrees/<agent>/<branch>`.
    fn managed_worktree(home: &Path, agent: &str, branch: &str) -> PathBuf {
        let wt = crate::worktree_pool::daemon_managed_worktree_root(home)
            .join(agent)
            .join(branch);
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".agend-managed"), "").unwrap();
        wt
    }

    fn write_binding(home: &Path, agent: &str, wt: &Path) {
        let dir = crate::paths::runtime_dir(home).join(agent);
        std::fs::create_dir_all(&dir).unwrap();
        let v = serde_json::json!({
            "version": 1, "agent": agent, "task_id": "T", "branch": "b",
            "worktree": wt.to_str().unwrap(),
        });
        std::fs::write(dir.join("binding.json"), v.to_string()).unwrap();
    }

    /// A marker-managed worktree whose agent is neither live nor bound is flagged
    /// as a StrayWorktree (the core detection).
    #[test]
    fn stray_managed_worktree_with_no_agent_flagged() {
        let home = tmp_home("stray");
        let wt = managed_worktree(&home, "ghost", "feat/x");
        let v = detect_violations(&home);
        assert_eq!(
            v.len(),
            1,
            "an orphan managed worktree must be flagged: {v:?}"
        );
        assert_eq!(v[0].kind, ViolationKind::StrayWorktree);
        assert_eq!(v[0].agent.as_deref(), Some("ghost"));
        assert_eq!(v[0].path, dunce::canonicalize(&wt).unwrap());
        std::fs::remove_dir_all(&home).ok();
    }

    /// A BOUND managed worktree (binding.json present) must NOT be flagged — the
    /// #1 false-positive guard.
    #[test]
    fn bound_managed_worktree_not_flagged() {
        let home = tmp_home("bound");
        let wt = managed_worktree(&home, "boundy", "feat/y");
        write_binding(&home, "boundy", &wt);
        assert!(
            detect_violations(&home).is_empty(),
            "a bound managed worktree must NOT be flagged as stray"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Empty home → no managed worktrees → no violations (no spurious flags).
    #[test]
    fn empty_home_no_violations() {
        let home = tmp_home("empty");
        assert!(detect_violations(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }
}
