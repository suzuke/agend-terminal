# Sprint 54 — ci_watch Reliability + Lifecycle Hardening + Docs Sprint

**Date**: 2026-05-07
**Owner**: lead (draft) → general (review)
**Status**: PLAN DRAFT — operator review pending, no IMPL until approved
**Source-of-truth**: `origin/main` HEAD `8a74e30` (post #481 close-path cleanup merge)
**Predecessor sprints**: Sprint 53 wire-and-cleanup (PR #463 PLAN; P0-1/1.5/1.6/2/3/X + P1-4 IMPL; #477 comms-unbind + #481 close-path hotfixes; pre-Sprint-54 Smokes 1+2+3)

---

## §1 Background

### 1.1 What Sprint 53 closed

Per PR #463 PLAN + retrospective decisions (`d-20260506171720519048-2`, `d-20260506171736738779-3`, `d-20260506171805866878-4`):

- **Phase 1-5 wired into agent workflow**: dispatch hook auto-creates binding + leases worktree + auto-watches CI on `delegate_task` with branch field (P0-1/1.5/1.6/2).
- **release_worktree as SOLE binding mutation point**: comms.rs `binding::unbind` removed (#477 critical); P0-X exposes the only public exit.
- **Production-smoke gate established**: §1.4 hard learning baked into protocol — every phase requires "who calls this in prod?" trace before merge.
- **Test discipline**: P0-3 dead-code-helper-pattern lint gate enforces tests must call production fns.
- **Empirical regression-proof discipline**: every Tier-2 fix must demonstrate test FAIL on revert.
- **Cleanup lifecycle three-tier layering**: per-pane (`full_delete_instance`) → per-deployment (`cleanup_deployment_dirs` + `reconcile_after_close`) → boot reconcile (`reconcile_orphans`).
- **Documentation**: FLEET-DEV-PROTOCOL §13 (bypass usage), README "Git Behavior Modification" disclosure section.

### 1.2 What Sprint 53 left for follow-up

Three categories of unresolved work surfaced during the sprint:

**A. ci_watch reliability** — multiple thin failure modes, each independently small but compounding:
1. GitHub API rate-limit hits during long polls; recovery path silently drops notifications (Smoke 3 likely catch).
2. Multi-caller subscriber semantics — last-write-wins overwrite when both lead and dev watch the same branch (`d-20260506155323776106-0`).
3. Instance overwrite vs merge semantics (#152).
4. GITHUB_TOKEN auto-detect from `gh` config + agent-visible setup warning (`d-20260506171309264856-1`).

**B. Lifecycle edge cases** — cleanup layering done, but corners remain:
5. release_orphan_worktree edge case (#142).
6. task_done auto-release lifecycle gap.
7. Daemon-created worktree initial base point — currently empty `init` commit instead of `origin/main` (Smoke 3 dev's friction).
8. Reconcile should scan orphan workspace dirs without deployment record (`d-20260506164303828512-0`).
9. rmdir empty deployment parent (cosmetic).
10. gemini/kiro stale telegram pickup ids cleanup.

**C. Docs + dev quality** — broad surface, lower urgency:
11. Docs sprint A (complete usage docs, audited samples) + B (zh-TW translation) (`d-20260506143714240555-1`).
12. bypass-hint helper module + agent-visible audit.
13. KIRO_TRUST_REGEX test constant dedup (#144).
14. timezone display fix.
15. discord.rs clippy.
16. 7 pre-existing flaky tests.
17. P1-3 phase-2 generic template passthrough (cheerc PR #473 seed; #137).

### 1.3 Smoke 3 finding (under verification at PLAN authoring time)

Smoke 3 dispatched dev → branch `test/smoke3-docs-only` → docs-only README change → draft PR #482. CI completed green on all 3 platforms within ~10min. Auto-watch_ci fired correctly per log (`dispatch auto-watch_ci target=dev repo=suzuke/agend-terminal branch=test/smoke3-docs-only`). However, ci_watch hit a GitHub API rate-limit window during the active poll cycle. Recovery path expected post-17:30 UTC catch-up. Outcome (PASS = catch-up notification fires; FAIL = silent drop) determines whether Sprint 54 §A1 (rate-limit recovery hotfix) is P0 or P1.

---

## §2 Goals / Non-goals

### Goals

1. **ci_watch reliability** — eliminate the two known notification-loss modes (rate-limit silent drop + multi-caller last-write-wins) so the daemon's CI feedback loop can be trusted as a first-class signal.
2. **Lifecycle edge cases** — close release_orphan_worktree + task_done auto-release + daemon-worktree-initial-base gaps so the bind/lease/release lifecycle is invariant-clean.
3. **Reconcile orphan dirs** — extend boot reconcile to sweep workspace dirs without deployment records (third tier safety net for pre-Sprint-53 leftovers).
4. **Docs sprint** — complete usage docs (English audited + Traditional Chinese) so external contributors (cheerc, songsid, etc.) can self-onboard without lead/general hand-holding.
5. **Dev-quality cleanup** — clear backlog of low-priority lint/test/regex items that have been deferred multiple sprints.

### Non-goals

- Re-architecting ci_watch as a generic event bus. The reliability work is targeted at the two known failure modes; a broader subscriber framework is Sprint 55+ if at all needed.
- Adding new feature surfaces (TUI panels, MCP tools) beyond what reliability/lifecycle work strictly requires.
- Operator-facing UX for binding/lease visibility. Log-level surface from Sprint 53 is sufficient until external feedback surfaces a pain point.
- Migrating Phase 4 GC from dry-run to enforce. That stays a separate operator-driven decision per Sprint 53 §3.

---

## §3 Architecture decisions

### 3.1 ci_watch reliability strategy

The two failure modes share an underlying architecture concern: **ci_watch is one polling loop with one subscriber slot**. Sprint 54 splits this into:

- **Poll layer**: Owns rate-limit handling, ETag caching, GraphQL / `gh` CLI fallback path. Surfaces `PollOutcome::{Pending, Success, Failure, RateLimited{retry_after}}` to caller.
- **Subscriber layer**: Owns per-recipient delivery. Multiple subscribers (lead + dev + future agents) all receive on classification change. Append-only on `ci watch` MCP call, drain on terminal classification.

This gives §A1 (rate-limit recovery — poll layer pauses + resumes from last-known-state) and §A2 (multi-caller — subscriber layer per-recipient channel) clean seams without a wholesale rewrite.

### 3.2 Lifecycle: daemon-worktree initial base

Sprint 53 dispatch hook creates worktrees from the daemon's empty `init` commit, requiring agents to `git fetch origin main && git reset --hard origin/main` before they have a usable tree. Two paths:

- **Path A (cheap)**: dispatch hook fetches origin/main first, creates worktree from FETCH_HEAD instead of init. ~5 LOC change, one network round-trip per dispatch.
- **Path B (correct)**: daemon maintains a `mirror/` ref tracking origin/main, refreshes lazily on dispatch staleness. More moving parts, but no per-dispatch network cost.

Recommendation: **Path A** for Sprint 54 (immediate UX win, small surface), defer Path B until measured pain.

### 3.3 task_done lifecycle

Currently `task done` updates the task board but does NOT release the worktree. Sprint 54 wires `task done` → `release_worktree` (via the same MCP tool path established in P0-X). Auto-release is opt-out (env or task field) for cases where dev wants to keep the worktree for follow-up work.

### 3.4 Boot reconcile expansion

Sprint 53 #475 added `reconcile_orphans(home)` to sweep deployments-without-deployment-records. Sprint 54 extends to also sweep `workspace/<name>` dirs without fleet.yaml entry AND without binding.json. The third-tier safety net catches:
- Crashed-mid-close deployments
- Manually-deleted fleet.yaml entries
- Pre-Sprint-53 artifacts from before unified close path

### 3.5 Docs sprint scoping

Sprint A (English audited): every command + flag in `docs/CLI.md` and `docs/USAGE.md` re-verified against current binary behavior. Each example block has a checked-off "verified at HEAD <SHA>" footnote. Estimate: 2-3 days, tedious but mechanical.

Sprint B (zh-TW translation): translate the Sprint A output. Use existing zh-TW retrospective notes' tone as style guide. Estimate: 1-2 days.

Both sprints land in same Sprint 54 cycle so the audit + translation co-evolve and don't drift.

---

## §4 Phase plan

| Phase | Title | Effort | Tier | Dependencies |
|---|---|---|---|---|
| **P0-1** | ci_watch poll/subscriber split | 2-3d | Tier-2 dual | none |
| **P0-2** | Rate-limit recovery (catch-up + adaptive backoff) | 1-2d | Tier-2 dual | P0-1 |
| **P0-3** | Multi-caller subscriber semantics | 1d | Tier-2 dual | P0-1 |
| **P0-4** | GITHUB_TOKEN auto-detect from `gh` + setup warning | 0.5d | Tier-1 | none |
| **P1-1** | Daemon-worktree initial base = origin/main (Path A) | 0.5d | Tier-2 | none |
| **P1-2** | task_done → release_worktree wiring | 0.5d | Tier-2 | none |
| **P1-3** | release_orphan_worktree edge case (#142) | 0.5d | Tier-1 | none |
| **P1-4** | Boot reconcile expansion (workspace/ orphan scan) | 0.5d | Tier-1 | none |
| **P1-5** | rmdir empty deployment parent | 0.25d | Tier-1 | none |
| **P1-6** | Stale telegram pickup ids cleanup | 0.5d | Tier-1 | none |
| **P2-1** | Docs Sprint A — English audit | 2-3d | Tier-1 | none |
| **P2-2** | Docs Sprint B — zh-TW translation | 1-2d | Tier-1 | P2-1 |
| **P2-3** | bypass-hint helper module + audit | 1d | Tier-2 | none |
| **P2-4** | KIRO_TRUST_REGEX dedup (#144) | 0.25d | Tier-1 | none |
| **P2-5** | timezone display fix | 0.25d | Tier-1 | none |
| **P2-6** | discord.rs clippy | 0.25d | Tier-1 | none |
| **P2-7** | 7 pre-existing flaky tests triage | 1d | Tier-1 | none |
| **DEFER** | P1-3 phase-2 generic template passthrough (#137 / cheerc #473) | 1-2d | Tier-2 | — | DEFERRED to Sprint 55, depends on settled fleet.yaml schema |

Total effort: P0 ~5-7d, P1 ~3d, P2 ~5-8d. Sprint 54 capacity: 2 weeks ≈ 10 working days. Realistic landing: P0 + P1 + P2-1 (English docs) + P2-4/5/6 (low-effort cleanup). zh-TW + bypass-hint + flaky-test triage carry to Sprint 55 if needed.

### Sequencing

- P0-1 first (architecture seam) → P0-2 + P0-3 in parallel (independent surfaces).
- P0-4 standalone, can land any time.
- P1-* mostly independent, can land any time.
- P2-1 before P2-2 (zh-TW depends on stable English).

---

## §5 Production-smoke gate

Per Sprint 53 §1.4 hard learning + d-20260506080208947938-0:

| Phase | Smoke gate |
|---|---|
| P0-1 | Dispatch dev with branch + observe poll/subscriber split via log layers |
| P0-2 | Force rate-limit (e.g. `gh api -H "X-RateLimit-Remaining: 0"` mock) → confirm catch-up fires |
| P0-3 | Lead + dev both `ci watch` same branch → both inboxes receive notification |
| P0-4 | Fresh agent_home with no GITHUB_TOKEN → confirm setup warning surfaces |
| P1-1 | Dispatch dev → confirm worktree HEAD matches origin/main (no fetch+reset needed) |
| P1-2 | Dev `task done` → confirm worktree + binding released |
| P1-3 | Trigger orphan via crash simulation → confirm release_orphan_worktree handles |
| P1-4 | Pre-Sprint-53 leftover dir → restart daemon → confirm swept |
| P1-5/P1-6 | Visual verify after operation |
| P2-1 | Each verified example block re-runs cleanly at noted HEAD |
| P2-3 | Bypass-hint message renders correctly when shim denies |

Each smoke produces a verifiable artifact (log line / file / inbox message).

### §5.1 Smoke gate classification — Path A (strict) vs Path C (parallelizable)

Sprint 53 §1.4 established that "CI green + dual VERIFIED + soak ≠ production wired" because the original PRs lacked a positive caller path in the agent workflow. The hard-learning fix was: smoke pre-merge per phase. Sprint 54 has 17 phases; serializing every phase on operator-restart-then-smoke would multiply Sprint 54 wall-clock by N restart wakeups. This subsection defines when smoke can be deferred safely vs when it MUST gate merge.

**Path C (parallelizable — dual VERIFIED + CI green = mergeable, smoke runs on next operator restart)**

Eligible only if **all three** conditions hold:
1. Phase touches no new daemon-resident wiring (refactor of existing wired system, additive notification path, or pure logic change)
2. Empirical regression-proof anchor exercises the **production code path** (not mock / not synthetic — the same fn called by production callers)
3. Reviewer dual VERIFIED + CI green on all 3 platforms

**Path A (strict — smoke MUST pass before merge)**

Required for any phase that introduces:
- New wiring (new MCP tool, new dispatch hook, new spawn site, new event subscriber)
- Lifecycle changes (bind / lease / release / worktree GC / reconcile)
- Channel changes (`comms.rs`, `channel/`, ci_watch subscriber-set semantics)
- New cross-process coordination (file locks, atomic_write contracts, etc.)

The §1.4 hard learning was about wire-class changes — Path A reproduces that discipline. Path C carves out refactor-class changes where the production code path is already exercised by tests.

**Process**

- Lead pre-marks each phase as A or C in the IMPL dispatch task summary
- Phase classifications can be challenged before IMPL begins (dev / reviewer / general)
- C phases: merge after dual VERIFIED + CI green, smoke runs on next operator restart, hotfix if smoke fails
- A phases: smoke pass is hard merge gate

**Phase classification (Sprint 54)**

| Phase | Path | Rationale |
|---|---|---|
| P0-1 ci_watch poll/subscriber split | **C** | Refactor of existing wired ci_watch; `subscriber_fan_out_notifies_every_member` exercises production `ci_check_repo` |
| P0-2 Rate-limit recovery hardening | **A** | Touches poll lifecycle + adaptive backoff timing |
| P0-3 Multi-caller subscriber semantics | **C** if scoped to subscriber-array operations on top of P0-1; **A** if it adds new dispatch-time subscription paths |
| P0-4 GITHUB_TOKEN auto-detect | **A** | New daemon-startup wiring + agent-visible warning surface |
| P1-1 Daemon-worktree initial base = origin/main | **A** | Lifecycle (worktree creation path) |
| P1-2 task_done → release_worktree wiring | **A** | New cross-tool wiring (task lifecycle ↔ worktree lifecycle) |
| P1-3 release_orphan_worktree edge case | **A** | Lifecycle |
| P1-4 Boot reconcile expansion | **A** | Lifecycle / startup wiring |
| P1-5 rmdir empty deployment parent | **C** | Pure logic addition, exercised by reconcile tests |
| P1-6 Stale telegram pickup ids cleanup | **A** | Channel state |
| P2-1 Docs Sprint A | **C** | Docs only, no daemon impact |
| P2-2 Docs Sprint B (zh-TW) | **C** | Docs only |
| P2-3 bypass-hint helper module | **A** | New shim integration surface |
| P2-4 KIRO_TRUST_REGEX dedup | **C** | Pure refactor of test constant |
| P2-5 timezone display fix | **C** | Pure logic on existing render path |
| P2-6 discord.rs clippy | **C** | Lint cleanup, no behavior change |
| P2-7 Pre-existing flaky tests triage | **C** | Test-only |

References: `d-20260506171720519048-2` (empirical regression-proof discipline), `d-20260506080208947938-0` §1.4 hard learning.

---

## §6 Risks

1. **ci_watch refactor scope creep** — split into poll + subscriber may invite "while we're in there" expansion. Mitigation: write the architecture seam as the first commit, fence subsequent work.
2. **Rate-limit testing is fragile** — depends on GitHub API quirks. Mitigation: use `gh api` mocks for unit tests; production-smoke gate uses real rate-limit headers.
3. **Docs audit is tedious** — easy to skip half-way through. Mitigation: tracker checklist in PR description, every example explicitly verified.
4. **Sprint 53 hotfix tail** — if rate-limit recovery proves to be a P0 hotfix (Smoke 3 FAIL), it lands BEFORE Sprint 54 starts and shrinks Sprint 54 P0-2 scope to subscriber-only-fix. Mitigation: Smoke 3 verdict is the start signal.

---

## §7 Open questions for operator review

1. **P2 priority within Sprint 54** — should bypass-hint helper module land Sprint 54 (better external-contributor onboarding) or wait until external feedback surfaces a pain point?
2. **Docs Sprint B (zh-TW)** — is a 1-2d translation pass sufficient, or should we use a lighter touch (machine translation + human review of key sections)?
3. **task_done auto-release default** — opt-in (explicit `release: true`) or opt-out (release by default, `keep_worktree: true` opts out)? Operator workflow preference matters here.
4. **DEFER list** — confirm P1-3 phase-2 (#137 / cheerc #473) defers to Sprint 55. If we want it Sprint 54, P0-3 multi-caller should pair with it.

---

## §8 References

- Sprint 53 PLAN: PR #463 (`docs/proposals/sprint-53-wire-and-cleanup.md`)
- Sprint 53 retrospective decisions:
  - `d-20260506171720519048-2` — empirical regression-proof discipline
  - `d-20260506171736738779-3` — release_worktree single source of truth
  - `d-20260506171805866878-4` — cleanup lifecycle three-tier layering
- FLEET-DEV-PROTOCOL-v1.md §13 (AGEND_GIT_BYPASS=1 usage)
- Decision board Sprint 54 candidates:
  - `d-20260506164303828512-0` — reconcile orphan workspace dirs
  - `d-20260506155323776106-0` — ci_watch multi-caller append subscribers
  - `d-20260506143714240555-1` — Docs sprint A+B
  - `d-20260506171309264856-1` — GITHUB_TOKEN auto-detect
- Sprint 54 follow-up tasks: #137, #142, #144, #152
