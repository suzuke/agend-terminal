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

#### LOW docs-only single-reviewer exception

Sprint 22 P3 amendment. Mid-scope and code-touching protocol PRs continue to require dual reviewer per the rules above. The exception below narrows the dual-review trigger so that genuinely docs-only protocol edits — the kind operators land routinely as protocol housekeeping — do not bottleneck on two reviewers.

**A protocol PR qualifies for the single-reviewer exception when ALL of the following hold:**

1. **LOW risk classification** — `docs/FLEET-DEV-PROTOCOL-v1.md` (or sibling protocol files) edit only.
2. **Diff size ≤ 50 LOC** measured against `git diff origin/main...HEAD` (3-dot diff so the reviewer's freshness-boundary read is unaffected).
3. **No production code change beyond `src/instructions.rs` template strings** — i.e., the only `src/` touch allowed is the agent-instructions template that mirrors the protocol prose. Any change to `src/agent.rs`, `src/daemon/`, `src/channel/`, `src/mcp/`, `src/api/`, `src/app/`, or `src/tasks.rs` (etc.) **disqualifies** the exception even if the docs portion is small.
4. **No new rule that mid-scope+ PRs would inherit** — e.g., adding a new `§3.5.x` subsection that introduces an enforcement obligation for non-protocol PRs is mid-scope, not LOW. Pure clarifications, typo fixes, and example additions to existing rules qualify.

**When the exception applies, the dispatch may select either:**

- **Single reviewer** (default for the LOW-docs-only path) — primary reviewer alone covers the full Reviewer Contract v1.1 against the existing freshness boundary.
- **Operator self-merge** — operator may merge directly without dispatching a reviewer when the operator authored or proof-read the diff. This matches the existing operator-author lifecycle for HOTFIX paths.

The dispatch document or PR description must state which arm of the exception is invoked, e.g. `LOW docs-only protocol PR — single reviewer per §3.5.4-exception` or `LOW docs-only protocol PR — operator self-merge per §3.5.4-exception`. Reviewers and the orchestrator can audit the qualification by reading the diff against the four conditions above.

**Mid-scope+ unchanged**: the rules in [Second reviewer exception](#second-reviewer-exception) above still trigger dual review for protocol or merge-gate changes outside this exception (high-risk shared behavior, repeated reject loops, primary-requested second opinion, operator-mandated dual review). The exception narrows the *default* dual-review obligation for docs-only edits; it does not change when the dispatch may explicitly opt in to dual review.

**Amendment recursion** (this PR's own gate): the amendment PR that introduces this exception is itself dispatched under the *pre-amendment* §3.5.5 mandatory-dual-review rule (i.e., it walks dual reviewer). The exception only applies to PRs landing **after** this amendment merges. This avoids the bootstrap paradox of the amendment PR using its own exception to short-circuit its review.

**Case study evidence** (PR #226 — Sprint 21 Phase 5a Q6 protocol amendment, `§10.5 Rule 5 spawn rationale`): operator self-merged at 2026-04-27 03:35 UTC after a single-reviewer VERIFIED + 3-platform CI green. The PR was 2 files (`docs/FLEET-DEV-PROTOCOL-v1.md` +44, `src/instructions.rs` +1), +45/-0 lines total — exemplifying *exactly* the LOW docs-only + `instructions.rs` template touch pattern that this exception's conditions #2-#3 carve out. Under the pre-amendment rule this technically required dual review; the operator's self-merge worked out (no regression, dev-reviewer Tier-1 verdict robust) but encoded the gap this exception now formalises. Future docs-only protocol PRs should cite this exception in the dispatch rather than relying on operator override.

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

### 10.4 Worktree mandatory for impl and reviewer (v1.2 amendment)

**Rule:** All implementers and reviewers must work in a git worktree, not the main repository working tree. This prevents checkout races when multiple agents pipeline work concurrently (v1.2 Rule 1).

**Evidence:** git reflog showed 12+ checkout operations in 1 hour on the main repo — two implementers racing `git checkout` on the same working tree during pipeline dispatch.

**Worktree naming convention:**
```
git worktree add ../agend-terminal.worktrees/<branch-name> <branch-name>
```
Or for reviewers:
```
git worktree add /tmp/agend-prNNN-review <branch-name>
```

**Exceptions:**
- **dev-lead**: merge operations + read-only queries may use the main repo (atomic merge is safe).
- **general**: operator interface agent, not bound by impl/reviewer workflow.

**Edge case policies:**

| ID | Policy |
|---|---|
| E4.1 | **Worktree per branch.** Each PR branch gets its own worktree. Do not reuse worktrees across branches. |
| E4.2 | **Cleanup after merge.** `git worktree remove <path>` + `git branch -d <branch>` after PR merge. Stale worktrees waste disk and confuse `git worktree list`. |
| E4.3 | **Consistent with CLAUDE.md.** This rule formalizes the existing CLAUDE.md global rule "never commit directly to main; always use worktree + branch" into the fleet protocol. |
| E4.4 | **Pipeline + worktree.** Rule 1 pipeline dispatch naturally requires worktrees — you cannot have two branches checked out in one working tree. Worktree mandatory is the mechanical prerequisite for pipeline to work safely. |

### 10.5 Spawn site rationale (v1.2 amendment)

**Rule:** Every `tokio::spawn` / `thread::spawn` / `std::thread::Builder::new().spawn(...)` site **MUST** carry a `// fire-and-forget: <reason>` comment at the call site OR explicitly store the `JoinHandle` for graceful join on shutdown. The choice between the two is design-conscious — fire-and-forget acknowledges the thread/task outlives the daemon shutdown path; stored-JoinHandle commits to a join order in the shutdown sequence.

**Evidence:** Sprint 20 Track B (`docs/codebase-review-2026-04-27/DAEMON.md` JoinHandle inventory) found 11 spawn sites in daemon scope, **only 1** with explicit rationale (`supervisor.rs` module-doc). Sprint 20.5 Track 7 B↔C cross-validation extended this to **13–15+ fleet-wide spawn sites with 0 graceful-join handling** — `app/telegram_hooks.rs:56,76` added two more unnamed sites in Track C scope, confirming the pattern is systemic, not daemon-internal.

This rule formalizes the v1.2 baseline so Sprint 22+ hardening (real graceful-join refactor) has an explicit pre-condition: every new spawn site explains itself, every refactor that adds a `JoinHandle` has a documented reason.

**Naming convention** (rule applies to thread name + comment):

```rust
// fire-and-forget: <reason>; shutdown semantics: <how this thread exits>
std::thread::Builder::new()
    .name("agent_pty_read".into())
    .spawn(move || pty_read_loop(...))
    .ok();
```

OR (stored JoinHandle):

```rust
// joined-on-shutdown: <where the join happens>
let handle = std::thread::Builder::new()
    .name("daemon_tick".into())
    .spawn(move || tick_loop(...))?;
// ... join during graceful shutdown:
let _ = handle.join();
```

**Exceptions:**
- **`#[cfg(test)]` modules** (E5.2): test-only spawns inside `#[cfg(test)] mod tests { ... }` are exempt — test fixtures often spawn ephemeral threads that the test framework reaps automatically.
- **Trait-method spawn that wraps a caller-provided closure** (E5.3): if the spawn is delegated by a trait method and the caller is responsible for the rationale (e.g. `Channel::send` internally spawns to send-and-forget per-channel), the trait-method site inherits the caller-site rationale. Document at the caller site, not at the trait-method site.

**Edge case policies:**

| ID | Policy |
|---|---|
| E5.1 | **Short-lived (<1s) spawns** can use a 1-line rationale. Full design rationale (pointing at architecture doc / module-level explanation) is only required when the thread/task lives for the full daemon process or longer than 1s. Example: `// fire-and-forget: short-lived (~300ms) cleanup of orphan binding; thread dies on completion` is acceptable for `app/telegram_hooks.rs:56`. |
| E5.2 | **Test-only spawns exempt.** `#[cfg(test)] mod tests { ... }` spawn sites do not require this comment. Production-quality `tracing::warn!` etc. are still recommended for tests that exercise real spawn paths but not enforced. |
| E5.3 | **Trait-method spawn inherits caller rationale.** When a trait method (e.g. `Channel::send`) spawns internally on behalf of a caller, the trait-method site carries a short pointer (`// see caller site for fire-and-forget rationale`) and the caller site carries the full rationale. This avoids stale duplication if multiple callers share a trait method. |
| E5.4 | **Spawn allowlist invariant test (Phase 5b enforce).** Sprint 21 Phase 5b (impl-2) introduces a `cargo test` invariant that `rg "thread::spawn|tokio::spawn"` against an allowlist of explicitly-rationalised sites — test fails if a new spawn site lands without `// fire-and-forget: ` or `// joined-on-shutdown: ` comment in the same file. Forward-reference to the test (when it lands): `tests/spawn_rationale_audit.rs`. |

**See also:** Sprint 20 SYNTHESIS.md JoinHandle inventory (counts + per-site doc status); Sprint 20.5 SYNTHESIS v2 fleet-wide extension; per-backend agent files (`.agend/<backend>.md`) for backend-specific spawn instructions; §10.4 Rule 4 (worktree mandatory) — sibling rule covering a different audit surface (worktree race vs spawn race).
