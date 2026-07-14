//! #2496 (adversarial consensus d-20260701140903334693-1): the SAFE same-agent
//! rebind-repair path for `bind_self(rebase_mode=true)`.
//!
//! Split out of `force_release/mod.rs` to keep that file under the
//! `src/mcp/handlers` 750-LOC handler invariant (`tests/file_size_invariant.rs`)
//! — same reason `gc.rs` is its own file.

use std::path::Path;

/// What (if anything) [`attempt_safe_rebind_repair`] changed. The issue's
/// acceptance criteria requires callers to know exactly which of these
/// happened — never a bare boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepairAction {
    /// No existing binding for this agent — nothing to repair; the normal
    /// bind proceeds untouched (fresh first-bind semantics).
    NoOp,
    /// A binding existed but its recorded worktree is dead (missing / not a
    /// git repo) — there is no LIVE protected worktree to endanger, so this
    /// is exactly the original `rebase_clean_self` incident-recovery case
    /// (stale/corrupt binding + any leftover dir at the legacy path
    /// formula): legacy cleanup ran (`destructive_release` semantics, but
    /// safe here because nothing live was touched).
    StaleStateCleared,
    /// The worktree was already on the requested branch; no worktree
    /// mutation was needed — `bind_full`'s own #2496 guard-b exception lets
    /// the immediately-following bind write binding.json to match reality.
    MetadataOnly,
    /// The worktree was clean but on a different branch; an in-place
    /// `git switch` (never `-c` — no branch creation in a repair path)
    /// moved it to the requested branch.
    SwitchedBranch,
}

pub(crate) struct RepairResult {
    pub(crate) action: RepairAction,
    pub(crate) continuation: Option<RebaseContinuation>,
}

pub(crate) struct RebaseContinuation {
    pub(crate) worktree: std::path::PathBuf,
    pub(crate) source_repo: std::path::PathBuf,
    pub(crate) requested_branch: String,
    pub(crate) previous_branch: String,
    pub(crate) marker_body: Vec<u8>,
    pub(crate) binding_body: Vec<u8>,
    pub(crate) binding_signature: Option<Vec<u8>>,
    pub(crate) binding_fingerprint: crate::binding::BindingFingerprint,
}

impl RebaseContinuation {
    pub(crate) fn rollback(&self, home: &Path, agent: &str) -> Result<(), String> {
        let _lease = crate::binding::acquire_branch_lease_lock(
            home,
            &self.source_repo.display().to_string(),
            &self.requested_branch,
        )
        .map_err(|e| format!("rollback branch lease lock failed: {e}"))?;
        let _agent_lock = crate::binding::acquire_agent_mutation_lock(home, agent)?;
        let _binding_lock = crate::binding::acquire_binding_file_lock(home, agent)?;
        let live = crate::binding::guarded_binding_disk_fresh(home, agent);
        match live {
            crate::binding::GuardedBinding::Known { fingerprint, .. }
                if fingerprint == self.binding_fingerprint => {}
            crate::binding::GuardedBinding::Known { .. } => {
                return Err(
                    "rollback refused: binding generation advanced while rebase was in flight"
                        .to_string(),
                )
            }
            crate::binding::GuardedBinding::Absent => {
                return Err(
                    "rollback refused: binding disappeared while rebase was in flight".to_string(),
                )
            }
            crate::binding::GuardedBinding::Opaque(reason) => {
                return Err(format!(
                    "rollback refused: binding became opaque while rebase was in flight: {reason}"
                ))
            }
        }
        let current = crate::git_helpers::git_cmd(&self.worktree, &["branch", "--show-current"])
            .map_err(|e| format!("rollback read current branch failed: {e}"))?;
        if current != self.previous_branch {
            crate::git_helpers::git_cmd(&self.worktree, &["switch", &self.previous_branch])
                .map_err(|e| format!("rollback git switch failed: {e}"))?;
        }
        std::fs::write(
            self.worktree.join(crate::worktree_pool::MANAGED_MARKER),
            &self.marker_body,
        )
        .map_err(|e| format!("rollback marker write failed: {e}"))?;
        crate::store::atomic_write(&crate::paths::binding_path(home, agent), &self.binding_body)
            .map_err(|e| format!("rollback binding write failed: {e}"))?;
        let restored_binding = serde_json::from_slice(&self.binding_body)
            .map_err(|e| format!("rollback binding parse failed: {e}"))?;
        crate::binding::refresh_cached(home, agent, restored_binding);
        let signature = crate::paths::runtime_dir(home)
            .join(agent)
            .join("binding.json.sig");
        match &self.binding_signature {
            Some(body) => crate::store::atomic_write(&signature, body)
                .map_err(|e| format!("rollback binding signature write failed: {e}"))?,
            None => {
                let _ = std::fs::remove_file(signature);
            }
        }
        Ok(())
    }
}

impl RepairResult {
    pub(crate) fn no_continuation(action: RepairAction) -> Self {
        Self {
            action,
            continuation: None,
        }
    }
}

/// Why [`attempt_safe_rebind_repair`] refused to repair. The acceptance
/// criteria requires fail-closed behavior with a clear, specific reason —
/// callers MUST surface this as a blocked error, never silently fall through
/// to a destructive release.
#[derive(Debug, Clone)]
pub(crate) enum RepairBlocked {
    Opaque(String),
    TargetOpaque(String),
    BindingChanged,
    Dirty,
    OtherAgentHoldsBranch(String),
    ActiveCiWatch,
    ActiveTask,
    SwitchFailed(String),
    /// The legacy stale-state cleanup ([`rebase_clean_self`]) itself refused
    /// (path-safety violation) — defense-in-depth; `agent_ops::validate_branch`
    /// should already have caught this upstream.
    PathUnsafe(String),
}

impl std::fmt::Display for RepairBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepairBlocked::Opaque(e) => write!(f, "opaque binding state: {e}"),
            RepairBlocked::TargetOpaque(e) => write!(f, "opaque target state: {e}"),
            RepairBlocked::BindingChanged => write!(f, "binding changed during guarded repair"),
            RepairBlocked::Dirty => write!(f, "worktree has uncommitted changes"),
            RepairBlocked::OtherAgentHoldsBranch(a) => {
                write!(f, "branch is already held by another agent ('{a}')")
            }
            RepairBlocked::ActiveCiWatch => write!(
                f,
                "the worktree's current branch has an active CI watch for this agent"
            ),
            RepairBlocked::ActiveTask => write!(
                f,
                "the worktree's current branch has an active task linked to it"
            ),
            RepairBlocked::SwitchFailed(e) => write!(f, "git switch failed: {e}"),
            RepairBlocked::PathUnsafe(e) => write!(f, "{e}"),
        }
    }
}

/// Attempt a SAFE same-agent rebind repair — tried FIRST by
/// `bind_self(rebase_mode=true)`, in place of the old unconditional
/// `rebase_clean_self` (which was exactly as destructive as
/// `release_worktree`, defeating the whole point of `rebase_mode`).
///
/// Reads the agent's ACTUAL bound worktree via `binding::read` (the #2496
/// root-cause fix — NOT a recomputed `worktrees/<agent>/<branch>` path
/// formula, which never matches the flat `worktrees/<agent>-<source>` layout
/// `repo action=checkout bind:true` actually uses). Only if that worktree is
/// daemon-managed, owned by THIS agent, and clean does it proceed:
/// - already on `branch` → [`RepairAction::MetadataOnly`] (nothing to mutate;
///   the caller's subsequent `bind_full` picks this up via its own #2496
///   guard-b exception).
/// - on a different branch → in-place `git switch` (plain, no `-c` — this is
///   a repair path, it must never silently create a branch) →
///   [`RepairAction::SwitchedBranch`].
///
/// [`RepairAction::NoOp`] when there's no existing binding at all — nothing
/// live to protect, the caller's normal bind proceeds as a fresh first-bind.
///
/// [`RepairAction::StaleStateCleared`] when a binding or exact managed target
/// is stale and the guarded transaction clears it without touching unrelated
/// worktrees.
///
/// Every other case — dirty, marker-agent mismatch, the requested
/// branch already held by another agent, an active CI watch or task still
/// tied to the worktree's CURRENT (about-to-be-abandoned) branch, or the
/// `git switch` itself failing — returns [`RepairBlocked`]. Callers MUST
/// treat this as fail-closed and must NOT fall through to a destructive
/// release: these are all cases where a LIVE worktree WAS found but touching
/// it isn't safe.
#[allow(dead_code)]
pub(crate) fn attempt_safe_rebind_repair(
    home: &Path,
    agent: &str,
    branch: &str,
    explicit_repo: Option<&Path>,
    sender: Option<&str>,
) -> Result<RepairAction, RepairBlocked> {
    attempt_safe_rebind_repair_with_continuation(home, agent, branch, explicit_repo, sender)
        .map(|result| result.action)
}

pub(crate) fn attempt_safe_rebind_repair_with_continuation(
    home: &Path,
    agent: &str,
    branch: &str,
    explicit_repo: Option<&Path>,
    sender: Option<&str>,
) -> Result<RepairResult, RepairBlocked> {
    let guard = crate::mcp::handlers::dispatch_hook::acquire_rebase_guard(home, agent)
        .map_err(RepairBlocked::PathUnsafe)?;
    let permit = guard.permit();
    attempt_safe_rebind_repair_with_permit(home, agent, branch, explicit_repo, sender, permit)
}

pub(crate) fn attempt_safe_rebind_repair_with_permit(
    home: &Path,
    agent: &str,
    branch: &str,
    explicit_repo: Option<&Path>,
    sender: Option<&str>,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> Result<RepairResult, RepairBlocked> {
    super::s2::rebase_repair(home, agent, branch, explicit_repo, sender, permit)
}

/// `Some(blocked)` iff `candidate` is a real branch being abandoned (non-empty
/// and different from the `target` we're moving TO) that still has a live
/// dependent (this agent's CI watch, or any active task linked to it).
/// `target` itself is never checked — it's where we're going, not what's
/// being abandoned.
pub(crate) fn reject_if_branch_has_dependents(
    home: &Path,
    agent: &str,
    candidate: &str,
    target: &str,
) -> Option<RepairBlocked> {
    if candidate.is_empty() || candidate == target {
        return None;
    }
    if crate::binding::agent_has_active_ci_watch_on_branch(home, agent, candidate) {
        return Some(RepairBlocked::ActiveCiWatch);
    }
    if crate::binding::branch_has_active_task(home, candidate) {
        return Some(RepairBlocked::ActiveTask);
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(suffix: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let h = std::env::temp_dir().join(format!(
            "agend-force-release-repair-{}-{}-{}",
            std::process::id(),
            suffix,
            id,
        ));
        std::fs::create_dir_all(&h).ok();
        h
    }

    /// A REAL git-repo dir, deliberately at a FLAT (`<home>/live/<agent>`)
    /// path — NOT the legacy `<home>/worktrees/<agent>/<branch>` formula — so
    /// tests prove `attempt_safe_rebind_repair` resolves the worktree via
    /// `binding::read` (the actual #2496 fix) rather than any path formula.
    fn tmp_git_repo(home: &Path, agent: &str) -> std::path::PathBuf {
        let dir = home.join("live").join(agent);
        std::fs::create_dir_all(&dir).unwrap();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&dir)
            .output()
            .ok();
        std::fs::write(dir.join(".gitignore"), ".agend-managed\n").unwrap();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["add", ".gitignore"])
            .current_dir(&dir)
            .output()
            .ok();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@test",
                "commit",
                "-q",
                "-m",
                "init",
            ])
            .current_dir(&dir)
            .output()
            .ok();
        dir
    }

    fn git_switch(repo: &Path, branch: &str) {
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["switch", "-c", branch])
            .current_dir(repo)
            .output()
            .ok();
    }

    fn write_managed_marker(worktree: &Path, agent: &str, branch: &str, source_repo: &Path) {
        std::fs::write(
            worktree.join(crate::worktree_pool::MANAGED_MARKER),
            format!(
                "agent={agent}\nbranch={branch}\nsource_repo={}\n",
                source_repo.display()
            ),
        )
        .unwrap();
    }

    /// #2496 (consensus test 7): the worktree lives at a FLAT path (not the
    /// legacy `<agent>/<branch>` formula `rebase_clean_self` computes) and is
    /// already on the requested branch — `attempt_safe_rebind_repair` must
    /// find it via `binding::read`, report `MetadataOnly`, and touch NEITHER
    /// the worktree NOR call any destructive release.
    #[test]
    fn attempt_safe_rebind_repair_finds_real_worktree_via_binding_metadata_only_2496() {
        let home = tmp_home("2496-repair-metadata-only");
        let wt = tmp_git_repo(&home, "agentA");
        let src = home.join("src");
        std::fs::create_dir_all(&src).unwrap();
        write_managed_marker(&wt, "agentA", "main", &src);
        crate::binding::bind_full(&home, "agentA", "", "feat/x", &wt, &src, false)
            .expect("first bind ok");
        // Worktree is genuinely on `main` right now (never switched) —
        // request the branch it's ALREADY on.
        let result = attempt_safe_rebind_repair(&home, "agentA", "main", Some(&src), None);
        assert!(
            matches!(result, Ok(RepairAction::MetadataOnly)),
            "expected MetadataOnly, got {result:?}"
        );
        assert!(wt.exists(), "worktree must be untouched: {result:?}");
        assert!(
            crate::binding::read(&home, "agentA").is_some(),
            "binding must be untouched (no release_full call)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2496 (consensus test 8): the worktree is clean but on a DIFFERENT
    /// branch than requested, with no stale-branch dependents —
    /// `attempt_safe_rebind_repair` performs an in-place `git switch` and
    /// reports `SwitchedBranch`.
    #[test]
    fn attempt_safe_rebind_repair_switches_branch_in_place_when_clean_2496() {
        let home = tmp_home("2496-repair-switch");
        let wt = tmp_git_repo(&home, "agentA");
        let src = home.join("src");
        std::fs::create_dir_all(&src).unwrap();
        write_managed_marker(&wt, "agentA", "main", &src);
        crate::binding::bind_full(&home, "agentA", "", "main", &wt, &src, false)
            .expect("first bind ok");
        // `feat/y` must already exist as a local ref — the repair's plain
        // `git switch` (no `-c`) deliberately never creates a branch.
        git_switch(&wt, "feat/y");
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["switch", "main"])
            .current_dir(&wt)
            .output()
            .ok();

        let result = attempt_safe_rebind_repair(&home, "agentA", "feat/y", Some(&src), None);
        assert!(
            matches!(result, Ok(RepairAction::SwitchedBranch)),
            "expected SwitchedBranch, got {result:?}"
        );
        let actual = crate::git_helpers::git_cmd(&wt, &["branch", "--show-current"]).unwrap();
        assert_eq!(actual, "feat/y", "worktree must be switched in place");
        assert!(
            crate::binding::read(&home, "agentA").is_some(),
            "binding must survive the switch (no release_full call)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// codex-reviewer (PR #2523 review, finding 1): the "double-stale
    /// ordering" bug — binding.branch=A, worktree ACTUALLY on B, requested C.
    /// A has a live dependent. Pre-fix, the repair checked dependents only on
    /// B, then switched the worktree to C — mutating live state before
    /// `bind_full`'s later guard-b check on A could ever reject. Must now
    /// block on A's dependent BEFORE any `git switch`, leaving the worktree
    /// on B untouched.
    #[test]
    fn attempt_safe_rebind_repair_blocks_on_recorded_branch_dependent_before_switching_2496() {
        let home = tmp_home("2496-repair-double-stale");
        let wt = tmp_git_repo(&home, "agentA");
        let src = home.join("src");
        std::fs::create_dir_all(&src).unwrap();
        write_managed_marker(&wt, "agentA", "B", &src);
        // binding.branch = "A".
        crate::binding::bind_full(&home, "agentA", "", "A", &wt, &src, false)
            .expect("first bind ok");
        // Worktree drifts to B out-of-band (bind/release bypassed).
        git_switch(&wt, "B");
        // A (the STALE recorded branch, not B) has an active task.
        crate::tasks::handle(
            &home,
            "agentA",
            &serde_json::json!({
                "action": "create",
                "title": "work on A",
                "assignee": "agentA",
                "branch": "A",
            }),
        );

        let result = attempt_safe_rebind_repair(&home, "agentA", "C", Some(&src), None);
        assert!(
            matches!(result, Err(RepairBlocked::ActiveTask)),
            "expected ActiveTask block on the recorded branch A, got {result:?}"
        );
        let actual = crate::git_helpers::git_cmd(&wt, &["branch", "--show-current"]).unwrap();
        assert_eq!(
            actual, "B",
            "worktree must NOT have been switched — the block must land before any mutation"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// codex-reviewer (PR #2523 review, finding 2): the dead-worktree
    /// fallback must also preflight the recorded branch's dependents before
    /// falling through to the destructive `rebase_clean_self` — a missing
    /// worktree doesn't make a live CI watch/task on the recorded branch
    /// disappear.
    #[test]
    fn attempt_safe_rebind_repair_blocks_stale_state_cleanup_when_recorded_branch_has_dependent_2496(
    ) {
        let home = tmp_home("2496-repair-dead-with-dependent");
        let wt = tmp_git_repo(&home, "agentA");
        let src = home.join("src");
        std::fs::create_dir_all(&src).unwrap();
        write_managed_marker(&wt, "agentA", "main", &src);
        crate::binding::bind_full(&home, "agentA", "", "A", &wt, &src, false)
            .expect("first bind ok");
        // Worktree is now GONE — the incident-recovery scenario.
        std::fs::remove_dir_all(&wt).ok();
        crate::tasks::handle(
            &home,
            "agentA",
            &serde_json::json!({
                "action": "create",
                "title": "work on A",
                "assignee": "agentA",
                "branch": "A",
            }),
        );

        let result = attempt_safe_rebind_repair(&home, "agentA", "C", Some(&src), None);
        assert!(
            matches!(result, Err(RepairBlocked::ActiveTask)),
            "expected ActiveTask block, got {result:?}"
        );
        assert!(
            crate::binding::read(&home, "agentA").is_some(),
            "binding must survive — rebase_clean_self/release_full must NOT have run"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
