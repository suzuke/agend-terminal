[繁體中文](FEATURE-worktree.zh-TW.md)

# Worktree Management

AgEnD isolates branch-carrying work in daemon-managed Git worktrees. This document describes the current provision, binding, guarded release, and automatic-release behavior.

## Usage Scenarios

> **Target audience:** agent infrastructure. Agents normally receive a bound worktree from task dispatch and use ordinary Git inside it.

- **Fresh branch task:** dispatch with a branch, or call `repo action=checkout` with `bind:true`, to provision and bind a dedicated worktree.
- **Recovery or rebind:** call `bind_self` when an existing agent needs to recover or re-establish its own binding.
- **Normal cleanup:** call `release_worktree` after work is safe to reclaim.
- **Path-oriented cleanup:** call `repo action=release` for a known linked-worktree path.
- **Automatic cleanup:** terminal PR/task lifecycle events enqueue a durable release intent; the daemon releases only when the current lease still satisfies the release invariant.

## State and Layout

Branch-dispatch worktrees use the canonical pool layout:

```text
$AGEND_HOME/worktrees/<instance>/<branch>/
```

An explicit `repo action=checkout` currently uses a stable single-level
`<instance>-<repository-key>` directory under `$AGEND_HOME/worktrees/`. Treat
the `worktree` path returned by the daemon and `binding_state` as authoritative;
callers must not derive or hard-code either layout.

Two pieces of state establish authority:

- `.agend-managed` inside the worktree proves daemon ownership.
- `runtime/<instance>/binding.json` records the instance, branch, source repository, worktree path, task, and lease identity.

Legacy layouts may be recognized for recovery, but new worktrees use the canonical layout. An operator-created worktree without the managed marker is never treated as a normal daemon-owned release target.

## Provision and Bind

### Fresh work: `repo action=checkout`

```json
{
  "tool": "repo",
  "action": "checkout",
  "repository_path": "/path/to/repo",
  "branch": "feature/example",
  "bind": true
}
```

With `bind:true`, AgEnD provisions the branch worktree and writes the marker and binding as one lifecycle operation. Protected branches are rejected. This is the preferred entry point for a fresh task when the source-repository path is known.

With `bind:false`, checkout creates an unbound inspection worktree. It is not a dispatch lease.

### Disposable review checkout

For a full-tree review, use typed disposable provenance instead of leaving an
ordinary review branch behind:

```json
{
  "tool": "repo",
  "action": "checkout",
  "repository_path": "/path/to/repo",
  "branch": "review/2819-r0",
  "from_ref": "<full subject-head SHA>",
  "expected_head": "<same full subject-head SHA>",
  "bind": true,
  "task_id": "t-...",
  "checkout_purpose": "disposable_review"
}
```

`disposable_review` is accepted only for a branch that this checkout proves is
new both locally and on `origin`. The initial signed binding records
`DaemonProvisionedReview` provenance and the exact `provisioned_head`; there is
no later metadata-upgrade window.

Release may delete the clean review branch after its matching review task is
terminal, no other binding holds it, no PR is headed by the review branch, and
the actual tip still equals `provisioned_head`. The subject PR may remain open.
Dirty work, divergence, missing/corrupt provenance, unknown task state, or an
unproven remote branch state fails closed and preserves the branch.

### Recovery: `bind_self`

```json
{
  "tool": "bind_self",
  "repository_path": "/path/to/repo",
  "branch": "feature/example",
  "task_id": "t-...",
  "rebase_mode": true
}
```

`bind_self` binds the calling instance only. Use it for recovery, a safe rebind, or when the repository is resolved from fleet configuration. `repository_path` and the legacy `repository` argument are mutually exclusive; prefer `repository_path`.

`rebase_mode:true` first runs the guarded repair/rebind path. It does not authorize overwriting another live lease.

Neither `bind_self` nor a self-claimed checkout silently invents a CI continuation. CI watch is armed from an actual branch-carrying task dispatch or by an explicit `ci action=watch` call.

## Binding Guarantees

- Branch names and repository paths are validated before creation.
- Protected branches cannot become ordinary agent leases.
- A branch already leased to another agent is a conflict, not an implicit takeover.
- Binding and lifecycle operations are serialized so bind, rebase, and release cannot race freely.
- Fresh dispatch should carry `task_id`; automatic lifecycle release applies only to dispatch leases with a non-empty task ID.

Use `binding_state` to inspect the authoritative binding before acting on a worktree.

## Normal Release

```json
{
  "tool": "release_worktree",
  "instance": "dev-agent"
}
```

The current MCP release is a guarded hard release; there is no normal 24-hour “soft release” phase.

The release transaction:

1. acquires the lifecycle permit;
2. snapshots the guarded binding;
3. reacquires the branch, agent, and binding locks;
4. confirms the fresh binding fingerprint still matches the snapshot;
5. preserves dirty work before removal;
6. verifies the managed marker and exact target;
7. removes the linked worktree, prunes Git metadata, and clears the matching binding.

If preservation fails, release fails closed and keeps the binding. A changed lease fingerprint also stops the operation. A second call after a successful release is an idempotent success.

Use `dry_run:true` to preview the operation without destructive effects.

The removal implementation first asks Git to remove the worktree. A filesystem fallback is permitted only after the target has been verified as the daemon-managed worktree selected by the guarded transaction.

## Emergency Release

`release_worktree(force:true)` is the guarded recovery path that absorbed the former standalone force-release tool.

```json
{
  "tool": "release_worktree",
  "instance": "dev-agent",
  "branch": "feature/example",
  "force": true,
  "repository_path": "/path/to/repo"
}
```

- `branch` is required.
- The caller must be the owning instance, its team orchestrator, or an operator.
- The target must resolve below the daemon worktree pool.
- Markerless, opaque, ambiguous, ownerless, or mismatched state is preserved rather than guessed away.
- `repository_path` is an optional hint for Git metadata cleanup.

Force is for stale-state recovery, not a shortcut around the normal release checks.

## `repo action=release`

`repo action=release` accepts a worktree path and revalidates it at execution time.

- A daemon-managed target is delegated to the canonical guarded release using its marker owner, binding fingerprint, exact-path check, and caller authorization.
- An unmanaged target is eligible only when Git proves that it is a linked, non-bare worktree.
- Main worktrees, bare repositories, non-repositories, shallow/system paths, stale managed markers, and ambiguous targets are rejected.
- The direct removal fallback is used only after the unmanaged target is revalidated as that exact linked worktree.

This action is path-oriented, but it is not an unrestricted directory deletion API.

## Automatic Release

Merge, close, task completion, and qualifying reviewer-verdict events enqueue disk-backed recompute intents. The worker processes an intent only when it contains a dispatch lease with a task ID and the live lease still matches the captured identity, including source repository, branch, path, and issue time.

The core release invariant is:

```text
terminal PR
OR
positively confirmed no PR AND all matching repository/branch tasks are terminal
```

An open PR or unknown PR state keeps the worktree. Close-without-merge follows its conservative grace rules. Cross-repository evidence cannot satisfy another repository's lease.

Automatic release also:

- honors worktree release opt-out;
- preserves dirty WIP before releasing an eligible terminal lease and retries if preservation fails;
- keeps non-terminal or otherwise ineligible intents for later recomputation;
- compares the exact lease fingerprint immediately before release so a re-lease cannot be deleted;
- permits the narrow reviewer-cleanup path only for eligible clean reviewer bindings after a terminal review task or verdict.

A typed `disposable_review` binding uses the stricter provenance path described
above. Its subject PR is not the branch-lifecycle signal; terminal review-task
state plus exact provenance/tip/occupancy checks are.

The PR-state notification and release worker are decoupled; consumers must not depend on release occurring immediately before a `pr-merged` message.

## Branch Cleanup

Worktree removal and local branch deletion are separate decisions. Branch cleanup skips protected branches and fails closed when merge or lifecycle evidence is insufficient. Cleanup intent may be recorded for a clean branch that cannot yet be deleted.

## Operational Checklist

1. Start fresh work with a branch-carrying dispatch or `repo action=checkout bind:true`.
2. Confirm the daemon-reported worktree and work inside that directory.
3. For a full-tree review, add `checkout_purpose:"disposable_review"`, exact `expected_head`, and the review `task_id` on a new branch.
4. Use `bind_self` only for recovery/rebinding.
5. Inspect uncertain state with `binding_state` or `release_worktree dry_run:true`.
6. Use normal `release_worktree` when cleanup is authorized.
7. Reserve `force:true` for guarded recovery and supply the exact branch.
8. Never manually delete a managed directory while its binding is live.

## Source Pointers

- `src/mcp/handlers/worktree.rs` — `bind_self` and `release_worktree`
- `src/mcp/handlers/ci/release.rs` — `repo action=release`
- `src/mcp/handlers/ci/checkout_disposable.rs` — typed disposable-review admission
- `src/worktree.rs` — canonical dispatch-worktree path derivation
- `src/worktree_pool.rs` — lease, guarded release, WIP preservation, and branch cleanup
- `src/daemon/auto_release.rs` — durable auto-release intents and release invariant
- `src/binding.rs` — binding records and guarded fingerprints
