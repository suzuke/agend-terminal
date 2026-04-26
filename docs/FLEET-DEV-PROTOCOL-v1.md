# Fleet Development Protocol v1.2

**Status:** ACTIVE — all fleet agents must follow this protocol.
**Version history:** v1.0 (2026-04-22), v1.1 (2026-04-23), v1.2 (2026-04-26).
**Informed by:** implementer feedback, reviewer feedback, operator observations, 4-perspective challenge round.

## 1. Shared task board as single source of truth

**Use daemon `task` tool, NOT per-agent local TaskCreate.**

All work items visible to all agents via `task list`.

### Lifecycle

```
task create (orchestrator)
  → task claim (implementer)
    → task update --status in_progress (implementer, on PR push)
    → task update --status blocked (if waiting)
    → task update --status verified (reviewer, on VERIFIED verdict)
    → task done --result "PR #N merged" (dev-lead, on merge)
```

**Three-state completion model (v1.2):** `in_progress` → `verified` → `done`.
See §10.3 for full rules and edge cases.

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

### Metadata fields (v1.1 addition, extended v1.2)

Add to every review report:
- `scope_source`: decision ID or design doc section that defined scope
- `audit_mode`: `full_review` | `finding_reaudit` | `scope_conformance`
- `reviewed_head`: git SHA at time of review (v1.2: snapshot, not contract — any subsequent commit resets verdict state)
- `commands`: verification commands run (e.g. `cargo test --features tray`)
- `files`: files audited

**VERIFIED is an audit trail, not a quality guarantee.** The verdict records what was checked at `reviewed_head`; it does not promise the code is bug-free. This framing prevents retroactive blame when post-merge issues surface.

### Re-review dispatch template (v1.2)

When dispatching r2 (re-review after REJECTED), the dispatch must enumerate r1 findings with status:

```
r1 findings:
- F1: <description> → fixed (commit abc1234)
- F2: <description> → deferred (tracked as task t-xxx)
- F3: <description> → withdrawn (decision d-xxx)
```

If r1 findings status is missing, reviewer falls back to `audit_mode: full_review`.


### 3.5 Multi-reviewer dispatch

Multi-reviewer support exists to reduce reviewer bottlenecks without weakening Reviewer Contract v1.1. The default remains **one accountable reviewer per PR**.

#### Allocation

The orchestrator assigns a review to one primary reviewer using load-based routing first, with round-robin fallback.

Load-based routing checks, in order:

1. Open or claimed review tasks on the shared task board.
2. The reviewer instance `waiting_on` state.
3. Recent inbox task/query obligations that require a reply.
4. Explicit busy responses from the reviewer.

If reviewer availability is equivalent or unknown, use round-robin among eligible reviewers.

A reviewer who cannot start promptly should reply with a structured busy response:

```text
BUSY
current: <task id or message id>
unblock: <condition or estimate>
can_take_after: <time or "unknown">
```

The orchestrator may then reassign the review instead of waiting.

**Implementation note:** Review delegates are tracked in `dispatch_tracking.json` alongside impl delegates. The 15/30min stuck timeout (`sweep_stuck`) applies to review dispatches equally.

#### Active review lifecycle

A reviewer may have at most one active review task unless an incoming task is explicitly marked `interrupt=true` with a reason.

A review task becomes active when the reviewer reads or accepts the delegate_task, and remains active until `report_result` is sent.

New review dispatches to an active reviewer are queued, not implicitly preemptive. The reviewer must finish and report the active task before starting another, unless:

1. The new task is an explicit continuation of the same PR/thread, or
2. The dispatch includes `interrupt=true` with a reason and the orchestrator records why.

This encodes the anti-interrupt rule from the post-Sprint 7 review: silent preemption by newer dispatch was the Sprint 8 PR-H failure mode. The addendum prevents it by protocol rather than reviewer discipline alone.

#### Primary reviewer accountability

Every PR review has exactly one **primary reviewer**. The primary reviewer owns the final review report and must cover the full Reviewer Contract v1.1:

1. Source of truth.
2. Scope boundary.
3. Freshness boundary.

The primary report must include the standard metadata fields:

```yaml
scope_source: <decision id or design doc section>
audit_mode: full_review | finding_reaudit | scope_conformance
freshness_boundary: <commit sha, file version, or explicit stale condition>
```

#### Second reviewer exception

Do not assign two reviewers to the same PR by default.

A second reviewer may be assigned only when the dispatch explicitly includes `second_reviewer: true` or equivalent prose, and one of these conditions applies:

1. High-risk shared behavior, protocol, CI, or merge-gate change.
2. Repeated reject/re-review loop where a fresh eye is needed.
3. Primary reviewer requests a second opinion on a bounded question.
4. Operator or orchestrator records a scope decision requiring dual review.

The second reviewer must receive a bounded contract:

```text
Second reviewer scope: <specific files, findings, or question>
Must pin freshness boundary: <same commit/file boundary as primary>
Expected output: VERIFIED | REJECTED | UNVERIFIED with evidence
```

A second reviewer may narrow the audit surface, but must not omit the freshness boundary. A secondary `VERIFIED` does not replace the primary review.

#### Contract splitting

Do not split Reviewer Contract v1.1 so that no reviewer owns the whole contract.

Allowed:

- Primary covers source of truth, scope boundary, and freshness boundary.
- Secondary audits a bounded risk area against the same freshness boundary.
- Secondary performs grep-based confirmation for a named finding or regression risk.

Not allowed:

- Primary checks only source/freshness while secondary checks only scope.
- Two partial reviews are combined into one implied full review.
- A secondary review uses a different commit or file state without declaring the primary review stale.

#### Verdict merge rules

For merge gating, verdict severity is ordered:

```text
REJECTED > UNVERIFIED > VERIFIED
```

Worst verdict wins for the PR gate.

A `REJECTED` verdict must include finding-level evidence and the violated source/scope reference. The orchestrator creates one task per accepted finding.

An `UNVERIFIED` verdict must state the missing audit surface or stale boundary that prevented verification.

A `VERIFIED` verdict must state the freshness boundary reviewed.

If reviewers disagree on verdict or evidence:

1. The PR remains unmerged.
2. The orchestrator compares findings against the dispatch contract and freshness boundary.
3. If the conflict is factual or scope-related, the orchestrator posts a correction or clarification decision.
4. If the conflict cannot be resolved by the dispatch contract, escalate to the operator.

#### Re-review routing

Finding re-audits should normally return to the reviewer who issued the finding.

Reassign only when:

1. The original reviewer is unavailable or busy.
2. The re-audit is purely mechanical and the source/freshness boundary is explicit.
3. The orchestrator records that a fresh reviewer is intentional.

A reassigned re-audit must reference the finding task ID and must not broaden scope unless the dispatch explicitly says so.

#### 3.5.8 Cross-backend behavior claims

Any PR body claim of cross-backend behavior—e.g. "All CLI backends do X", "supports Y across kiro/Codex/Claude/Gemini", "behavior consistent on Linux/macOS/Windows"—**must** be either:

1. **Backed by per-backend test evidence** (preferred). Either:
   - Real backend spawn test (e.g. `#[ignore]` or cargo feature gated)
   - Capability matrix entry referenced in PR body with `verified: true` per backend

2. **Marked explicitly as `unverified claim`** with backlog task reference for verification:
   - PR body must contain phrase `unverified cross-backend claim` plus task ID
   - Backlog task must describe how/when verification will run

##### Reviewer enforcement

When reviewing a PR with cross-backend claims:
- Check PR body for both evidence (option 1) and unverified mark (option 2)
- If neither present, output `REJECTED` with finding "unverified cross-backend claim — must add per-backend test evidence or mark as unverified with backlog reference"

##### Rationale

Sprint 9 PR #159 (`interrupt` MCP tool) merged with PR body claim "All CLI backends treat ESC as stop generation". No per-backend test verified this; the claim was inferred from documentation. Operator caught the gap post-merge. Sprint 10 PR-X (backend harness) added transport verification but explicitly left semantics `Unverified`.

This rule prevents the pattern where reviewer-merged PRs ship documentation claims that were never tested. Either prove with evidence or transparently flag as future work.

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

### Response channel matches source channel

Every agent must reply via the same channel the input arrived on:

| Source signal | Reply mechanism |
|---|---|
| `(Reply using the reply tool, NOT direct text)` system hint | `reply` MCP tool (telegram) |
| `[from:OTHER_AGENT_NAME]` prefix | `send_to_instance` MCP tool |
| **Neither of the above** (operator typed in TUI) | **direct text** — do not use any tool |

**Why**: the daemon does not intercept TUI stdin, so there is no hint. If the agent uses `reply` (telegram) when the operator typed in TUI, the response appears in telegram instead of the terminal — the operator waits forever in TUI. The reverse (direct text when input came from telegram) is equally broken.

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


## 10. Workflow efficiency rules (v1.2)

Three rules to eliminate idle time. Operator-authorized 2026-04-26.

### 10.1 Pipeline dispatch

**Rule:** Implementer pushes PR, then immediately starts the next task. Do not wait for review or merge.

- PR rejected → implementer interrupts current task to rework the rejected PR.
- PR merged → current task unaffected, continue.

**Edge case policies:**

| ID | Policy |
|---|---|
| E1.1 | **Strict on-top-of-main.** Pipeline tasks must branch from main. If next task depends on a pending PR, do not pipeline — wait for merge. |
| E1.2 | **Pipeline depth ≤ 2.** Maximum: 1 PR in review + 1 task in progress. Three-deep cascades are unmanageable. |
| E1.3 | **Context-switch threshold on reject.** If next task is ≤30% done, switch back immediately. If ≥70% done, dev-lead may allow finishing before rework. Between 30-70%, dev-lead decides. |
| E1.4 | **Backend-aware capacity.** Claude agents: 3-4 concurrent review items. Kiro-cli agents: 1-2. Same caps apply to implementers. |

### 10.2 Reviewer does not wait for CI

**Rule:** Reviewer starts code review as soon as PR is pushed. CI green → send verdict immediately. CI red → handle by failure type.

**Edge case policies:**

| ID | Policy |
|---|---|
| E2.1 | **CI fail classification by job.** `fmt`/`clippy` red → lint issue, impl fixes, verdict still valid. `build`/`test` red → logic error, requires one more review round. Snapshot-only diff (no generator logic change) → impl updates snapshot, verdict valid. |
| E2.2 | **CI green is necessary, not sufficient.** Reviewer verdict is authoritative regardless of CI color. CI green does not auto-approve; CI red does not auto-reject. |
| E2.3 | **Force-push during review invalidates verdict.** Default: any push after review starts resets verdict. Exception: reviewer can verify commit-level patch hash matches via stack-base diff — but default invalidation is safer. |
| E2.4 | **`reviewed_head` is a snapshot, not a contract.** VERIFIED applies to the exact SHA in `reviewed_head`. Any subsequent commit resets verdict state. Aligns with GitHub "dismiss stale review on push". |
| E2.5 | **Dual reviewer (§3.5.5) not short-circuited.** Rule 2 does not override §3.5.5 mandatory dual review. Dev-lead must not auto-merge on single VERIFIED + CI green when dual review is required. |
| E2.6 | **Reviewer pipeline cap by backend.** Reviewers also pipeline. Claude: 3-4 concurrent reviews. Kiro-cli: 1-2. |
| E2.7 | **Scope-creep priority over CI red.** REJECT primary reason is always scope violation. CI failure is secondary detail. |
| E2.8 | **r2 dispatch must enumerate r1 findings.** Re-review dispatch template must list each r1 finding as fixed/deferred/withdrawn. Missing enumeration → reviewer falls back to `full_review`. |

### 10.3 Task close on completion

**Rule:** Task state tracks PR lifecycle through three states: `in_progress` → `verified` → `done`.

| State | Who sets | When |
|---|---|---|
| `claimed` | Implementer | Task accepted |
| `in_progress` | Implementer | PR pushed |
| `verified` | Reviewer | VERIFIED verdict sent |
| `done` | Dev-lead | PR merged |

**Ownership:** Impl-claimed tasks are closed by the impl owner (or dev-lead on merge). Reviewers close only tasks assigned to them (review dispatch tasks). This avoids permission conflicts where reviewer cannot update impl-claimed tasks.

**Edge case policies:**

| ID | Policy |
|---|---|
| E3.1 | **Impl owns close for impl tasks.** Reviewer sets `verified`; dev-lead sets `done` on merge. Reviewer does not attempt `task done` on impl-claimed tasks (daemon rejects non-owner close). |
| E3.2 | **Three-state model.** `in_progress` (impl working) → `verified` (reviewer approved) → `done` (merged). No skipping states. |
| E3.3 | **Merge fail handling.** If merge fails (conflict) after `verified`, task drops back to `in_progress`. Impl resolves conflict, re-pushes, reviewer re-verifies. |
| E3.4 | **Multi-round review cycle.** REJECTED → task stays `in_progress` → rework → push → re-review → `verified`. Task never enters `done` until merge. |
| E3.5 | **Dev-lead merge gate.** Dev-lead verifies task state before merge. If reviewer/impl forgot to update, dev-lead updates as safety net. Protocol-level mitigation; daemon auto-close on PR merge is a future enhancement. |
| E3.6 | **Idempotent close.** Closing an already-done task is a no-op (daemon should not error). |
| E3.7 | **Done-but-superseded.** If scope changes after task is done, post a decision and create a new task with `depends_on`. Do not reopen the original. |
| E3.8 | **Verdict evidence chain.** Every verdict report must include: `reviewed_head`, `scope_source`, `audit_mode`, `commands`, `files`. See §3 metadata fields. |
