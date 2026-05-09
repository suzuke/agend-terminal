# Sprint 60 PLAN — Multi-Candidate Inventory + Dispatch Shape

**Date**: 2026-05-10
**Author**: lead
**Status**: PLAN draft v0 (awaiting general review + operator scope-appetite ruling)
**Source-of-truth**: `origin/main` HEAD `361fada` (Sprint 59 Wave 2 PR-3 #576 just merged、closing Engineering anti-stall arc 7-PR layered)
**Auto-trigger**: per operator-approved (A) auto-dispatch sprint PLAN on prior closeout (general dispatch m-20260509185129186761-340)

---

## §0 Context

**Sprint 59 closeout** (2026-05-09) shipped 9 PRs (Wave 1: 5 + Wave 2: 4) including:
- Engineering anti-stall arc completion (7 PRs / 5 layers across Sprint 58→59)
- 4-tracker supervisor coexistence (AntiStallTracker + IdleWatchdogTracker + DecisionTimeoutTracker + HelperStalenessWatchdog)
- Operator decisions Q1=(b) ledger preservation + Q2=(C) bypass-free permanent protocol
- Wave 2 PR-1 4-revision stale-content reviewer diagnostic learning
- Wave 2 PR-IMPL teloxide API gap → F2 track-on-create scope reduction

**Sprint 60 candidate pool** — 16+ items accumulated through Sprint 58→59 surface-blocks + protocol-audit + operator surfaces。

**Capacity baseline**：~10 PRs per Sprint per Sprint 58/59 baseline。Sprint 60 should aim for similar or slightly lower if infrastructure work dominates。

**Sprint 60 theme** (proposed): **infrastructure hardening + Q2=(C) automation gap closure**。Most P0 candidates are direct responses to Wave 1 PR-2 BYPASS incident root cause + chicken-and-egg edge cases that surfaced during Sprint 59 cycles。

---

## §1 Goal

Sprint 60 ships infrastructure that:
1. Closes the `bind_self` lease_failed edge case that forced PR-2 incident's BYPASS workaround (P0 #1)
2. Eliminates daemon-restart requirement post-MCP-tool-add (P0 #2)
3. Provides operator-not-at-computer SPOF recovery (P0 #3)
4. Cohesive PR conflict resolution mechanism (P0 #4、subset of #1)
5. Optional Skills System backend integration (P1 #5、capacity-permitting)
6. Optional Bopomofo IME Shape A IMPL (P1 #6、operator-gate pending cross-backend confirmation)

**Non-goals**:
- teloxide upgrade (P2、deferred pending API surface investigation)
- LOC estimation methodology overhaul (P2、process improvement、async with implementation)
- parallel filler safety reactivation (P2、gated by P0 #1 bind_self rebase mode shipping first)

---

## §2 Verified state (origin/main 361fada)

Engineering anti-stall arc landed (7 PRs)：
- `src/daemon/anti_stall.rs` — task ETA stall watchdog (#567)
- `src/daemon/idle_watchdog.rs` — fleet-idle watchdog (#568)
- `src/daemon/decision_timeout.rs` — operator-decision timeout (#572)
- `src/daemon/helper_staleness_watchdog.rs` — deployment cadence proactive (#576)
- `src/mcp/handlers/force_release.rs` — bind_self lease_failed escape hatch (#571)
- `docs/PROCESS-LEAD-CLOSEOUT-CLAIM-STATE-DISCIPLINE.md` — claim-state narrative (#569)
- `src/mcp/tools.rs` — kind=task task_id required (#566)

Wave 2 telegram topic cleanup landed:
- `src/bootstrap/doctor_topics.rs` — 4-class taxonomy (#574)
- `src/channel/telegram/bootstrap.rs` — track-on-create refactor (#574)
- `src/channel/telegram/topic_registry.rs` — DeleteTopicOutcome + permission helper (#574)

RCA docs landed:
- `docs/RCA-TELEGRAM-TOPIC-CLEANUP.md` (#573)
- `docs/RCA-BOPOMOFO-IME-CURSOR-REGRESSION.md` (#575)

**Sprint 60 base**: HEAD 361fada、CI green、no in-flight PRs、all worktrees released。

---

## §3 Inventory — P-ranking

### P0 — Infrastructure (4 candidates、ship-priority)

**Total P0 estimate**：~330-650 LOC + tests = roughly half of Sprint 60 capacity envelope

#### P0-1. `bind_self` rebase mode

- **Source**: Wave 1 PR-2 BYPASS incident root cause + Wave 2 PR-1 (P3) recovery learning
- **Symptom**: `bind_self` refuses with `lease_failed` when daemon in-memory worktree state stuck after merge-conflict + on-disk cleanup
- **Fix shape**: daemon-managed stale-worktree handling without operator-restart - rebase mode flag on `bind_self` that releases stale lease + rebinds in single atomic operation
- **LOC est**: ~50-100 prod + ~30-60 test = ~80-160 total
- **Tier**: Tier-1 single primary
- **Path**: Path A IMPL with smoke (verifies stale-state recovery)
- **Files**: `src/mcp/handlers/bind.rs` (~30-50 LOC) + `src/daemon/worktree.rs` (~20-50 LOC) + tests
- **Dependencies**: none
- **Operator gate**: NO — general self-decide (small scope、direct response to operator-acknowledged incident)
- **ETA**: ~1.5-2hr

#### P0-2. daemon hot-reload tool registry

- **Source**: Wave 1 PR-5 → PR-4 chicken-and-egg (force_release_worktree helper required daemon restart to load post-MCP-tool-add)
- **Symptom**: Adding new MCP tool requires daemon restart to pick up; restart adds friction + risk during emergency unblock cycles
- **Fix shape**: scan `src/mcp/tools.rs` registry on schedule OR explicit reload endpoint OR file-watch on tools.rs
- **LOC est**: ~150-250 prod + ~80-120 test = ~230-370 total
- **Tier**: Tier-1 single primary, possibly Tier-2 if reload semantics involves state reconciliation
- **Path**: Path A IMPL with smoke (verifies new tool callable without restart)
- **Files**: `src/daemon/mcp_registry.rs` (NEW、~100 LOC) + `src/daemon/supervisor.rs` (+5 wire-in) + `src/mcp/handlers/admin.rs` (~30 LOC for reload endpoint) + tests
- **Dependencies**: optional dep on P0-1 (stale-state cleanup synergy) but not blocker
- **Operator gate**: NO — general self-decide
- **ETA**: ~2-3hr

#### P0-3. operator restart MCP tool

- **Source**: Wave 1 PR-4 (P3) recovery surfaced operator-not-at-computer SPOF (operator had to manually decide P3 abandon when chicken-and-egg hit)
- **Symptom**: Operator-restart-required edge cases create blocking SPOF if operator unavailable
- **Fix shape**: MCP tool that triggers controlled daemon restart with state preservation hooks
- **LOC est**: ~80-150 prod + ~50-80 test = ~130-230 total
- **Tier**: Tier-1 single primary
- **Path**: Path A IMPL with smoke (verifies controlled restart preserves state)
- **Files**: `src/mcp/handlers/restart.rs` (NEW、~80 LOC) + `src/mcp/tools.rs` (+1 tool entry, count 31 → 32) + `src/daemon/lifecycle.rs` (~40 LOC) + tests
- **Dependencies**: synergy with P0-2 (hot-reload reduces restart need but P0-3 covers cases where restart IS needed)
- **Operator gate**: NO — general self-decide
- **ETA**: ~1.5-2hr

#### P0-4. PR conflict resolution mechanism (subset of P0-1)

- **Source**: Q2=(C) bypass-free protocol + Wave 1 PR-2 incident retrospective
- **Symptom**: rebase conflict requires manual daemon 3-step + force-push、no automation guard against partial-state errors
- **Fix shape**: bundled into P0-1 OR separate helper - automation around `release_worktree → bind_self → rebase → push` flow
- **LOC est**: ~50-100 prod + ~30-50 test = ~80-150 total (overlaps with P0-1 scope)
- **Tier**: Tier-1 single primary
- **Path**: Path A IMPL with smoke
- **Files**: same as P0-1 + possibly `src/mcp/handlers/conflict_resolution.rs` (new helper) + tests
- **Dependencies**: combine with P0-1 OR ship as follow-up
- **Operator gate**: NO — general self-decide
- **ETA**: ~1-1.5hr if combined with P0-1, ~2hr if separate

**P0 dispatch shape recommendation**: Wave 1 = P0-1 + P0-4 combined PR (~150-250 LOC) → Wave 1 = P0-2 (~200 LOC) → Wave 1 = P0-3 (~130-230 LOC) sequential。Total Wave 1 ETA ~5-7hr。

### P1 — Backlog (2 candidates)

**Total P1 estimate**：~700-1100 LOC (Skills System) + ~5 LOC (Bopomofo IME) — Skills System dominates

#### P1-1. Skills System Plan

- **Source**: codex-verify APPROVED 2026-05-02 + operator confirmed P1 candidate 2026-05-09 telegram「P1候選」
- **Goal**: 5-backend (Claude Code / Codex / Gemini / OpenCode / Kiro CLI) social skill discovery via mattpocock/skills SKILL.md protocol
- **Fix shape**:
  - Unified source `~/.agend-terminal/skills/` with symlink (Windows copy + staleness check fallback)
  - skills-lock.json version pinning
  - CLI commands: `agend skills add/remove/list/update`
  - fleet.yaml per-instance skills specification
- **LOC est**: ~500-700 prod + ~150-300 tests + ~50-100 Windows fallback = total ~700-1100 LOC including tests
- **Tier**: Tier-1 single primary baseline、possibly Tier-2 if cross-platform branches surface complexity
- **Path**: Path A IMPL with smoke (5-backend × symlink/copy paths × Windows = many edge cases、test coverage heavy)
- **Files**: `src/skills.rs` (NEW、main module) + `src/instructions.rs` (+50 LOC integration) + `src/mcp/mod.rs` (+30 LOC if MCP surface needed) + `src/cli.rs` (+~100 LOC for CLI subcommands) + Windows-specific copy-fallback module + extensive tests
- **Dependencies**: none hard、but release-order recommendation ship after P0 stable (per general analysis)
- **Operator gate**: NO — general self-decide (but flag if scope materially exceeds 1100 LOC)
- **Reference docs**: 
  - mattpocock/skills protocol: https://github.com/mattpocock/skills
  - OpenCLI (19.4K stars) validates direction: https://github.com/jackwener/OpenCLI
- **ETA**: ~4-6hr (largest single PR estimate)

#### P1-2. Bopomofo IME IMPL Shape A — ⚠ operator-gate pending

- **Source**: RCA #575 (Wave 2 PR-2) + operator-reported regression issue #532
- **Symptom**: Claude Code agent pane cursor not focused on command line during Bopomofo IME composition
- **Fix shape**: Shape A — drop scroll-offset half of cursor-emit gate at `src/render/core_render.rs:400` (~5 LOC change、unwinds old over-restriction、ratatui already clamps cursor to inner-rect via lines 404-406)
- **LOC est**: ~5 prod + ~10-20 test = ~15-25 total
- **Tier**: Tier-1 single primary
- **Path**: Path A IMPL with manual smoke (operator runs IME composition test post-merge)
- **Files**: `src/render/core_render.rs` (-1 line + minor adjustment at line 400) + tests
- **Dependencies**: ⚠ **OPERATOR-GATE PENDING**：cross-backend reproduction confirmation (Claude Code / Codex / Kiro / Gemini)
  - If only Claude Code reproduces → backend-specific contamination → operator triage required → may move to backend-specific fix
  - If all four reproduce → Shape A IMPL ships under general self-decide
- **Operator gate**: YES — cross-backend reproduction confirmation required pre-IMPL dispatch
- **ETA**: ~30min once operator confirms

**P1 dispatch shape recommendation**: 
- P1-1 Skills System → W3 OR Sprint 61 W1 depending on Wave 1 P0 completion timing (per general analysis: don't compete with W1 P0 infra)
- P1-2 Bopomofo IME → ASAP post operator confirmation (could fit any wave、tiny scope)

### P2 — Deferred (13+ candidates)

#### P2-A. Sprint 58 deferred items (8)

[List from Sprint 58 closeout deferred-items roster — verbatim catalog from Sprint 58 PLAN deferred section]
- **A1-A8**: per Sprint 58 closeout synth deferred items (specific items need restoration from Sprint 58 lead memory)

**Suggested handling**: re-evaluate during Sprint 60 W3-W4 after P0+P1 complete、ship 1-2 if capacity permits。Else Sprint 61 candidate pool。

#### P2-B. Protocol-audit candidates (4)

[List remaining 4 protocol-audit candidates post-#566 resolution]
- **B1-B4**: per Sprint 58 protocol-audit catalog remaining items

**Suggested handling**: bundle into single doc/RCA PR if scope per-item is small。

#### P2-C. teloxide 0.13.0 upgrade evaluation + chat-side forum-topic enumeration (NEW Sprint 59 W2)

- **Source**: Wave 2 PR-IMPL F2 deferral (teloxide 0.11.2 lacks forum-topic enumeration API、F2 track-on-create shipped instead but ungettable `stale_chat` class remains a known limitation)
- **Goal**: evaluate teloxide 0.13.0 (or successor) for chat-side enumeration support; if available、unlock 5-class (γ) taxonomy + post-hoc duplicate detection
- **Fix shape**: 
  - Phase 1: dependency-bump evaluation PR (sweep telegram-related code for breaking changes)
  - Phase 2: if enumeration available、implement (γ) `stale_chat` class detection + (α-b) live-chat orphan-cleanup extension
- **LOC est**: 
  - Phase 1: ~100-200 LOC (dep upgrade + breaking change adjustments)
  - Phase 2: ~150-250 LOC (if enumeration available)
- **Tier**: Tier-1 single primary
- **Path**: Path B RCA-first → Path A IMPL conditional
- **Operator gate**: NO — general self-decide

#### P2-D. LOC estimation methodology improvement + ceiling enforcement protocol (NEW Sprint 59 W2)

- **Source**: Wave 2 overage learnings (PR-IMPL 3-5x estimate miss + PR-3 1.65x estimate miss)
- **Goal**: systematic estimation framework + ceiling enforcement protocol
- **Fix shape**:
  - Estimation framework: separate boilerplate / test density / new vs refactor distinction
  - Ceiling enforcement: hard-fail on >150% overrun? soft-warn on >130%? automated CI check?
- **LOC est**: ~50-100 prod (CI script + estimation methodology doc) + ~30-50 test = ~80-150 total
- **Tier**: Tier-1 single primary
- **Path**: Path B RCA + Path A doc/CI integration
- **Operator gate**: NO — general self-decide

#### P2-E. Parallel filler safety post bind_self rebase mode (NEW Sprint 59 W2)

- **Source**: Sprint 59 sequential dispatch decision (Q2=(C) lease-thrash protection per memory `feedback_filler_pr_file_overlap_audit.md`)
- **Goal**: re-evaluate parallel filler dispatch safety post P0-1 bind_self rebase mode shipping
- **Fix shape**:
  - Document parallel-feasibility criteria
  - Smoke test parallel filler PR cycle to verify lease/rebind state machine
  - Decision: re-enable parallel filler-during-reviewer-wait OR maintain sequential default
- **LOC est**: ~20-50 LOC (mostly doc + 1-2 smoke tests)
- **Tier**: Tier-1 single primary
- **Path**: Path B RCA + Path A smoke test
- **Operator gate**: NO — general self-decide
- **Dependencies**: requires P0-1 bind_self rebase mode shipped first

**P2 dispatch shape recommendation**: cherry-pick from P2 list based on Sprint 60 capacity envelope。Aim for 0-2 P2 items in Sprint 60、defer rest to Sprint 61。

---

## §4 Dependencies graph

```
P0-1 (bind_self rebase mode)
  ├─→ P0-4 (PR conflict resolution、subset/companion)
  └─→ P2-E (parallel filler safety)

P0-2 (daemon hot-reload) ──[synergy]── P0-3 (operator restart MCP)

P1-1 (Skills System) ──[release-order]── ships after P0 stable

P1-2 (Bopomofo IME) ──[operator-gate]── cross-backend confirmation

P2-A/B/C/D ──[independent]── async with above
```

---

## §5 Dispatch shape

### Recommended Wave structure (capacity ~10 PRs)

**Wave 1 — P0 infrastructure (3-4 PRs、~5-7hr ETA)**
- W1 PR-1：P0-1 + P0-4 combined (`bind_self` rebase mode + PR conflict mechanism)
- W1 PR-2：P0-2 (daemon hot-reload tool registry)
- W1 PR-3：P0-3 (operator restart MCP tool)
- W1 PR-4 (optional)：P1-2 Bopomofo IME if operator confirms cross-backend pre-Wave-1

**Wave 2 — P1 backlog + select P2 (2-4 PRs、~6-9hr ETA)**
- W2 PR-1：P1-1 Skills System Plan IMPL (largest single PR)
- W2 PR-2 (optional)：P2-D LOC estimation methodology + ceiling enforcement
- W2 PR-3 (optional)：P2-E parallel filler safety re-evaluation (post P0-1 verifies)
- W2 PR-4 (optional)：1-2 P2-A Sprint 58 deferred items

**Wave 3 — Sprint closeout + carryover (1-2 PRs)**
- W3 PR-1 (optional)：P2-B protocol-audit bundled doc/RCA PR
- W3 PR-2 (optional)：P2-C teloxide upgrade evaluation Phase 1

### Sequential vs parallel

Sequential default per Q2=(C) lease-thrash protection (Sprint 59 active rule)。Re-evaluate parallel safety after P0-1 ships per P2-E。

### File-overlap audit pre-dispatch

For each Wave 1 PR、verify in-flight PR file touches do not overlap (memory `feedback_filler_pr_file_overlap_audit.md`)。Particular concern：
- `src/daemon/supervisor.rs` (P0-2 + P0-3 may touch)
- `src/mcp/tools.rs` (P0-3 adds new tool entry、P1-1 may add skills MCP)
- `src/cli.rs` (P0-3 status surface + P1-1 skills subcommand both touch — sequential dispatch)

---

## §6 Operator gates

### Pre-IMPL operator gate
- **P1-2 Bopomofo IME**：cross-backend reproduction confirmation required
- **P2-A Sprint 58 deferred selection**：if Sprint 60 capacity allows >2 deferred items、operator review pick

### Post-IMPL operator confirmation
- **P0-1 bind_self rebase mode**：operator post-merge smoke (verifies stale-state recovery in real edge case)
- **P1-1 Skills System**：operator post-merge smoke (verifies 5-backend skill discovery + symlink + Windows fallback)

### Auto-proceed without operator
- All other P0 items (general self-decide per current authority)
- All other P2 items (general self-decide)

---

## §7 Total estimate aggregate

### Capacity envelope: ~10 PRs / ~20-30hr ETA total

| Bucket | PRs (typical) | LOC range | ETA range |
|---|---|---|---|
| P0 infrastructure | 3-4 | 540-1110 LOC + tests | 5-7hr |
| P1 backlog | 1-2 | 715-1125 LOC + tests | 4-6hr |
| P2 select | 0-3 | 0-450 LOC + tests | 0-4hr |
| **Total** | **5-9 PRs** | **1255-2685 LOC** | **9-17hr core** |

### Strict-capacity scenario (4 PRs only)
- W1: P0-1+P0-4 / P0-2 / P0-3 (3 PRs)
- W2 OR W1: P1-2 Bopomofo IME (1 PR、operator-gated)
- Skills System P1-1 → punt Sprint 61 W1
- All P2 → Sprint 61

### Aspirational scenario (8-9 PRs)
- W1: P0-1+P0-4 / P0-2 / P0-3 (3 PRs)
- W2: P1-1 Skills System (1 large PR)
- W3: P1-2 Bopomofo IME / P2-D LOC estimation / P2-E parallel filler / 1 P2-A deferred (4 PRs)

---

## §8 Scope appetite recommendation

**Lead recommendation: balanced scenario** (5-7 PRs、Wave 1 P0 + Wave 2 P1-1 + Wave 3 selective P2)

Rationale:
1. **P0 must-ship**: 4 P0 items address active operational pain (BYPASS incident root cause + chicken-and-egg + SPOF coverage)
2. **P1-1 Skills System fits**：codex-verify APPROVED + operator P1 confirmed + capacity allows W2 placement
3. **P1-2 Bopomofo IME**: tiny scope、operator-gate is the only blocker、ship-in-any-wave on confirmation
4. **P2 selective**: 1-2 items ship if Wave 3 capacity remains、else carryover
5. **No infrastructure-only Sprint risk**: Skills System + Bopomofo IME provide user-visible value alongside infrastructure

**Operator scope-appetite question**：do you prefer (i) infrastructure-heavy (P0 + P2) deferring P1-1 to Sprint 61、OR (ii) balanced (P0 + P1) per lead recommendation、OR (iii) feature-heavy (P0-1 only + P1 + P2-D/E)?

Default: balanced scenario per general auto-proceed mode if no operator override within 24hr post-PLAN-merge。

---

## §9 Risks + mitigations

### Risk 1: P0-2 daemon hot-reload complexity higher than estimated
- **Mitigation**: surface-block to lead at scope-creep > 400 LOC、cherry-pick to follow-up PR if needed

### Risk 2: P1-1 Skills System Windows-fallback edge cases
- **Mitigation**: extensive test coverage planned per general analysis、CI runs Windows job already

### Risk 3: P0 work introduces new chicken-and-egg cycles
- **Mitigation**: operator restart MCP tool (P0-3) provides escape hatch + force_release_worktree (Sprint 59 #571) covers stale-lease cases

### Risk 4: Bopomofo IME cross-backend confirmation takes longer than Sprint 60 window
- **Mitigation**: punt to Sprint 61 W1 if no operator confirmation by Sprint 60 W2 closeout

### Risk 5: LOC overage repeats from Wave 2 (PR-IMPL 3-5x、PR-3 1.65x)
- **Mitigation**: P2-D LOC estimation methodology + ceiling enforcement protocol ships if Sprint 60 capacity permits

---

## §10 IMPL gate / next steps

**Auto-trigger**: Sprint 60 PLAN VERIFIED → general dispatches Wave 1 PR-1 (P0-1 + P0-4 combined) per auto-proceed mode

**Operator pre-Wave-1 review** (optional): operator may flag scope-appetite preference + Bopomofo IME cross-backend status

**Default next step**: post-PLAN-merge、lead dispatches Wave 1 PR-1 sequential per Q2=(C) protocol

---

## §11 Cross-references

- **Sprint 59 closeout synth**: lead m-20260509185020...
- **Operator Q1+Q2 decisions**: feedback memory `feedback_q2_bypass_free_rebase_protocol.md`
- **Filler-overlap audit rule**: feedback memory `feedback_filler_pr_file_overlap_audit.md`
- **Reviewer stale-content diagnostic**: feedback memory `feedback_reviewer_stale_content_diagnostic.md`
- **STRICT triple-banner v2**: feedback memory `feedback_strict_triple_banner_v2.md`
- **AGEND_GIT_BYPASS prohibition**: feedback memory `feedback_no_agend_git_bypass.md`
- **Skills System Plan reference**: general memory `project_skills_system_plan_reference.md`
- **Sprint 60 candidate pool snapshot**: project memory `project_sprint60_candidate_pool.md`

---

## §12 Closeout — review request

Lead requests general + (optional) operator review of:
- **§3 P-ranking**：is P0/P1/P2 ordering aligned with operational priority?
- **§5 dispatch shape**：is Wave 1/2/3 structure realistic for ~10-PR capacity?
- **§7 capacity envelope**：is balanced scenario (5-7 PRs) right OR should we go strict-capacity / aspirational?
- **§8 scope appetite**：operator pre-IMPL preference question
- **§9 risk register**：any missed risks?

Verdict expectations：VERIFIED + scope appetite ruling → IMPL dispatch begins。REJECTED + adjustment guidance → r1 PLAN revision。
