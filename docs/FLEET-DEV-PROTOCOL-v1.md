# Fleet Development Protocol v1.2 (Condensed)

**Status:** ACTIVE — all fleet agents must follow this protocol.

## §0. KISS Principle

Every PR must answer: **"What real problem does this solve?"** and **"Would deletion break anyone?"** Changes lacking a concrete failure mode = `KISS-VIOLATION — UNVERIFIED`.

## §1. Task Board (Single Source of Truth)

Use daemon `task` tool, NOT per-agent local task lists.

**Lifecycle**: `create` → `claim` → `in_progress` → `verified` → `done`

**Rules**:
- Orchestrator creates tasks; Implementer/Reviewer update status
- `depends_on` must be set when dependency exists
- `task done` must include `--result`

## §2. Decisions Panel

Use `decision(action: post)` to freeze scope definitions or ground truth changes.
- `tags` must include track + PR number
- `scope: fleet` for cross-track; `scope: project` for track-specific
- `supersedes` links corrections to original decision

## §3. Review Protocol

### 3.1 Pre-implementation
Orchestrator posts scope decision + creates task.

### 3.2 Review Dispatch Contract (3 parts)
1. **Source of truth** — design doc or decision ID
2. **Scope boundary** — audit X, ignore Y
3. **Freshness boundary** — stale if changed after {sha}

### 3.3 Verdict
`VERIFIED` / `REJECTED` / `UNVERIFIED`

Every review report must include: `scope_source`, `audit_mode`, `reviewed_head`, `commands`, `files`

### 3.4 Re-review (r2) Dispatch
Must enumerate r1 findings with status: fixed / deferred / withdrawn. Missing → reviewer falls back to `full_review`.

### 3.5 Multi-reviewer
- Default: single primary reviewer
- Dual reviewer only when: high-risk shared behavior, repeated reject loop, primary requests, operator mandates
- Verdict severity: `REJECTED > UNVERIFIED > VERIFIED` — worst wins

### 3.6 LOW Docs-only Exception
All conditions must hold for single-reviewer or operator self-merge:
1. Only `docs/FLEET-DEV-PROTOCOL-*.md` or `REVIEWER-CONTRACT-*.md` edits
2. Diff ≤ 50 LOC
3. No `src/` behavior change (`src/instructions.rs` template strings exempt)
4. No new rule affecting mid-scope+ PRs

### 3.7 Cross-backend Claims
Must have per-backend test evidence, OR mark as `unverified cross-backend claim` + backlog task.

### 3.8 Cross-team Auth Chain
Cross-team reviewer borrowing or task delegation must cite operator auth chain (e.g. operator message ID). New agents must not assume cross-team access without explicit authorization.

### 3.9 External-fixture Validation
Three PR categories require external fixtures:
1. **Wire-format** — production capture / RFC fixture / cross-implementation reference
2. **Concurrent-state** — multi-threaded harness / loom / stress loop
3. **Persistence-replay** — write → restart → restore round-trip

Additional: wire-format invariant tests (pin shape); production-path-coupled (no helper mimics).

### 3.10 Test-first
Feature/fix PRs must be test-first: failing test commit BEFORE impl commit.
- Reviewer verifies: `git checkout <test-sha>` fails → `HEAD` passes
- Exemptions: docs-only, pure refactor, test-only, dep bump, EMERGENCY, pure deletion, empirical-revert

### 3.11 Deferred-defense
- (a) Known-issue recurs in production → auto-escalate to P0
- (b) Deferred backlog must have `due_at` (default: 2 sprints)
- (c) Same root cause deferred twice → mandatory dual reviewer + operator sign-off
- (d) Removing defensive code → 4-perspective counter-example challenge; 0 compelling = safe to delete

### 3.12 Verdict Externalization (was §3.5.13)
Fleet-internal verdict MUST mirror to GH PR comment (`gh pr comment`). Self-merge gate: dual VERIFIED + CI green + verdict mirror posted — all three required before merge.

### 3.13 Log-level Changes (was §3.5.14)
Must have inline rationale, otherwise `LEVEL-CHANGE-RATIONALE-ABSENT — UNVERIFIED`.

### 3.14 Observability PRs (was §3.5.15)
Must include e2e integration test exercising the production hook path.

### 3.15 Daemon-core Cushion Rule
PRs touching daemon core / channel / supervisor / state.rs must include stress test + lock-ordering analysis before dispatch. "不急 ship" principle — correctness over velocity for infrastructure changes.

## §4. Daemon Enforcement Gates

### 4.1 Push-time Semantic Gate (Sprint 44)
Daemon validates dev's push claim matches actual diff. Recognized grammar:
- `"no other changes"` / `"byte-equal verified"` / `"scope follows dispatch spec X"` / `"only formatting"` / `"deps unchanged"`

Unknown grammar → hard reject. No pass-through.

### 4.2 Reviewer SHA-staleness Gate (Sprint 44)
Daemon compares `reviewed_head` against PR HEAD at verdict time. Mismatch → reject verdict. Reviewer must `git fetch` and re-review. Fail-closed (fetch failure = reject).

### 4.3 Hallucinated-fn Check (Sprint 44)
When push claim references a function name, daemon verifies existence via syn-lite + rg fallback. Not found → reject push.

### 4.4 Reserved Name Warning (Sprint 46)
Instance names with routing semantics (`general`, `lead`, `dev`, `reviewer`) emit warning on create. Not a hard reject.

## §5. Async Pipeline

Impl pushes PR then immediately starts next task. Reviewer issues verdict then immediately takes next review. dev-lead maintains pending list; dual-VERIFIED + CI green → self-merge.

**Key rules**:
- Impl push must include scope statement (follows spec / deviated because)
- Orchestrator pre-dispatch verification: cross-check dev's claim against actual artifact before forwarding to reviewer
- dev-lead uses `schedule(action: create)` for auto-poll (30min fallback)
- Takeover requires 4 criteria independently verified (heartbeat stale ≥1h, last_input frozen, idle state, zero activity)
- Merge must atomically include `git worktree remove` + `git branch -D`
- Orchestrator owns `ci(action: watch)` for own-orchestrated branches
- Stuck-agent timeout: see §9 timeout staircase

## §6. Communication

Use `send` for all inter-agent messaging:

| `request_kind` | Use | Expects reply? |
|---|---|---|
| `task` | delegation | yes |
| `report` | result/verdict | depends |
| `update` | FYI | no |
| `query` | question | yes |

**Routing**: `target_instance` (single) or `targets` / `team` / `tags` (broadcast)

- Pure ack → use `react` (emoji), not `send`
- Response channel must match source channel
- **Router-layer channel discipline (Sprint 52)**: daemon auto-mirrors agent direct text to the corresponding channel. Agent does not need to force `reply` tool — infrastructure handles routing.

## §7. CI

Use `ci(action: watch)`, not manual polling. Clean up worktree + branch after merge.

**PR open semantics (Sprint 54)**. Implementers MUST open feature PRs as
**ready** for review by default. The `--draft` flag is reserved for
exactly three scenarios:

1. **Smoke / verification PRs** that will not be merged (e.g. CI
   notification path tests). Title prefixes `[smoke]` / `chore: smoke`.
2. **Explicit work-in-progress** where the implementer needs to push
   midway and is not yet asking for review. Move to ready before
   pinging lead/reviewer.
3. **External-PR patches** where lead is augmenting a community
   contribution before the upstream PR is merged.

A draft PR is hidden from GitHub's default UI filters, so operator and
reviewer miss it without explicit checks. Default-ready keeps the
review pipeline visible.

**Setup-warning surfacing (Sprint 54 P0-4)**. CI-related MCP responses
may include a top-level `setup_warning` string when no GitHub token is
reachable (env unset AND `gh` unavailable/unauthed). The daemon polls
unauthenticated in that state and exhausts the 60 req/hr cap quickly.
Agents MUST surface `setup_warning` verbatim to the user the first time
it appears in a session — it is operator-actionable guidance, not a
log line. Suggested phrasing: "CI watch responded: <setup_warning>".
Subsequent occurrences within the same session may be deduplicated.

## §8. Progress Visibility

Task state changes emit to Telegram. Instance lifecycle events (non-fleet.yaml origin) broadcast with `origin` field. `create_instance` defaults to isolated workspace (`~/.agend-terminal/workspace/<name>`).

## §9. Waiting & Timeout

- `set_waiting_on` to declare blockers (auto-clears after 120s inactivity)
- Use `schedule(action: create)` for check-ins (cross-backend)

**Timeout staircase** (single source of truth):

| Elapsed since dispatch | Action |
|---|---|
| < 20 min | Normal. `describe_instance` — fresh heartbeat = agent active. |
| 20 min, heartbeat fresh | Agent working. Extend wait. |
| 20 min, heartbeat stale (>120s) | Ping via `send` with direct question. |
| 20 min, no response to ping | `replace_instance` and re-dispatch. |

**Backend modifiers**:
- kiro-cli: 1-2h longer wait (context compaction self-heals); escalate to operator rather than `interrupt`
- Other backends (claude/codex/gemini/opencode): use staircase above as-is

### Supervisor Notify
Daemon detects agent entering error state (UsageLimit/RateLimit/Hang/Crashed/AuthError/PermissionPrompt) → notifies orchestrator. 60s debounce per agent.

## §10. Git Workflow

- Never commit directly to main; always use worktree + branch
- Branch naming: `feat/`, `fix/`, `docs/`
- Clean up immediately after merge
- **Never** `git worktree add <path> main` — locks main, breaks operator builds. Always use `-b <new-branch>`. Recovery: `cd <worktree> && git switch -c <dedicated-branch>`

## §11. Tool Quick Reference

| Need | Use | NOT this |
|---|---|---|
| Track work | `task(action: create/list/claim/done)` | local task lists |
| Record decisions | `decision(action: post)` | Markdown files |
| Assign work | `send(kind: task)` + `task(action: create)` | only one |
| Report results | `send(kind: report)` | free-text |
| CI | `ci(action: watch)` | `gh pr checks` |
| Wait state | `set_waiting_on` | prose |
| Health check | `describe_instance` | guessing |
| Schedule | `schedule(action: create)` | backend-specific tools |
| Timeout | `replace_instance` | waiting forever |

## §12. Workflow Efficiency

### 12.1 Pipeline Dispatch
Push PR then immediately start next task. Depth ≤ 2. Must branch from main (no stacking on pending PR).

### 12.2 Reviewer Does Not Wait for CI
Start review on PR push. `reviewed_head` is a snapshot; subsequent commits reset verdict.

### 12.3 Task Close
`in_progress` → `verified` (reviewer) → `done` (dev-lead merge). Three states, no skipping.

### 12.4 Worktree Mandatory
Impl/reviewer must use worktrees. `git worktree add -b <branch> <path> origin/main`. **Never** `git worktree add <path> main`.

### 12.5 Spawn Site Rationale
Every spawn must have `// fire-and-forget: <reason>` OR store JoinHandle. Test-only exempt.

## §13. `AGEND_GIT_BYPASS=1` Usage

**TL;DR:** emergency override only. Default is bare `git`. Bypass when shim explicitly denies AND the operation is on the required-bypass list below.

### 13.1 When you should NOT use bypass

Inside your bound worktree, all routine git ops pass through the shim cleanly. Run them bare:

```bash
git status / diff / log / show
git add / commit / fetch
git push origin <your-branch>     # any branch except main
git checkout <existing-branch>    # within current repo
git reset --hard <ref>            # within your worktree
```

Don't preemptively prefix `AGEND_GIT_BYPASS=1`. Try bare git, read the deny message if it fires, then decide.

### 13.2 When bypass is required

Operations on the lifecycle/safety surface the daemon manages directly:

- `git worktree add` / `remove` / `move` — worktree pool is daemon-owned (Phase 3 lease, P0-X release)
- `git checkout main` from an agent worktree — cross-branch deny in the shim matrix
- Operator manual cleanup of orphan worktree or orphan binding (no MCP tool yet for some edge cases)
- Daemon's own internal git command — bypass is set by the daemon to prevent self-recursion through its own shim

If your op isn't on this list and you reach for bypass, you're probably solving the wrong problem.

Note: `git push origin main` is **workflow-prohibited** (PR + CI gates required), but the current shim matrix does not deny it directly. The protection comes from review process, not from the shim. Don't push to main even though `git push origin main` would not trip a shim deny today.

### 13.3 Why bypass is costly

Skipping the shim skips the safety net:

- **Phase 1 trailer skipped** — commit lacks `Agend-Agent: <name>` provenance, breaks audit trail
- **Deny matrix skipped** — risky ops (force-push to protected refs, etc.) run unguarded
- **Git registry can drift** — `git worktree add` outside the daemon's pool leaves untracked entries; subsequent leases may collide
- **Phase 5 hotspot warning skipped** — concurrent edits to flagged files don't surface on the dispatch path

These are not catastrophic individually. They erode the invariants the shim was built to maintain.

### 13.4 Default workflow

1. Run bare `git <command>`.
2. If the shim denies, read the deny message — it names the specific reason and suggests a remediation.
3. If the remediation is "use bypass," set `AGEND_GIT_BYPASS=1 AGEND_GIT_BYPASS_AGENT=<your-name>` for that one command.
4. If the remediation is something else (e.g., "use the task board to get a worktree assignment"), follow it.

`AGEND_GIT_BYPASS_UNTIL=<epoch>` exists for time-windowed bypass during multi-step operator interventions; per-command env is preferred for normal use.

---

## Appendix: Section Number Map (old → new)

| Old (v1 full) | New (condensed) |
|---|---|
| §3.5.5 | §3.6 |
| §3.5.9 | §3.7 |
| §3.5.10 | §3.9 |
| §3.5.11 | §3.10 |
| §3.5.12 | §3.11 |
| §3.5.13 | §3.12 |
| §3.5.14 | §3.13 |
| §3.5.15 | §3.14 |
| §3.6 | §5 |
| §10.1-10.5 | §12.1-12.5 |
