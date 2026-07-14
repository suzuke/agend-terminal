//! #2496 (adversarial consensus d-20260701140903334693-1): the SAFE same-agent
//! rebind-repair path for `bind_self(rebase_mode=true)`.
//!
//! Split out of `force_release/mod.rs` to keep that file under the
//! `src/mcp/handlers` 750-LOC handler invariant (`tests/file_size_invariant.rs`)
//! — same reason `gc.rs` is its own file.

use super::rebase_clean_self;
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

/// Why [`attempt_safe_rebind_repair`] refused to repair. The acceptance
/// criteria requires fail-closed behavior with a clear, specific reason —
/// callers MUST surface this as a blocked error, never silently fall through
/// to a destructive release.
#[derive(Debug, Clone)]
pub(crate) enum RepairBlocked {
    Dirty,
    NotDaemonManaged,
    MarkerAgentMismatch(String),
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
            RepairBlocked::Dirty => write!(f, "worktree has uncommitted changes"),
            RepairBlocked::NotDaemonManaged => {
                write!(
                    f,
                    "worktree is not daemon-managed (.agend-managed marker missing)"
                )
            }
            RepairBlocked::MarkerAgentMismatch(a) => write!(
                f,
                "worktree's .agend-managed marker belongs to a different agent ('{a}')"
            ),
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
/// [`RepairAction::StaleStateCleared`] when a binding exists but its recorded
/// worktree is dead (missing, or not a git repo) — there's no LIVE protected
/// worktree here, so falling back to the legacy [`rebase_clean_self`] cleanup
/// (clear the corrupt binding + any leftover dir at the pre-#2496 path
/// formula) is safe; this is the ORIGINAL incident-recovery purpose
/// `rebase_mode` shipped for, and it's preserved unchanged.
///
/// Every other case — dirty, unmanaged, marker-agent mismatch, the requested
/// branch already held by another agent, an active CI watch or task still
/// tied to the worktree's CURRENT (about-to-be-abandoned) branch, or the
/// `git switch` itself failing — returns [`RepairBlocked`]. Callers MUST
/// treat this as fail-closed and must NOT fall through to a destructive
/// release: these are all cases where a LIVE worktree WAS found but touching
/// it isn't safe.
pub(crate) fn attempt_safe_rebind_repair(
    home: &Path,
    agent: &str,
    branch: &str,
) -> Result<RepairAction, RepairBlocked> {
    let Some(binding) = crate::binding::read(home, agent) else {
        return Ok(RepairAction::NoOp);
    };
    // codex-reviewer (PR #2523 review): the recorded binding branch can have
    // its own live dependents (CI watch / task) INDEPENDENT of the worktree's
    // actual branch — a "double-stale" binding (binding.branch=A, worktree
    // actually on B, requested C) must not silently abandon A's tracking just
    // because the mutation touches B→C. Preflight EVERY branch this call is
    // about to abandon — recorded_branch always, actual_branch too once known
    // — BEFORE any mutation (switch or the destructive fallback below).
    let recorded_branch = binding
        .get("branch")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let wt_str = binding
        .get("worktree")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let worktree = Path::new(wt_str);
    if wt_str.is_empty() || !worktree.exists() || !crate::worktree::is_git_repo(worktree) {
        // No LIVE worktree at the recorded path — nothing to protect from
        // mutation, but the binding we're about to CLEAR can still be the
        // only thing tracking `recorded_branch` for a live CI watch/task.
        if let Some(blocked) =
            reject_if_branch_has_dependents(home, agent, &recorded_branch, branch)
        {
            return Err(blocked);
        }
        // Stale-state cleanup: the recorded worktree is already GONE (no live WIP
        // to preserve here), and this repair helper carries no caller identity →
        // None routes any WIP-preserved notice to the agent's team orchestrator
        // (fallback: operator inbox), never a hardcoded recipient.
        return match rebase_clean_self(home, agent, branch, None, None) {
            Ok(_) => Ok(RepairAction::StaleStateCleared),
            Err(e) => Err(RepairBlocked::PathUnsafe(e)),
        };
    }
    if !crate::worktree_pool::is_daemon_managed(worktree) {
        return Err(RepairBlocked::NotDaemonManaged);
    }
    if let Some(marker_agent) = crate::binding::managed_marker_agent(worktree) {
        if marker_agent != agent {
            return Err(RepairBlocked::MarkerAgentMismatch(marker_agent));
        }
    }
    if crate::worktree::has_uncommitted_changes(worktree) {
        return Err(RepairBlocked::Dirty);
    }
    let source_repo_str = binding
        .get("source_repo")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if let Some(other) =
        crate::binding::scan_existing_branch_binding(home, source_repo_str, branch, agent)
    {
        return Err(RepairBlocked::OtherAgentHoldsBranch(other));
    }
    let actual_branch =
        crate::git_helpers::git_cmd(worktree, &["branch", "--show-current"]).unwrap_or_default();

    // Preflight BOTH candidate abandoned branches before any mutation —
    // recorded_branch (the binding we're about to overwrite) and, if it
    // differs, actual_branch (the branch the switch below would move away
    // from). Order doesn't matter: either blocking either one must happen
    // strictly before the `git switch` call.
    if let Some(blocked) = reject_if_branch_has_dependents(home, agent, &recorded_branch, branch) {
        return Err(blocked);
    }
    if actual_branch != recorded_branch {
        if let Some(blocked) = reject_if_branch_has_dependents(home, agent, &actual_branch, branch)
        {
            return Err(blocked);
        }
    }

    if actual_branch == branch {
        return Ok(RepairAction::MetadataOnly);
    }
    // Plain `git switch` — NOT `worktree::checkout_branch` (that falls back
    // to `git switch -c`, silently CREATING a branch; a repair path must
    // never do that as a side effect).
    use crate::git_helpers::{git_cmd, GitError};
    match git_cmd(worktree, &["switch", branch]) {
        Ok(_) => Ok(RepairAction::SwitchedBranch),
        Err(GitError::NonZero { stderr, .. }) => Err(RepairBlocked::SwitchFailed(stderr)),
        Err(GitError::Spawn(e)) => Err(RepairBlocked::SwitchFailed(e.to_string())),
    }
}

/// `Some(blocked)` iff `candidate` is a real branch being abandoned (non-empty
/// and different from the `target` we're moving TO) that still has a live
/// dependent (this agent's CI watch, or any active task linked to it).
/// `target` itself is never checked — it's where we're going, not what's
/// being abandoned.
fn reject_if_branch_has_dependents(
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

    fn write_managed_marker(worktree: &Path, agent: &str) {
        std::fs::write(
            worktree.join(crate::worktree_pool::MANAGED_MARKER),
            format!("agent={agent}\nbranch=irrelevant\n"),
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
        write_managed_marker(&wt, "agentA");
        let src = home.join("src");
        std::fs::create_dir_all(&src).unwrap();
        crate::binding::bind_full(&home, "agentA", "", "feat/x", &wt, &src, false)
            .expect("first bind ok");
        // Worktree is genuinely on `main` right now (never switched) —
        // request the branch it's ALREADY on.
        let result = attempt_safe_rebind_repair(&home, "agentA", "main");
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
        write_managed_marker(&wt, "agentA");
        let src = home.join("src");
        std::fs::create_dir_all(&src).unwrap();
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

        let result = attempt_safe_rebind_repair(&home, "agentA", "feat/y");
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
        write_managed_marker(&wt, "agentA");
        let src = home.join("src");
        std::fs::create_dir_all(&src).unwrap();
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

        let result = attempt_safe_rebind_repair(&home, "agentA", "C");
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
        write_managed_marker(&wt, "agentA");
        let src = home.join("src");
        std::fs::create_dir_all(&src).unwrap();
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

        let result = attempt_safe_rebind_repair(&home, "agentA", "C");
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
