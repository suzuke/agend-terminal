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

### 3.3.1 CI Verification Gate (Sprint 61)
Before approving merge, orchestrator/reviewer MUST independently verify CI:

```
gh pr checks <PR#>
```

**Hard rules:**
- Exit code 0 (all checks pass) required before merge approval
- Do NOT rely on dev's self-reported CI status or ci_watch notifications alone
- Do NOT rely on partial check results (e.g., only LOC overrun passing)
- If any check is `pending`, wait and re-check
- If any check is `fail`, block merge and report to implementer

**Rationale:** Sprint 61 incident — ci_watch emitted false [ci-pass] on partial completion, leading to merge of failing code.

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
- Every fix PR MUST include an empirical reproduction test case. Reviewers MUST verify the presence and validity of this test.
- Reviewer verifies: `git checkout <test-sha>` fails → `HEAD` passes
- Exemptions: docs-only, pure refactor, test-only, dep bump, EMERGENCY, pure deletion, empirical-revert

### 3.11 Deferred-defense
- (a) Known-issue recurs in production → auto-escalate to P0
- (b) Deferred backlog must have `due_at` (default: 2 sprints)
- (c) Same root cause deferred twice → mandatory dual reviewer + operator sign-off
- (d) Removing defensive code → 4-perspective counter-example challenge; 0 compelling = safe to delete

### 3.12 Verdict Externalization (was §3.5.13)
Fleet-internal verdict MUST mirror to GH PR comment (`gh pr comment`). Self-merge gate: dual VERIFIED + CI green (independently verified via `gh pr checks`) + verdict mirror posted — all three required before merge.

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

### 4.5 Cross-team ACK Absorption Exception (Sprint 61, #612)
One-shot backends (Codex) skip PTY injection for `kind=update` and `kind=report` messages to avoid wasting turns. However, **cross-team messages are NEVER silently absorbed** — they always inject to PTY regardless of backend or message kind. Team membership is checked at delivery time; agents not in any team are treated as cross-team (safe default). Absorbed messages are audit-logged as `ack_absorbed` events.

## §5. Async Pipeline

Impl pushes PR then immediately starts next task. Reviewer issues verdict then immediately takes next review. dev-lead maintains pending list; dual-VERIFIED + CI green (independently verified via `gh pr checks`) → self-merge.

**Key rules**:
- Impl push must include scope statement (follows spec / deviated because)
- Orchestrator pre-dispatch verification: cross-check dev's claim against actual artifact before forwarding to reviewer
- dev-lead uses `schedule(action: create)` for auto-poll (30min fallback)
- Takeover requires 4 criteria independently verified (heartbeat stale ≥1h, last_input frozen, idle state, zero activity)
- Merge must atomically include `git worktree remove` + `git branch -D`
- Post-merge: orchestrator verifies main CI green before reporting task completion upstream. Failed main CI = immediate P0 (revert or hotfix).
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

**Dispatch milestone updates** — when you accept a `task` dispatch, send `kind=update` to the dispatcher at each of these milestones without being asked:

1. **r0 ready** — PR opened (or work artifact handed off), with verbatim links / heads.
2. **CI all-green** — every CI gate the PR runs has reported success. The `[ci-pass]` watch broadcast does NOT substitute — confirm via your own update so the dispatcher's loop closer fires regardless of their channel state.
3. **Reviewer verdict received** — VERIFIED / REJECTED / UNVERIFIED, with the reviewer's identity and key finding summary.

Re-review cycles (r1, r2, …) repeat the same three milestones. The dispatcher relies on these as the loop closer; missing any forces them to poll, which is anti-pattern (see §7).

- Pure ack → do not reply (ACK absorption §4 handles this automatically)
- Response channel must match source channel
- **Router-layer channel discipline (Sprint 52)**: daemon auto-mirrors agent direct text to the corresponding channel. Agent does not need to force `reply` tool — infrastructure handles routing.

## §7. CI

Use `ci(action: watch)` for ongoing monitoring, not manual polling. Exception: merge-gate final verification requires one-shot `gh pr checks <PR#>` per §3.3.1. Clean up worktree + branch after merge.

**No manual orchestrator polling**. Orchestrators (lead, general,
operator-in-the-loop) MUST NOT manually poll PR / CI state via
`gh pr view`, `gh run list`, repeated `cargo test`, or equivalent.
Rely on:

1. The dispatchee's `kind=update` milestones (§6) — r0 ready, CI
   all-green, reviewer verdict.
2. `ci(action: watch)` fan-out — `[ci-pass]` / `[ci-fail]` /
   `[ci-watch-stalled]` arrive automatically.

Manual polling masks broken dispatch communication and burns cache /
rate-limit budget unnecessarily. If a milestone is missing past a
reasonable window, the correct response is to message the dispatchee
asking why, not to poll. Polling is also a smell that the dispatch
brief itself didn't enumerate the expected milestones — fix the
dispatch, not the symptom.

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

**Health surface (Sprint 54 P0-5)**. The `ci(action: watch)` response
and the new `ci(action: status)` aggregator both carry `rate_limit_active`,
`rate_limit_until`, and `next_poll_eta` so agents can tell whether CI
polling is healthy without reading watch files. The daemon also
fans out two inbox event kinds when polling stalls behind a rate-limit
window: `ci-watch-stalled` after 3 consecutive missed polls (exactly
once per stall window) and `ci-watch-resumed` on the first successful
poll afterward. Both events go to every subscriber via the P0-1 fan-out
contract — no last-write-wins. Surface stalled events promptly; resumed
events confirm recovery and may be acknowledged silently.

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
- **Generic `bind_self` (Sprint 54 P1-7)**: any agent (lead, dev, reviewer, …) may proactively claim a worktree via `bind_self {repo, branch}` without going through the dispatch hook. Inherits every dispatch invariant — Phase 1 trailers, P0-1.5 cross-agent registry, P0-1.6 actual-HEAD verification, P0-X release_worktree as sole exit, source_repo persistence, auto watch_ci. Use case: lead orchestrator escalating to Path A IMPL on a hot branch. Pair with `release_worktree` to unbind. `main`/`master` rejected with E4.5; cross-agent branch conflicts return `code: cross_agent_conflict`.

### release_worktree branch-cleanup scope

`release_worktree` auto-cleanup ONLY operates on branches that satisfy ALL of:
1. The worktree was daemon-managed (`.agend-managed` marker verified)
2. The branch is confirmed merged into main OR remote tracking ref is gone (squash-merge)
3. Protected refs (main/master) are NEVER touched

User-checkout branches, operator-created worktrees without `.agend-managed` marker, and any branch where the marker cannot be verified are NEVER deleted.

## §11. Tool Quick Reference

| Need | Use | NOT this |
|---|---|---|
| Track work | `task(action: create/list/claim/done)` | local task lists |
| Record decisions | `decision(action: post)` | Markdown files |
| Assign work | `send(kind: task)` + `task(action: create)` | only one |
| Report results | `send(kind: report)` | free-text |
| CI monitoring | `ci(action: watch)` | manual `gh run list` loops |
| CI merge gate | `gh pr checks <PR#>` | trusting dev self-report |
| Wait state | `set_waiting_on` | prose |
| Health check | `describe_instance` | guessing |
| Schedule | `schedule(action: create)` | backend-specific tools |
| Timeout | `replace_instance` | waiting forever |

**Daemon-state error format (Sprint 54 #488 hotfix)**. Tools that
depend on daemon-resident state — `reply`,
`download_attachment` — never silently fall back to a local handler
when the daemon is unreachable. They return a structured error of the
form `tool '<NAME>' requires daemon API; not reachable: <CAUSE>`.
Agents seeing this prefix should surface the message as-is to the
user (it's operator-actionable: restart daemon / check socket) rather
than retry blindly. Stateless tools (`inbox`, `task`, `send`, etc.)
still fall back gracefully for offline workflows.

## §12. Workflow Efficiency

### 12.1 Pipeline Dispatch
Push PR then immediately start next task. Depth ≤ 2. Must branch from main (no stacking on pending PR).

### 12.2 Reviewer Does Not Wait for CI
Start review on PR push. `reviewed_head` is a snapshot; subsequent commits reset verdict.

### 12.3 Task Close
`in_progress` → `verified` (reviewer) → merge (CI green per §3.3.1) → post-merge main CI green → `done`.

**Post-merge verification**: After squash-merge, orchestrator MUST verify main branch CI passes:
```
gh run list -b main --limit 1
```
or wait for ci_watch [ci-pass] on main. Only declare task `done` after main CI is confirmed green. If main CI fails post-merge, immediately investigate and fix (revert if necessary).

### 12.4 Worktree Mandatory
Impl/reviewer must use worktrees. `git worktree add -b <branch> <path> origin/main`. **Never** `git worktree add <path> main`.

### 12.5 Spawn Site Rationale
Every spawn must have `// fire-and-forget: <reason>` OR store JoinHandle. Test-only exempt.

### 12.6 Multi-PR Wave Sequential Merge
When multiple PRs ship in the same wave (same dispatch/task_id):
1. Merge sequentially: A → rebase B on new main → re-verify CI → merge B → ...
2. Never parallel merge — later PRs have stale base
3. After each merge, remaining PRs must rebase and re-run CI before merge

Daemon enforcement: `send(sequencing: "sequential-merge-only")` signals this constraint to downstream agents. Recipients MUST merge one at a time and verify CI between each merge.

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
