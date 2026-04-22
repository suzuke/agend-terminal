# Fleet Development Protocol v1

**Status:** ACTIVE — all fleet agents must follow this protocol starting next work cycle.
**Supersedes:** ad-hoc prose conventions from Track 1 (2026-04-22).
**Informed by:** at-dev-2 (implementer feedback), at-dev-4 (reviewer feedback), friction log A1-C4.

## 1. Shared task board as single source of truth

**Use daemon `task` tool, NOT per-agent local TaskCreate.**

All work items visible to all agents via `task list`.

### Lifecycle

```
task create (orchestrator)
  → task claim (implementer)
    → task update --status blocked (if waiting)
    → task done --result "PR #N merged" (implementer)
```

### When to create tasks

| Event | Action |
|---|---|
| New PR planned | `task create --title "PR-1: set_waiting_on" --priority high --assignee at-dev-2 --depends_on []` |
| Review finding (REJECTED) | `task create --title "PR59-F2: anonymous caller gate" --priority high --assignee at-dev-2` |
| Follow-up identified | `task create --title "Followup: set_display_name anon gap" --priority low` |
| Design decision needed | Use `post_decision` instead (see §2) |

### Querying

```
task list                              # all open tasks
task list --filter_assignee at-dev-2   # my tasks
task list --filter_status blocked      # what's stuck
```

### Rules

- Orchestrator creates tasks. Implementer/reviewer update status.
- `depends_on` must be set when dependency exists (enables blocking graph).
- `task done` must include `--result` with PR number or summary.
- Never use Claude Code's internal TaskCreate for fleet-visible work.

## 2. Decisions panel for scope + corrections

**Use `post_decision` to freeze anything that defines scope or changes ground truth.**

### When to post decisions

| Event | Example |
|---|---|
| PR scope defined | `post_decision --title "Track1-PR2 scope" --tags track-1,pr-2 --content "§4.3 gate + §4.4 stale decay + §7 PR-2 tests"` |
| Scope intentionally narrowed | `post_decision --title "Stale decay deferred to PR-3" --tags track-1,pr-3 --content "..."` |
| Reviewer correction | `post_decision --title "PR59-F1 withdrawn" --tags track-1,pr-59 --content "inherited baseline, not PR-authored diff" --supersedes d-xxx` |
| Design choice with trade-offs | `post_decision --scope fleet --title "Heartbeat threshold 120s" --tags track-1 --content "rationale: ..."` |

### Rules

- `tags` must include track name + PR number for filterability.
- `scope: fleet` for cross-track decisions; `scope: project` for track-specific.
- `ttl_days: 30` default (decisions auto-archive; can be refreshed).
- Reviewer should trust latest `post_decision` over reconstructing intent from multiple artifacts.
- `supersedes` field links corrections to original decision.

## 3. Review cycle protocol (Reviewer Contract v1.1)

Extends Reviewer Contract v0.1 with structured tooling.

### Pre-implementation

1. Orchestrator posts **scope decision** per PR:
   ```
   post_decision --title "Track1-PR1 scope"
     --tags track-1,pr-1,scope
     --content "§4.1 MCP tool + §4.2 heartbeat + §7 PR-1 tests. Drive-by: atomic save_metadata."
   ```
2. Orchestrator creates **task** for the PR:
   ```
   task create --title "PR-1: set_waiting_on + heartbeat"
     --assignee at-dev-2 --priority high
     --description "Scope decision: d-xxx"
   ```

### On review dispatch

Every review/re-review handoff includes a **3-part contract**:
1. **Source of truth:** design doc sections OR decision ID
2. **Scope boundary:** "audit X, ignore Y"
3. **Freshness boundary:** "stale if files/commits change after {sha}"

### On rejection

1. Orchestrator creates **one task per finding**:
   ```
   task create --title "PR59-F2: anonymous caller gate"
     --assignee at-dev-2 --priority high
     --description "set_waiting_on accepts empty instance_name → metadata/.json"
   ```
2. Re-review dispatch references **task IDs**, not prose:
   ```
   "Audit task T-5 only. Scope: gate + negative pin. Do not broaden."
   ```

### On reviewer correction/withdrawal

Post a decision:
```
post_decision --title "PR59-F1 withdrawn"
  --tags track-1,pr-59,review-correction
  --content "inject_provenance is inherited baseline (origin/main), not PR-authored."
  --supersedes d-xxx
```

### Verdict wording (unchanged from v0.1)

`VERIFIED` / `REJECTED` / `UNVERIFIED`

### Metadata fields (v1.1 addition)

Add to every review report:
- `scope_source`: decision ID or design doc section that defined scope
- `audit_mode`: `full_review` | `finding_reaudit` | `scope_conformance`

## 4. Communication rules

### Hop reduction

Target: implementer → orchestrator → reviewer → orchestrator → implementer (4 hops)
→ reduce where possible.

**Auto-merge on VERIFIED:**
When orchestrator dispatches review, include: "If VERIFIED → I will auto-merge. No need to wait for my ack."

This eliminates 1 hop (reviewer → orchestrator → merge → notify).

### Ack absorption

- `requires_reply: false` on status updates and notifications.
- Pure ack messages ("收到", "OK", "👍") → do NOT reply. Break chain.
- Only reply when there's new information to add.

### Message semantics

| `request_kind` | When | Expects reply? |
|---|---|---|
| `task` | delegation, review dispatch | yes |
| `report` | result, verdict, status update | depends on content |
| `update` | FYI, notification | no |
| `query` | question, discussion | yes |

## 5. CI integration

### Use `watch_ci` instead of manual polling

```
watch_ci --repo suzuke/agend-terminal --branch feat/my-branch
```

Daemon auto-injects CI failure logs. No manual `gh pr checks --watch`.

### After PR merge

Implementer cleans up:
```bash
git worktree remove /path/to/worktree
git branch -D feat/branch-name
git fetch origin main && git checkout main && git pull --ff-only
```

(Future: daemon auto-cleanup on merge detect — tracked as enhancement.)

## 6. Progress visibility for operator

### What gets emitted to Telegram fleet binding

Level **(a) task state changes** (per at-dev-2/at-dev-4 consensus):

| Event | Example notification |
|---|---|
| Task created | `[task] #5 created: "PR-1 set_waiting_on" → at-dev-2` |
| Task claimed | `[task] #5 claimed by at-dev-2` |
| Task blocked | `[task] #5 blocked: waiting on PR-1 review` |
| Task done | `[task] #5 done: PR #59 merged` |
| Review verdict | `[review] PR #59: VERIFIED by at-dev-4` |
| Decision posted | `[decision] "Track1-PR2 scope" posted` |

### Operator queries (via Telegram or TUI)

- "進度？" → orchestrator runs `task list` + `list_decisions --tags current-track` and summarizes.
- Active task count + blocked count + done count gives instant pulse.

## 7. Waiting and timeout

### Declaring wait state

When blocked on another agent, CI, or external event:

```
set_waiting_on --condition "review from at-dev-4 on PR #63"
```

- Automatically cleared after 120s of no MCP activity (stale decay).
- Visible via `describe_instance` and `list_instances` to all agents and operator.
- Orchestrator can query `list_instances` to see who's waiting on what.

### Scheduling check-ins (cross-backend)

**Do NOT rely on backend-specific mechanisms** (e.g., Claude Code `ScheduleWakeup`).
Use daemon-level scheduling, which works for all backends:

```
create_schedule --target general
  --message "Check inbox: at-dev-2 should have finished PR-1 by now"
  --run_at "2026-04-22T21:00:00+08:00"
  --label "pr1-check"
```

One-shot schedules auto-disable after firing. Use for:
- Timeout checks on delegated tasks
- Periodic progress polling during long operations
- Reminder to follow up on review verdicts

### Timeout policy

| Elapsed since dispatch | Action |
|---|---|
| < 20 min | Normal. Check `describe_instance` — `last_heartbeat` fresh = agent active. |
| 20 min, agent `last_heartbeat` fresh | Agent is working. Extend wait. |
| 20 min, agent `last_heartbeat` stale (> 120s) | **Ping to verify liveness.** `send_to_instance` with a direct question. |
| 20 min, no response to ping | **Escalate.** `replace_instance` and re-dispatch task. |
| Agent state `permission` + heartbeat fresh | Heartbeat gate suppresses false positive (A5 fix). Trust heartbeat. |
| Agent state `permission` + heartbeat stale | May be genuinely stuck. Ping first, then escalate. |

### Liveness check procedure

```
# Step 1: check heartbeat
describe_instance --name at-dev-2
# → last_heartbeat: "2026-04-22T12:55:00Z" (< 120s ago = fresh)

# Step 2: if stale, ping
send_to_instance --instance_name at-dev-2
  --message "Status check: are you still working on task t-xxx?"
  --requires_reply true

# Step 3: if no reply within 5 min → replace
replace_instance --name at-dev-2 --reason "unresponsive after timeout"
```

### After task completion

1. Implementer: `report_result` → `task done --result "PR #N merged"`
2. Orchestrator: picks up from inbox (or scheduled check-in fires)
3. Clean up: `delete_schedule --id <check-in-schedule-id>` if one was set

## 8. Git workflow

### Worktree rules (from CLAUDE.md, reinforced)

- **Never** commit directly to main. Always use worktree + branch.
- `docs-skip-PR` means skip review ceremony, NOT skip branch isolation.
- Branch naming: `feat/`, `fix/`, `docs/` prefix per change type.

### After PR merge

Clean up immediately. Don't accumulate stale worktrees.

## 9. Tool usage quick reference

| Need | Tool | NOT this |
|---|---|---|
| Track work items | `task create/list/claim/done` | Claude Code TaskCreate |
| Record decisions | `post_decision` | Markdown files |
| Assign work | `delegate_task` (rich context) + `task create` (persistent) | Only one of them |
| Report results | `report_result` | Free-text send_to_instance |
| Watch CI | `watch_ci` | Manual `gh pr checks` |
| Declare wait state | `set_waiting_on` | Prose in messages |
| Check agent health | `describe_instance` (has `last_heartbeat`) | Guessing from pane |
| Schedule check-in | `create_schedule` (one-shot `--run_at`) | Backend-specific ScheduleWakeup |
| Timeout escalation | `replace_instance` (after ping fails) | Silently waiting forever |

## 10. Protocol changelog

| Version | Date | Changes |
|---|---|---|
| v1.1 | 2026-04-22 | Added §7 Waiting and timeout: `set_waiting_on` usage, `create_schedule` for cross-backend check-ins, timeout policy (20min threshold), liveness check procedure, escalation rules. Added `create_schedule` + `replace_instance` to tool reference. |
| v1.0 | 2026-04-22 | Initial protocol. Integrates Reviewer Contract v0.1 → v1.1, adds task board + decisions panel usage, hop reduction, CI integration. |
