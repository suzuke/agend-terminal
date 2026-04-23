# Design: Per-Agent Worktree + Per-Task Claim Hook for Git Isolation

**Status:** DRAFT v2
**Task:** t-20260423051848
**Author:** at-dev-b-kiro
**Reviewers:** at-dev-b-gemini (cross-backend), general (scope gate)
**Motivation:** PR #105 accidentally based off another agent's in-flight branch instead of `origin/main`, causing cross-contamination. All agents currently share the main working tree, making this class of error inevitable.

## Problem Statement

When multiple agents work on the same repo, they share a single git working tree. This causes:

1. **Branch contamination:** Agent A creates a branch from HEAD, but HEAD includes Agent B's uncommitted or in-flight changes
2. **File conflicts:** Two agents editing the same file simultaneously corrupt each other's work
3. **Stale state:** An agent's `git status` reflects another agent's staged changes
4. **No isolation on `task claim`:** Claiming a task doesn't set up a clean git context

## Design Overview

Two layers, independently deployable:

- **Layer 1: Per-agent worktree on spawn** — each agent gets its own git worktree at creation time
- **Layer 2: Per-task claim hook** — `task claim` auto-creates a feature branch in the agent's worktree

## Layer 1: Per-Agent Worktree on Spawn

### Current Behavior

`create_instance` with `working_directory` pointing to a git repo + `branch` parameter already calls `worktree::create()`, which creates `{repo}/.worktrees/{instance_name}/`. Without `branch`, the agent shares the repo's working tree directly.

### Proposed Change

When `create_instance` resolves a `working_directory` that is a git repo, **always** create a worktree — even without an explicit `branch` parameter. The default branch becomes `agend/{instance_name}` (matching existing `worktree::create` default).

```
# Before (current): agents share the repo working tree
create_instance name=dev-1 working_directory=/path/to/repo
→ agent works in /path/to/repo (shared!)

# After: each agent gets its own worktree
create_instance name=dev-1 working_directory=/path/to/repo
→ agent works in /path/to/repo/.worktrees/dev-1/
→ branch: agend/dev-1 (based on current HEAD)
```

### Opt-out

New fleet.yaml field `worktree: false` per instance to disable auto-worktree for agents that intentionally share the repo (e.g., read-only monitors).

```yaml
instances:
  dev-1:
    backend: claude
    working_directory: /path/to/repo
    # worktree: true is the default when working_directory is a git repo
  monitor:
    backend: claude
    working_directory: /path/to/repo
    worktree: false  # shares repo directly
```

### Implementation Points

1. **`spawn_single_instance` (mcp/handlers.rs:843):** After resolving `work_dir`, check `is_git_repo(&wd)`. If true and `worktree != false`, call `worktree::create(&wd, name, branch)`. Use the worktree path as the actual working directory.

2. **Team spawn (mcp/handlers.rs:390):** Same logic per team member. Each member gets `{repo}/.worktrees/{team}-{N}/`.

3. **`fleet.yaml` persistence:** Store the worktree path (not the source repo) as `working_directory`. Add `source_repo` field for cleanup/rebuild.

4. **`cleanup_working_dir` (agent_ops.rs:134):** Already handles worktree paths under `.worktrees/` — `remove_dir_all` works. Add `git worktree remove` call before directory removal to keep git's worktree registry clean. Also delete the `agend/{name}` branch to prevent branch bloat in the source repo.

### Lifecycle

```
create_instance
  → is_git_repo(working_directory)?
    → yes + worktree != false:
        worktree::create(repo, name, branch)
        actual_wd = {repo}/.worktrees/{name}/
    → no or worktree == false:
        actual_wd = working_directory (current behavior)
  → spawn agent with actual_wd

delete_instance
  → cleanup_working_dir (existing)
  → if worktree: git worktree remove + prune

replace_instance
  → delete old worktree
  → create fresh worktree (clean state)
```

## Layer 2: Per-Task Claim Hook

### Decision: DEFERRED to future iteration

After cross-backend review, Layer 2 adds marginal value over agents manually running `git checkout -b`:
- Agents that follow protocol will create branches anyway
- Agents that don't follow protocol can bypass the hook
- The enforcement value is low; the convenience value doesn't justify the complexity
- Branch naming (`task/{task_id}`) conflicts with practical PR conventions (`fix/...`, `feat/...`)

**Recommendation:** Ship Layer 1 only. Revisit Layer 2 if agents consistently fail to create branches.

## Fleet.yaml Schema Changes

```yaml
instances:
  dev-1:
    backend: claude
    working_directory: /path/to/repo/.worktrees/dev-1  # resolved path
    source_repo: /path/to/repo                          # NEW: for rebuild/cleanup
    worktree: true                                      # NEW: default true for git repos
    branch: agend/dev-1                                 # existing field
```

## Edge Cases

### Corrupted Worktree

If `git worktree add` fails (e.g., lock file left by crashed process):
1. Run `git worktree prune` on the source repo (already implemented)
2. Remove the stale directory
3. Retry `worktree::create`

`worktree::create` already handles the "already exists" case by reusing.

### Disk Full

`worktree::create` returns `None` on failure. The agent falls back to the source repo directory (degraded mode, logged as warning). This matches current behavior where worktree creation failure is non-fatal.

### Agent Crash Without Cleanup

On daemon restart, `worktree::list_residual` already lists orphaned worktrees. Add a reconciliation step in bootstrap:
- For each residual worktree not matching a live instance → `git worktree remove`

### Multiple Agents, Same Source Repo

Each agent gets its own worktree under `.worktrees/`. Git worktrees are designed for this — they share the object store but have independent working trees and index files. No locking issues.

### Concurrent Worktree Creation

Two agents spawning simultaneously against the same repo could race on `git worktree add`. Git uses a lock file (`$GIT_DIR/worktrees/{name}.lock`) internally, so concurrent `worktree add` with different names is safe. Same-name races are handled by `worktree::create`'s "already exists → reuse" fallback.

### Non-Git Working Directories

No change. Agents with non-git working directories continue to work as-is.

## Disk Estimation

A git worktree is lightweight — it shares the object store with the source repo. The overhead per worktree is:
- Working tree files (same as a checkout): ~size of repo content
- `.git` file (pointer): ~100 bytes
- Index file: ~proportional to number of tracked files

For a typical project (100MB repo content, 10 agents): ~1GB total worktree disk usage.

### Build Artifact Concern (Rust `target/`)

Each worktree gets its own `target/` directory by default. For Rust projects, 10 agents × `cargo build` = 10× `target/` (~2-5GB each = 20-50GB total). Mitigations:

1. **Shared `CARGO_TARGET_DIR`:** Set `CARGO_TARGET_DIR` env var to a shared location. Cargo handles concurrent builds via file locks. Simplest solution.
2. **sccache:** Shared compilation cache across worktrees. Reduces rebuild time but doesn't eliminate disk usage.
3. **Symlink `target/`:** Each worktree symlinks `target/` to a shared directory. Fragile — concurrent builds may conflict.

**Recommendation:** Set `CARGO_TARGET_DIR=$AGEND_HOME/shared-target` in agent environment. Add to `instructions::generate()` as an env var hint.

## Migration Path

### Existing Agents (no worktree)

No breaking change. Existing agents without worktrees continue to work. The auto-worktree behavior only activates for NEW instances created after the feature ships.

### Opt-in for Existing Agents

`replace_instance` with the same name triggers the new worktree creation path. Alternatively, a new command `migrate_to_worktree` could be added, but `replace_instance` is simpler and already handles the lifecycle.

### Fleet.yaml Backward Compatibility

The new `source_repo` and `worktree` fields are optional. Existing fleet.yaml files without these fields work unchanged.

## Implementation Plan (for t-20260423051857)

1. **PR A:** Add `source_repo` and `worktree` fields to `InstanceYamlEntry`. Update `spawn_single_instance` to auto-create worktrees. Update `cleanup_working_dir` to call `git worktree remove` + delete branch.
2. **PR B:** Bootstrap reconciliation — prune orphaned worktrees on startup.
3. **PR C (future):** Task claim hook — deferred per review feedback.
