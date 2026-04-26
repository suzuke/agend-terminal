# Task Board UX Redesign Proposal

**Sprint 17 PR-AV** — Design phase only, 0 production code.

**Problem**: Operator doesn't use task board for active management — only glances at overview. Trust is low because updates lag (audit found 6/31 open tasks were actually done). Task board role mismatch: designed for agent workflow tracking, but operator wants a sprint progress dashboard.

**Source**: Operator TUI feedback 2026-04-26 + v1.2 Rule 3 audit results.

---

## Phase 1: Current State Audit

### What exists today

| Feature | Location | Status |
|---|---|---|
| Task CRUD (create/claim/done/update) | `src/tasks.rs` | Working |
| Status: open/claimed/done/blocked/cancelled | Task struct | Working |
| Priority: low/normal/high/urgent | Task struct | Working |
| Assignee + ownership checks | `can_mutate_task()` | Working |
| depends_on auto-block/unblock | `evaluate_dependency_status()` | Working |
| due_at + overdue auto-unclaim | `sweep_overdue_claimed()` | Working |
| Done TTL filter (14d default) | `handle()` list action | Working |
| TUI Task Board overlay (Ctrl+B t) | `render.rs` + `overlay.rs` | Working |
| TUI Fleet View (Tab toggle) | `render.rs` BoardView::Fleet | Working |
| TUI Status Summary (Ctrl+B s) | `status_summary.rs` BoardView::Status | Working (PR-AT) |
| In-progress assignee grouping | `render.rs` | Working |
| Telegram keyword status trigger | `telegram.rs` handle_message | Working (PR-AT) |

### Operator pain points (from feedback)

1. **Stale data** — tasks marked "done" by reviewer but task board still shows "claimed" or "open". Root cause: no auto-close on PR merge; relies on manual `task done` discipline.
2. **No PR↔task link** — operator can't see which PR corresponds to which task without reading task descriptions.
3. **Glance-unfriendly** — too many items, no sprint-level progress indicator, no "what's blocked right now" highlight.
4. **Adding backlog is friction** — requires `task create --title "..." --description "..." --priority ...` via MCP tool. Operator wants telegram shorthand.

---

## Phase 2: Design Proposal

### Constraint 1: Glance overview (sprint progress)

**Proposal**: Enhance `BoardView::Status` (PR-AT base) with:

- **Sprint progress bar**: `[████████░░] 8/10 tasks done` at top
- **Blocked items highlighted red** with blocker reason
- **In-flight PRs** with CI status indicator (reuse `ci-watches/` data)
- **Time-since-last-update** per task (stale > 2h gets ⚠️ marker)

**MVP**: Add progress bar + stale marker to existing `build_summary()`. ~30 lines in `status_summary.rs`.

**Hook point**: `build_summary()` already reads task board. Add `ci-watches/` scan for PR status.

### Constraint 2: Auto-close task on PR merge

**Proposal**: Three implementation options, recommend Option B:

**Option A — gh webhook**: Daemon opens HTTP endpoint, GitHub sends webhook on PR merge. Pros: real-time. Cons: needs public endpoint / tunnel, complex setup.

**Option B — Reuse watch_ci merge detection**: `ci_watch.rs` already detects PR terminal state via `check_pr_terminal()` (polls `pulls?state=all`). When a PR merges:
1. `ci_check_repo` already calls `check_pr_terminal` → gets `PrState::Terminal`
2. **New**: scan task board for tasks whose `result` or `description` contains the PR branch name or PR number
3. Auto-set `status=done` + `result="auto-closed: PR #{N} merged"`
4. Event log entry for audit trail

Pros: reuses existing infra, no new endpoint. Cons: polling delay (60s).

**Option C — `task done` in PR merge hook**: dev-lead's merge script calls `task done`. Pros: simple. Cons: manual, same discipline problem.

**Recommended**: Option B. The `task_id` field on `delegate_task` (Sprint 6 PR-C) already links tasks to dispatches. If PR branch name matches task ID pattern, auto-close is reliable.

**MVP**: ~50 lines in `ci_watch.rs` `ci_check_repo` + `tasks.rs` helper.

### Constraint 3: Claim/assign — keep but don't strengthen

**No change proposed**. Current `claim` + ownership checks work for dev team. Operator doesn't use them and shouldn't need to. The auto-close (Constraint 2) removes the main friction (manual `task done`).

### Constraint 4: Operator adds backlog via telegram

**Proposal**: Telegram keyword shorthand:

- `加 task: <title>` or `add task: <title>` → creates task with `priority=normal`, `assignee=null`, `status=open`
- Parsed in `handle_message` same as status keyword (exact prefix match)
- Confirmation reply: "✅ Task created: <title> [<id>]"

**MVP**: ~20 lines in `telegram.rs` handle_message + `tasks.rs` create helper.

**Boundary**: Only title. Description/priority/assignee set later via MCP tool or dev-lead dispatch.

---

## Phase 3: Implementation Priority

| Priority | Item | Effort | Depends on |
|---|---|---|---|
| P0 | Sprint progress bar in Status panel | S (~30 lines) | PR-AT merged ✓ |
| P0 | Stale task marker (>2h no update) | S (~10 lines) | — |
| P1 | Auto-close task on PR merge (Option B) | M (~50 lines) | watch_ci infra ✓ |
| P1 | Telegram "加 task:" shorthand | S (~20 lines) | — |
| P2 | In-flight PR + CI status in summary | S (~20 lines) | ci-watches scan |
| P3 | Blocked items red highlight in TUI | S (~10 lines) | — |

**Recommended MVP (1 PR)**: P0 items (progress bar + stale marker) — immediate operator value, minimal risk.

**Phase 2 PR**: P1 items (auto-close + telegram shorthand) — addresses root cause of stale data.

---

## Open Questions for Operator

1. **Progress bar scope**: Per-sprint or all-time? If per-sprint, how to define sprint boundary? (Suggestion: tasks created in last 7 days, or tagged with sprint name)
2. **Auto-close confidence**: Is branch-name matching sufficient, or should we require explicit `task_id` in PR body?
3. **Telegram task creation**: Should it auto-assign to dev-lead for triage, or leave unassigned?
4. **Stale threshold**: 2 hours too aggressive? Suggest 4h for claimed tasks, 24h for open tasks.
