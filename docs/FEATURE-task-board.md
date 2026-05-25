# Task Board

The task board is the fleet's shared work-tracking surface. It is built on an
event-sourced model: every mutation is appended to `task_events.jsonl`, and the
current state is reconstructed by replaying (folding) those events. This gives
the board a complete audit trail, makes state reproducible, and provides the
foundation for sweep, health, and dependency evaluation.

## Usage Scenarios

> **Target audience:** Agent infrastructure — agents use this via MCP tools; operators typically don't interact directly.

**Task dispatch and tracking.** A lead agent creates a task on the board with a title, priority, and assignee, then dispatches it to a dev agent. The dev receives the task in its inbox, claims it via `task action=claim`, and begins work. As the dev progresses, it updates the task status to `in_progress`. When the work is complete, the dev marks it `done` with a result summary. The entire lifecycle is recorded as append-only events in `task_events.jsonl`.

**Cross-agent visibility.** A reviewer agent checks the task board to see which tasks are marked `done` and ready for verification. After reviewing the associated PR, the reviewer marks the task as `verified`. Meanwhile, the operator can observe the full board state at any time through the TUI without needing to query individual agents.

**Board hygiene.** Over time, tasks accumulate — some with owners who no longer exist in the fleet (ghost owners), others with expired deadlines. The operator runs `task action=health` to get a snapshot of board health, then uses `task action=sweep` in dry-run mode to identify candidates for cleanup before applying changes.

## 1. Design Rationale

- The task board is a single source of truth shared by every agent.
- Leads use it to dispatch work; agents claim and execute; reviewers verify.
- Operators can observe the global state at any time.
- It is an append-only, replayable state machine — not a mutable JSON file.
- `task_events.jsonl` is the canonical source; `tasks.json` is a legacy bridge.
- All new reads go through replay-fold; all new writes go through event append.

## 2. Files and Modules

- `src/task_events.rs` — event format definition and replay logic.
- `src/tasks.rs` — MCP board surface and legacy data bridging.
- `src/mcp/handlers/task.rs` — thin MCP handler that delegates to `tasks.rs`.
- `task_events.jsonl` — canonical event log.
- `task_events_archive/` — historical archive directory.
- `tasks.json` — legacy bridge file (read-only during migration).
- `TaskBoardState` — the output of replay-fold.
- `TaskRecord` — canonical read model from replay.
- `TaskEvent` — the write model.
- `TaskId` and `InstanceName` are newtypes to prevent ID mix-ups.
- Replay fails on unknown event variants or unsupported schema versions (fail-closed).
- Append uses a monotonic per-emitter sequence number for ordering.

## 3. Data Model

- `Task` is the public structure exposed via MCP (uses string status for compatibility).
- `TaskRecord` is the canonical view from replay (uses enum status).
- Key `TaskEvent` variants:
  - `Created` — carries title, description, priority, optional owner/due_at/depends_on/routed_to/branch/bind/eta_secs.
  - `Claimed` — sets the owner.
  - `InProgress` — sets the owner and marks work as started.
  - `Done` — transitions to Done status.
  - `Cancelled` — transitions to Cancelled status.
  - `Blocked` / `Unblocked` — sets or clears the block reason.
  - `Released` — clears owner and routed_to.
  - `Reopened` — re-opens a completed task (preserves owner).
  - `OwnerAssigned` — changes owner/routed_to only.
  - `PriorityChanged` — changes priority only.
  - `Verified` — marks reviewer approval without closing.
  - `Linked` — appends a PR link.
  - `TaskCloseProposed` — review-gated close proposal.

## 4. Status Semantics

| Status | Meaning |
|--------|---------|
| `open` | Awaiting claim |
| `claimed` | Someone has taken ownership |
| `in_progress` | Execution has started |
| `verified` | Reviewer approved |
| `done` | Completed |
| `cancelled` | Cancelled |
| `blocked` | Blocked by an external factor or dependency |

### Dependency Evaluation

- `depends_on` affects the view-layer effective status.
- An open task whose dependencies are incomplete appears as blocked.
- Once dependencies complete, the task reverts to open automatically.
- Claimed / done / cancelled tasks are never overridden by dependencies.
- This evaluation is in-memory only — it does not emit Blocked/Unblocked events.
- Circular or missing dependencies are treated as incomplete.
- Claiming respects the post-dependency view (you cannot claim a dependency-blocked task).
- `started_at` is set once on the first transition to in_progress.

## 5. `task action=create`

- `title` is required; `description`, `priority`, `assignee`, `depends_on`, `due_at`, `duration`, `branch`, `bind`, `eta_secs` are optional.
- Default priority is `normal`.
- `due_at` accepts RFC 3339; `duration` accepts `30m`, `1h`, `2d` shorthand.
- Appends a `Created` event and returns `event=created`.
- Does not auto-claim, auto-start, or auto-complete.

## 6. `task action=list`

- Default view shows only actionable tasks: open, claimed, in_progress, blocked.
- `include_history=true` includes completed items.
- `filter_status` and `filter_assignee` narrow the results.
- `limit` truncates by `updated_at` (newest first).
- Items completed more than 14 days ago are trimmed from the default view.
- `filtered_default` in the response indicates whether default trimming was applied.
- List is a pure read — it does not mutate the board.

## 7. `task action=claim`

- Requires `id`.
- Validates that the calling instance exists in fleet.yaml.
- Respects dependency evaluation — dependency-blocked tasks cannot be claimed.
- Self-reclaim (re-claiming your own task) is allowed.
- Appends a `Claimed` event and sets the caller as owner.
- Clears `routed_to` to reflect that ownership has transferred.

## 8. `task action=done`

- Requires `id`; optional `result` and `done_source`.
- Uses the task owner as `by` (falls back to caller if no owner).
- `force=true` enables ghost-owner cleanup and requires `force_reason`.
- Force mode records an audit entry in the event log.
- Appends a `Done` event.
- After completion, attempts best-effort cleanup of the bound worktree's init commit.

## 9. `task action=update`

- Requires `id`; can change `status`, `priority`, `assignee`.
- Status transitions map to canonical events:
  - open → claimed: `Claimed`
  - open → in_progress: `InProgress`
  - any → done: `Done`
  - any → cancelled: `Cancelled`
  - any → blocked: `Blocked`
  - blocked → open: `Unblocked`
  - claimed/in_progress → open: `Released`
  - done → open: `Reopened`
- Multiple changes can be batched in a single `append_batch` for atomic persistence.
- ACL rules match those of `done` (owner / orchestrator).

## 10. `task action=sweep`

- A board hygiene tool, not an always-on enforcer.
- Default is dry-run (`apply=false`).
- `apply=true` requires `confirm_ids` from a prior dry-run.
- Processes stale tasks and tasks whose linked PR has been closed.
- Cancelled tasks are emitted in batch with an audit reason.

## 11. `task action=health`

- Returns a read-only board snapshot: totals, by_status breakdown, ghost_owners, stale_claims, age aggregates, and recommendations.
- Ghost owners: **strict** (not in fleet or live registry) vs. **soft** (in fleet but not live).
- Stale claims: claimed tasks past their `due_at`.
- Age statistics cover non-terminal tasks only.
- Recommendations are operator-facing next-step hints.

## 12. Event Recording and Migration

- Append acquires a lock before writing; `append_batch` fsyncs multiple events atomically.
- Replay folds the archive first, then the hot log. It is a strict reader — unknown variants or higher-version schemas cause an abort (fail-closed).
- Legacy `tasks.json` is consumed only during migration, which converts old tasks into events and renames the file to `.legacy_pre_v2`.

## 13. ACL and Permissions

- Unassigned tasks can be mutated by any agent.
- The task owner and their team orchestrator can mutate.
- System identities (`system:auto_orphan`, `system:task_sweep`, etc.) bypass ACL.
- `force` mode is for historical cleanup, not a shortcut — it requires a reason.
- ACL is evaluated on the replay snapshot (small TOCTOU window; canonical truth is the event log).

## 14. Interactions with Other Subsystems

- **Teams** — affect assignee resolution.
- **Worktree / Binding** — `done` triggers best-effort worktree cleanup.
- **Dispatch** — creates task-to-branch associations.
- **CI Watch** — may mark tasks as done when a PR merges.
- **Inbox** — carries task-related notifications.
- **Health** — uses the board as an operator snapshot.
- **Sweep** — often reviewed alongside CI sweep.

## 15. Usage Guide

- Always provide `title` when creating.
- Use `branch` for tracking branches, `eta_secs` for stall watchdog, `depends_on` for sequencing, `assignee` for routing.
- Use `task action=list` for the full board; add `filter_assignee` for a personal view.
- Use `task action=health` to check for stuck or orphaned tasks.
- Use `task action=sweep` with dry-run first, then apply.
- Reserve `force=true` for historical data cleanup only.
- Prefer `done` events over plain-text result reports for traceability.

## 16. Implementation Checklist

- Any new event variant must update the replay fold.
- Any new status must update list/health projections.
- All writes must respect `append_batch` atomicity.
- New actions must update the MCP schema.
- ACL changes must include tests.
- Migrations must remain idempotent.
- Board operations must never silently swallow errors.
- New report-only features should use the read model.
- New write paths should go through `task_events`.
- New sweep rules should also be reflected in health.

## 17. Summary

The task board is the fleet's shared work protocol. Its semantics are maintained through events, not a single mutable file. The primary surface is `task create/list/claim/done/update/sweep/health`. Default listing is actionable-only. Dependencies are evaluated at the view layer. ACL is owner / orchestrator / system identity. Batch append and strict replay are the two most important invariants. When something looks wrong, check the event log before touching the view.
