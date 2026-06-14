[繁體中文](FEATURE-worktree.zh-TW.md)

# Worktree Management

This document describes git worktree creation, binding, release, and garbage
collection, as well as the division of responsibility among `bind_self`,
`release_worktree`, `force_release_worktree`, `repo action=checkout`, and
`repo action=release`.

## Usage Scenarios

> **Target audience:** Agent infrastructure — fully managed by the daemon; agents work in worktrees transparently without needing to manage them directly.

**Automatic workspace isolation.** When a lead dispatches a task with a branch name to a dev agent, the daemon provisions a dedicated git worktree for that agent at `~/.agend/worktrees/<agent>/<branch>/`. The dev works in this isolated directory, making commits and pushing without risk of conflicting with other agents working on different branches in the same repository.

**Mid-lifecycle recovery.** An agent's worktree binding becomes stale after a crash or daemon restart. The agent uses `bind_self` to re-establish its binding to the existing worktree, optionally with `rebase_mode=true` to rebase onto the latest upstream changes before resuming work.

**Controlled cleanup.** After a task is completed and the PR is merged, the daemon soft-releases the worktree (marking it with `released_at`). The worktree remains on disk for a 24-hour grace period. If no one reclaims it, GC (`gc_cutover` with `AGEND_WORKTREE_GC=1`) removes it automatically. For stuck or orphaned worktrees, the operator can use `force_release_worktree` as an emergency measure.

**Auto-release on merge (#1344).** When the pr_state scanner detects a PR has been merged, it automatically calls `auto_release_for_merged_branch` to free the worktree before emitting the `[pr-merged]` notification. This ensures `gh pr merge --delete-branch` succeeds without manual intervention. Dirty worktrees are skipped (with a warning log) and retried on the next scanner tick.

## 1. Design Rationale

- Each agent works in its own isolated workspace to avoid branch conflicts and shared-checkout contention.
- Worktrees are the core isolation mechanism, making the relationship between branches and agents trackable.
- The canonical layout is `~/.agend/worktrees/<agent>/<branch>/` (daemon-managed).
- Legacy layouts are detected but never used for new creation.
- State is described by three markers:
  - `binding.json` — runtime lease (the key input for release).
  - `.agend-managed` — daemon ownership marker (agent, branch, leased_at, released_at).
  - `.agend-pinned` — operator override to prevent GC.
- GC only targets daemon-managed worktrees; operator-created worktrees are never auto-deleted.

## 2. Directories and Markers

- `worktree_path(home, agent, branch)` returns the canonical path: `<home>/worktrees/<agent>/<branch>/`.
- `legacy_worktree_path` is used only for detecting old layouts (`<source_repo>/.worktrees/<agent>/`).
- `is_daemon_managed()` checks for `.agend-managed`.
- `.agend-managed` records `agent=`, `branch=`, `leased_at=`; after release, also `released_at=`.
- `.agend-pinned` contains a timestamp and prevents GC.
- `binding.json` records the worktree path and `source_repo`, enabling release to operate from the owning repo's perspective.

## 3. Worktree Creation

- `worktree::create()` verifies the target is a git repo (returns `None` otherwise).
- For repos without HEAD, it attempts an init commit to bootstrap worktree creation.
- If no branch is specified, it defaults to `agend/<instance_name>`.
- Branch names are validated; illegal names are rejected.
- If the target directory already exists:
  - Matching branch: reuse the existing worktree.
  - Mismatched branch: report a lease conflict.
- If the directory does not exist, `git worktree add` is called (with a fallback path on failure).

## 4. Creation Entry Points

### `repo action=checkout`

The most common external entry point.

- `bind=true` — atomic provision + bind. Requires `AGEND_INSTANCE_NAME`, rejects protected branches, creates a named branch on HEAD, and runs tail-ops (marker, binding.json, CI watch).
- `bind=false` — creates a detached-HEAD worktree for inspection only.

### `bind_self`

Suitable for mid-lifecycle rebinding.

- Can use `source_repo` from `fleet.yaml`.
- Supports `rebase_mode=true`.
- Preferred for recovery scenarios; fresh-task dispatch should use `repo action=checkout bind:true`.

## 5. Lease and Bind

- `worktree_pool::lease()` rejects protected branches (per `agent_ops::is_protected_ref`).
- Lease calls `worktree::create()`, then writes `.agend-managed` and `binding.json`.
- Both `bind_self` and `repo action=checkout bind:true` ultimately go through lease logic.
- Lease failure typically indicates a branch conflict or validation issue.
- The bind-in-flight guard is cleaned up by release/cleanup.

## 6. Soft Release

- `worktree_pool::release()` is a soft mark — it does not delete the worktree.
- It unbinds the lease and writes `released_at` into `.agend-managed`.
- The worktree becomes a GC candidate seed but continues to exist.
- This is Phase 3 soft marking, not hard deletion.

## 7. Hard Release

`release_full()` is the core of the `release_worktree` MCP tool.

1. Reads `binding.json`; returns idempotent no-op if absent.
2. Only processes `.agend-managed` worktrees; refuses to delete unmarked ones.
3. If the worktree path no longer exists, prunes related git metadata.
4. Attempts `git worktree remove --force`; falls back to `remove_dir_all` on failure.
5. Cleans up `binding.json` and the bind-in-flight guard.
6. Optionally runs branch cleanup (only for managed-verified worktrees).

## 8. Branch Cleanup

- Runs only for released daemon-managed worktrees.
- Skips protected branches.
- Fetches with `--prune`, checks if the branch is merged into main, and checks if the remote tracking ref has vanished.
- Deletes the local branch only when conditions are met.
- `branch_cleanup_skipped_reason` provides an auditable explanation when cleanup is skipped.

## 9. Emergency Cleanup

`force_release_worktree` is the emergency tool for stale worktree directories.

- Target must be within `<home>/worktrees/<agent>/<branch>/`; paths outside the pool are rejected.
- Attempts `remove_dir_all` directly (directory deletion failure does not block binding cleanup).
- Calls `release_full()` and runs git metadata prune.
- Shares safety logic with `rebase_clean_self()` (used by `bind_self(rebase_mode=true)`).

## 10. Repo Release

`repo action=release` is a path-centric release.

- Accepts a path; validates and canonicalizes it.
- Rejects unsafe system paths (including `HOME` itself) and paths that are too shallow.
- Attempts `git worktree remove --force`; falls back to `remove_dir_all`.
- Does not depend on agent binding — it is purely path-based.
- Compare with `release_worktree`, which is agent-centric.

## 11. Garbage Collection Semantics

- `gc_dry_run()` lists candidates without deleting anything.
- `gc_cutover()` performs actual deletion; requires `AGEND_WORKTREE_GC=1`.
- GC skips: non-daemon-managed worktrees, pinned worktrees, worktrees with active bindings, and worktrees without `released_at`.
- Grace window: 24 hours after `released_at`.
- Both dry-run and cutover record to the event log.

## 12. GC Candidate Criteria

A worktree is a GC candidate when:
1. It is daemon-managed.
2. It is not pinned.
3. Its agent name can be parsed.
4. It has no active binding.
5. It has a `released_at` timestamp older than 24 hours.

Both the new layout (`<home>/worktrees/<agent>/<branch>/`) and legacy layout (`<home>/workspace/*/.worktrees/*/`) are scanned. `evaluate_candidate()` centralizes the criteria so dry-run and cutover share the same logic.

## 13. Pin / Unpin

- `.agend-pinned` is an operator override marker.
- `pin()` writes a timestamp; `unpin()` removes the file.
- Pinning prevents GC but does not change binding or `released_at`.
- Use pin for worktrees that need long-term preservation.

## 14. Orphan Reconciliation

- `reconcile_orphan_leases()` is a boot-time, log-only scan.
- It finds runtime `binding.json` entries whose worktree paths no longer exist.
- It does not delete anything — it is diagnostic, not destructive.
- Use this to identify stale registry state when binding and filesystem are inconsistent.

## 15. Safety Boundaries

- Never `remove_dir_all` on paths outside the worktree pool.
- Never treat operator-created worktrees as daemon-managed.
- Never lease without validating the branch name.
- Never allow protected branches into lease.
- Never hard-delete without verifying `.agend-managed`.
- Never skip bind-in-flight cleanup.
- Release is explicit reclamation; GC is deferred collection; force_release is emergency recovery — they are not interchangeable.

## 16. Typical Workflow

1. New task: `repo action=checkout bind:true`.
2. Mid-lifecycle recovery: `bind_self`.
3. Ready to release: `release_worktree`.
4. Stale directory remains: `force_release_worktree`.
5. Preview candidates: `gc_dry_run`.
6. Execute collection: set `AGEND_WORKTREE_GC=1`, run cutover.
7. Preserve a worktree: `pin`.
8. Remove preservation: `unpin`.

## 17. Implementation Checklist

- Canonical path must be consistent across all code paths.
- New behaviors must preserve the daemon-managed marker.
- Release must never delete operator worktrees.
- GC must never scan worktrees with active bindings.
- GC grace must respect `released_at`.
- Legacy layout is detect-only, never create.
- Emergency cleanup must maintain path safety.
- `release_full` binding cleanup must not be skipped.
- Branch cleanup must check the protected set.
- Any new entry point must align with worktree pool semantics.

## 18. Summary

Worktrees are the isolation layer between agents and branches. `repo action=checkout bind:true` creates new bindings; `bind_self` rebinds mid-lifecycle; `release_worktree` releases formally; `force_release_worktree` handles emergencies; `repo action=release` releases by path. GC (`gc_dry_run` / `gc_cutover`) handles deferred reclamation. `.agend-managed`, `.agend-pinned`, and `binding.json` together describe the full state. When something goes wrong, determine whether the issue is in lease, release, or GC.