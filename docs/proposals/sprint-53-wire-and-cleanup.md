# Sprint 53 — Wire Phase 1-5 + Cleanup

**Date**: 2026-05-06
**Owner**: lead
**Status**: PLAN (operator review pending — no IMPL until approved)
**Source-of-truth**: `origin/main` HEAD `25ce65e` (post Hotfix F merge)
**Predecessor sprints**: Sprint 52 router-layer (#437/#440), agend-git-shim Phase 1-5 (#446–#455), Hotfix A-F (#436/#451/#457/#458/#459/#461)

---

## §1 Background

### 1.1 What Sprint 52 + Hotfix A-F shipped

- **agend-git-shim Phase 1-5**: trailer hook, Rust shim with deny matrix, worktree lease/release, GC dry-run, hotspot detection — all merged with dual-tier review + CI green + invariant tests.
- **6 hotfixes**: retry header dedup (A), provenance truncate (B), CI auto-watch on dispatch (C #451), app-mode wiring (D #457), ci-watch grace (E #458), ci-watch classification root cause (F #461) — plus Issue #456 teardown cleanup.

### 1.2 What the smoke test exposed (2026-05-06)

A trivial CHANGELOG entry PR was dispatched as end-to-end verification per operator §13 production-smoke gate. **5 of 6 checklist items failed**:

| # | Check | Result |
|---|-------|--------|
| 1 | Worktree at `$AGEND_HOME/worktrees/` + `.agend-managed` marker | ❌ Lease path never invoked |
| 2 | `Agend-Agent: dev` trailer in commit | ❌ No binding, hook didn't fire |
| 3 | `ci-watches/` auto-fire on dispatch (Hotfix C) | ❌ Empty post-dispatch |
| 4 | Fresh branch (avoid stale PR) | ✅ |
| 5 | ci-pass notification fires | ❌ (Hotfix F since fixed) |
| 6 | Worktree release marker preserved | ❓ N/A — lease never invoked |

Hotfix F (#461) closed item 5 at the classification layer. Items 1, 2, 3, 6 all share a common root cause: **Phase 1-5 shipped binaries with no caller in the agent workflow**. They sit as DENY guards but no positive path exists for agents to BIND or LEASE.

### 1.3 Live demonstration during this PLAN's authoring

While drafting this document, lead's `git worktree add` was denied by the shim:

```
agend-git: ERROR git worktree denied
           agent=lead, reason: fleet-managed — use agend-terminal worktree tools
           HINT: use the task board to get a worktree assignment, or set AGEND_GIT_BYPASS=1 for emergency override
```

The hint is aspirational — there is no "agend-terminal worktree tools" that an agent can call. This PLAN's author had to set `AGEND_GIT_BYPASS=1` to proceed, the same workaround dev used during the smoke test. The shim's denial is structurally correct; the workflow it gates is not yet built.

### 1.4 Hard learning

Three CI-green Tier-2 dual-VERIFIED PRs (#446, #447, #449) shipped what amounted to dead code, plus two more (#454, #455). The gating defects survived:

- 5 cargo test suites passing
- 8B param soak tests
- 2 reviewer VERIFIED verdicts per phase
- CI on all 3 platforms green
- Source-grep regression tests
- Lock-tier invariant tests

**`cargo test green + dual VERIFIED + soak ≠ production wired.`** The Sprint 49 cushion (pre-IMPL invariant test gate) caught the deadlock-class regression in PR-B r1 before merge. It did not catch dead-code-class regressions because none of the tests exercised the actual production entry point — `app::run_app` for the user-facing CLI. The test seam was always `daemon::run`.

Hotfix D (#457) was the corrective: the seam moved into `bootstrap::prepare`. But the deeper learning is that **a green test suite without a wired entry point is a coverage illusion**. Every phase needs a "who calls this in prod?" trace before merge.

## §2 Goals / Non-goals

### Goals

1. Wire `binding::bind` + `worktree_pool::lease` + `worktree_pool::release` into the live agent workflow so Phase 1-5's deny guards become positively useful.
2. Fix Hotfix C non-fire: agent-to-agent dispatch (`send` from lead-to-dev with `branch` field) does not currently trigger auto-watch_ci. Either repair the parse path or replace with explicit watch wired into the same delegate_task entry point.
3. Adopt a production-smoke test gate that exercises the actual entry point for every phase. CI green + dual VERIFIED is necessary but not sufficient.
4. Close P1 follow-ups: Issue #450 (cheerc fleet.yaml template fields) + MCP `gc-dry-run` operator-callable.

### Non-goals

- Re-architecting the shim itself. Phase 1-5 implementations are correct — only the entry-point wiring is missing.
- Building a generic plugin/extension system for future hooks. Wire what exists; defer abstraction.
- Operator-side TUI changes for binding visibility. CLI/log surface is sufficient for Sprint 53.
- AGEND_WORKTREE_GC=1 cutover. Phase 4 GC stays dry-run until separate operator wake decision.

## §3 Architecture decisions

### 3.1 Bind/lease entry point

Two viable seams:

| Option | Where | Cost | Lifecycle clarity |
|--------|-------|------|-------------------|
| A | Daemon-side: parse `delegate_task` payload, auto-invoke `bind` + `lease` when `branch` field present | Medium — wires into existing dispatch parser | High — bind lifetime = task lifetime, mirrors task_id boundary |
| B | Agent-side: new MCP tool `bind_worktree` / `release_worktree`, agent calls explicitly | Low — additive only | Lower — agents must remember to call, easy to forget |
| C | Both: A as default, B as escape hatch for explicit operator/agent control | High — two paths to maintain | Highest, but premature |

**Decision**: Option A.

Rationale:
- The smoke test failed precisely because there was no automatic step. Asking agents to remember to call `bind_worktree` first repeats the failure mode.
- `delegate_task` already carries `branch` field per Hotfix C #451 design. The dispatch path is the natural seam.
- Lifecycle is clean: bind on dispatch, release on task done / claim release / agent restart.

Option C is deferred to Sprint 54 if operator-callable explicit control becomes useful.

### 3.2 Bind/release lifecycle

```
delegate_task (lead → dev, branch=B, task_id=T)
  └─ daemon parses dispatch
     └─ if branch present:
        ├─ binding::bind(home, agent=dev, task_id=T, branch=B)  // writes binding.json
        └─ worktree_pool::lease(home, repo, agent=dev, branch=B)
           └─ creates $AGEND_HOME/worktrees/<id>/ + .agend-managed marker

dev IMPL → commit (Phase 1 hook reads binding.json → writes Agend-Agent trailer)
dev push + PR → merge

task done (dev → lead, kind=report)
  └─ daemon parses report
     └─ worktree_pool::release(home, lease)  // writes released_at, preserves marker
        └─ Phase 4 hourly GC sweep eventually reaps (gated AGEND_WORKTREE_GC=1)

binding::unbind on:
  - task_id matched done report
  - agent restart (orphan reconcile)
  - explicit teardown
```

### 3.3 Failure recovery

Bind/lease can fail — disk full, branch already leased to another agent, fork repo without write access. Failure modes:

| Failure | Behavior |
|---------|----------|
| Bind file write error | Log warn, dispatch proceeds without bind. Trailer absent (graceful degrade). |
| Lease conflict (branch already leased to another agent) | Reject dispatch with explicit error. Operator visible. Prevents two agents racing same branch. |
| Lease creation fails (disk, permission) | Reject dispatch. Operator gets actionable error. |
| Daemon restart between bind and release | Reconcile path on startup: read all binding.json + worktree pool, mark orphans (already exists per Phase 3, just needs to actually run). |

The bind/lease step is **best-effort for binding, hard-fail for lease**. Bind failure (just a JSON file) shouldn't block the task; lease failure (a real worktree) should, because the agent has nowhere to work.

### 3.4 Hotfix C non-fire root cause

Smoke test evidence: lead-to-dev `send` with `branch` field did not produce a `ci-watches/` entry. Two hypotheses:

- **H1**: Hotfix C parses operator-side dispatches only (e.g., from `general` proxy or via fleet.yaml init), not agent-to-agent `send` MCP tool calls.
- **H2**: The branch field is in `send` payload but Hotfix C reads from a different envelope (e.g., delegate_task structured event).

Resolution under Sprint 53 P0-1:

When the daemon-side `delegate_task` parser auto-invokes bind+lease (decision §3.1), it can invoke `watch_ci` on the same path. This makes Hotfix C either redundant or repairs it incidentally — both acceptable. Investigation step in §4 P0-2 will determine whether to delete Hotfix C #451 or merge its logic into the new dispatch hook.

### 3.5 Production-smoke test gate (per phase, mandatory)

Every Sprint 53 phase PR MUST include:

1. **Unit tests**: existing pattern, fast, mock-heavy. Required.
2. **Integration tests**: real daemon harness, real file system, no mocks for the seam under test. Required.
3. **Production-smoke artifact**: a script or test that exercises the actual user-facing entry point (`app::run_app` for CLI, or a real `delegate_task` dispatch for daemon paths) and asserts the expected files / log lines / state mutations. Required.

The 3rd category is what was missing in agend-git-shim Phase 1-5. It is non-negotiable for Sprint 53.

## §4 Phases

Each phase is a small testable PR. Total expected: 4 PRs for P0+P1, defer P2 unless operator wakes them.

### P0-1 — Daemon dispatch hook auto-binds and auto-leases

**Branch**: `feat/dispatch-auto-bind-lease`
**Tier**: Tier-2 dual review (touches dispatch parser + binding + lease, all critical-path)
**Files**: `src/daemon/dispatch.rs` (or wherever `delegate_task` is parsed), `src/binding.rs` (additive), `src/worktree_pool.rs` (additive)
**LOC est**: ~150-200

Tasks:
- Locate dispatch parse site (likely `src/daemon/dispatch.rs` or `src/mcp/handlers/send.rs`)
- Add post-parse hook: when `branch` present + recipient is fleet agent (not operator), invoke `binding::bind_full` + `worktree_pool::lease`
- Wire failure recovery per §3.3
- Production-smoke test: real daemon, real `send` MCP call, assert `binding.json` exists + `$AGEND_HOME/worktrees/<id>/` exists + `.agend-managed` marker present
- Integration test: lease conflict (two dispatches same branch) rejected
- Unit tests: bind/lease invocation paths

### P0-2 — Hotfix C non-fire investigation + consolidation

**Branch**: `fix/hotfix-c-dispatch-watch-consolidation`
**Tier**: Tier-1 single primary
**Files**: `src/daemon/ci_watch.rs`, dispatch hook from P0-1
**LOC est**: ~30-50

Tasks:
- Trace why `send` agent-to-agent does not trigger Hotfix C
- If P0-1's dispatch hook already covers ci-watch invocation, delete Hotfix C #451's old auto-watch path (it would be dead code)
- If Hotfix C still serves a distinct purpose (e.g., operator-side dispatches), document the boundary explicitly + add comment
- Production-smoke test: lead-to-dev `send` with branch → assert `ci-watches/` entry created within 5s
- Integration test: covers operator-side, agent-to-agent, and missing-branch cases

P0-2 depends on P0-1; sequence them.

### P1-3 — Issue #450 fleet.yaml template fields expand

**Branch**: `feat/issue-450-fleet-yaml-template-fields`
**Tier**: Tier-1 single primary
**Files**: `src/config/fleet_yaml.rs` (or InstanceYamlEntry definition), `src/deployments.rs` (deploy passes through)
**LOC est**: ~80-120

Tasks:
- Per Issue #450 (cheerc) — extend `InstanceYamlEntry` to include `args`, `model`, `env`, `ready_pattern`
- Modify `deploy()` to pass through the fields
- Define dynamic-fields shadow precedence (yaml override vs runtime override)
- Production-smoke test: deploy with extended fields → assert daemon spawns with correct args/model/env
- Issue link: https://github.com/suzuke/agend-terminal/issues/450

### P1-4 — Phase 4 MCP `gc-dry-run` operator-callable tool

**Branch**: `feat/mcp-gc-dry-run-tool`
**Tier**: Tier-1 single primary
**Files**: `src/mcp/handlers/worktree_gc.rs` (new), `src/worktree_pool.rs` (existing `gc_dry_run` exposure)
**LOC est**: ~50-80

Tasks:
- Add MCP tool wrapping existing `worktree_pool::gc_dry_run`
- Operator can call directly without grepping logs
- Production-smoke test: create stale lease → call MCP tool → assert candidate appears in result, marker preserved
- No deletion (still dry-run only — `AGEND_WORKTREE_GC=1` cutover separate operator decision)

### P2 (deferred unless operator wakes)

5. fleet.yaml schema_version
6. kanban view
7. leak process cleanup
8. Sprint 50 post-push hook

These are queue items, not Sprint 53 commitments.

## §5 Test gates per phase

| Phase | Unit | Integration | Production smoke |
|-------|------|-------------|------------------|
| P0-1 | bind/lease invocation paths | lease conflict rejection, daemon restart reconcile | real `send` MCP → assert binding.json + worktree dir + marker |
| P0-2 | dispatch parse paths | operator-side + agent-to-agent + missing branch | lead-to-dev `send` → ci-watches entry within 5s |
| P1-3 | yaml field parse | deploy passthrough | deploy → spawn assertion |
| P1-4 | MCP handler | dry-run candidate generation | stale lease → MCP call → candidate appears |

Production smoke is the merge gate. CI green + reviewer VERIFIED + production smoke green = ready to merge.

## §6 Risks

- **R1 — Lease conflict UX**: when two agents are dispatched same branch (rare but possible), how does the second dispatch surface failure? Operator-readable error vs silent ignore. **Mitigation**: explicit error message in dispatch reply, not just log.
- **R2 — Bind/lease at parse-time blocks dispatch latency**: file ops are fast but if disk slow, dispatch path stalls. **Mitigation**: tokio::spawn the lease step (fire-and-forget with comment per §10.5), keep dispatch path non-blocking.
- **R3 — Reconcile on daemon restart leaks worktrees**: existing `reconcile_orphan_leases` only marks orphans, doesn't reap. With auto-lease the worktree pool grows. **Mitigation**: P1-4's MCP tool gives operator visibility; cutover gate for actual deletion.
- **R4 — Hotfix C deletion regression**: if Hotfix C is deleted in P0-2 but it covered an unobserved code path, ci-watch coverage shrinks. **Mitigation**: P0-2 adds tests for all 3 paths (operator-side, agent-to-agent, missing-branch) before deletion. If a path can't be tested, it stays.
- **R5 — Production smoke flake**: real daemon startup + file ops are slower and more brittle than unit tests. **Mitigation**: dedicated `cargo test --features production-smoke -- --test-threads=1` profile, gated behind a feature flag, runs in nightly CI + manual merge gate.
- **R6 — Issue #450 yaml field collision**: if user yaml has unexpected key, schema parse fails. **Mitigation**: serde untagged or default-on-missing, plus migration test.
- **R7 — Sprint 53 itself ships dead code**: meta-risk. **Mitigation**: every phase exits with operator running the smoke test manually before merge approval. No exceptions.

## §7 Alignment items

- **Sprint 49 cushion compliance**: every phase PR adds source-grep + lock-tier-assert checks. P0-1 dispatch hook spawns at most one fire-and-forget for the lease step — needs `// fire-and-forget: lease creation can stall on slow disk, dispatch path stays hot` comment per §10.5.
- **Worktree-policy CLAUDE.md**: agent worktrees go to `$AGEND_HOME/worktrees/<id>/`, never reuse main checkout. P0-1 enforces this through `worktree_pool::lease`'s existing logic.
- **Test-parallel race check** (per global feedback memory): every phase test must run cleanly under `--test-threads=1` and default parallel.
- **Channel discipline (Sprint 52)**: dispatch hook MUST NOT acquire L1/L2 from a router thread. Lease + bind run on the dispatch parser thread, which is already L1-safe by Sprint 52 invariant 1. Add invariant test per phase that touches dispatch.
- **GitHub-state classification (Hotfix F)**: Hotfix C consolidation in P0-2 should preserve the `closed_at` freshness discriminator semantics from `src/daemon/ci_watch.rs`. Don't reintroduce stale-PR misclassification.

## §8 Estimates

| Phase | Worktree+plan | IMPL | Review cycle | CI + smoke | Total |
|-------|--------------|------|--------------|------------|-------|
| P0-1 | 30 min | 2-3 h | 1-2 h (Tier-2 dual) | 30 min | ~4-6 h |
| P0-2 | 15 min | 45 min | 30 min | 15 min | ~2 h |
| P1-3 | 15 min | 1-2 h | 30 min | 15 min | ~2-3 h |
| P1-4 | 15 min | 1 h | 30 min | 15 min | ~2 h |

**Total Sprint 53 P0+P1**: ~10-13 hours, distributed across 2 sessions.

P2 items deferred and not estimated.

## §9 Sequence

1. Operator review of this PLAN doc → operator approval
2. P0-1 dispatch (Tier-2 dual)
3. P0-1 merge → P0-2 dispatch (Tier-1)
4. P0-2 merge → smoke test re-run (CHANGELOG entry redo) — should now pass 6/6
5. P1-3 + P1-4 dispatched in parallel (independent, both Tier-1)
6. Sprint 53 close report

## §10 §13 questions — operator answers + rationale

Operator m-8: "全採用 lead 建議" for Q1-3; full delegation to lead on Q4-8.

1. **Bind failure tolerance** — **GRACEFUL.** Bind file is convenience metadata; if write fails, dispatch proceeds without trailer rather than blocking the task. Lease is the load-bearing artifact and stays hard-fail per §3.3.
2. **Lease conflict policy** — **REJECT second dispatch.** Two agents on same branch is a coordination bug, not a feature. Surface as explicit operator-readable error so the conflict is visible, not silently aliased.
3. **Bind lifetime** — **per-task.** Bound on `delegate_task` parse, released on matching `task done` report. Matches `task_id` boundary cleanly. Per-agent-session would leak across tasks and complicate cleanup on agent restart.
4. **Hotfix C disposition** — **delete after P0-2 confirms coverage.** Dead code is liability. P0-2 must add tests for all three paths (operator-side dispatch, agent-to-agent dispatch, missing-branch) before deletion. If any path resists testing, Hotfix C stays as defense-in-depth and the gap is documented inline.
5. **Production smoke CI cost** — **nightly + manual merge gate**, not every PR. Real-daemon spin-up adds 30-60s per test, and CI green on every PR didn't catch dead code anyway. Per-PR smoke would burn cycles without changing the failure mode. Manual operator review at merge time forces deliberate inspection of the smoke output, which is the actual mitigation. Sprint 53 phases each use manual smoke per phase merge.
6. **Phase 4 GC cutover timing** — **defer to Sprint 54.** `AGEND_WORKTREE_GC=1` deletes worktrees; we need ≥1 week of dry-run logs to confirm no false-positives in production. Sprint 53 close is too early — we'd be cutting over with hours of soak, not days.
7. **MCP gc-dry-run output format** — **human-readable default, JSON via flag.** Operator interactive use is primary; tooling is secondary. Matches existing MCP tool patterns (e.g., `task list`).
8. **P2 wake** — **none.** P0+P1 is already 10-13h. P2 items are nice-to-haves; budget discipline matters more than scope creep. Sprint 54 pulls from the P2 queue based on signal at that point. Specifically:
   - fleet.yaml schema_version: defer (no incompatible change planned in Sprint 53)
   - kanban view: defer (UX polish, not blocking)
   - leak process cleanup: defer (no recent operator complaint, can wait)
   - Sprint 50 post-push hook: defer (workflow already works without it)

## §11 Estimates summary

- This PLAN doc PR review: ~30 min
- P0+P1 IMPL total: ~10-13 hours
- P2 deferred

---

**End of PLAN — awaiting operator §10 answers + approval before P0-1 IMPL dispatch**
