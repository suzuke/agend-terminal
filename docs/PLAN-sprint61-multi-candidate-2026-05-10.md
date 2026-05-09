# Sprint 61 PLAN — Multi-Candidate Inventory + Dispatch Shape

**Date**: 2026-05-10
**Author**: lead
**Status**: PLAN draft v0 (awaiting general review + operator scope-appetite ruling)
**Source-of-truth**: `origin/main` HEAD `17011c0` (Sprint 60 W3 PR-2 #583 just merged、Sprint 60 closeout achieved)
**Auto-trigger**: per operator-approved (A) auto-dispatch sprint PLAN on prior closeout (general dispatch m-20260509231925101722-427)

---

## §0 Context

**Sprint 60 closeout** (2026-05-10) shipped 6 PRs (W1: 3 + W2: 1 + W3: 2):
- W1 P0 infrastructure (4 P0 items closed via 3 PRs、SPOF coverage trifecta complete)
- W2 P1 Skills System Plan IMPL landed (5-backend community skills)
- W3 P2 selective: LOC estimation methodology + parallel filler safety re-evaluation
- Engineering anti-stall arc 達成 — 10 PRs / 5 layers across Sprint 58→60

**Sprint 60 highlights**：
- 5/6 PRs first-attempt VERIFIED + 1 verdict-delivery-miss recovery + 1 r1 fixup
- ~2100 LOC total / ETA ~30-45% faster than 9-17hr core estimate
- 0 BYPASS across all 6 PRs (Q2=(C) bypass-free protocol maintained)
- Cumulative Sprint 56→60: 49 PRs merged (47 ledger-clean post-policy + 1 ⚠ scoped-bypass)

**Sprint 61 candidate pool** — 17+ items accumulated through Sprint 60 deferrals + Sprint 58/59 carried-over + Sprint 60 W3 surfaced patterns。

**Capacity baseline**：~6-10 PRs per Sprint per Sprint 58/59/60 actual (Sprint 60 was 6 PRs)。Sprint 61 should aim for similar throughput。

**Sprint 61 theme** (proposed): **Sprint 60 deferral wiring + protocol-audit cleanup + cross-cycle finishes**。Most P0 candidates are pure-wiring follow-ups to Sprint 60 deferrals + Bopomofo IME if operator confirms。

---

## §1 Goal

Sprint 61 ships the wiring follow-ups + cross-cycle items deferred from Sprint 60:
1. Skills System auto-install at agent launch (P0 #1、daemon-launch wiring follow-up to #581)
2. Skills System fleet.yaml per-instance override (P0 #2、schema follow-up to #581)
3. LOC estimation Component 2 helper script + GHA + label automation (P0 #3、follow-up to #582)
4. Parallel filler Option B formal schema (P0 #4、formalization of #583 MVP semantic)
5. Bopomofo IME Shape A (P1 #5、operator-gate pending、cross-backend confirmation)
6. Optional protocol-audit cleanup + Sprint 58 deferred selection (P2 selective)

**Non-goals**:
- teloxide 0.13.0 upgrade (P2、Sprint 62+ candidate pending API surface investigation)
- Multi-agent coordination test harness (P2、Sprint 62+ candidate)
- Verdict-delivery confirmation protocol (P2、observation-period extension)

---

## §2 Verified state (origin/main 17011c0)

### Sprint 60 landings still active
- `src/mcp/handlers/bind.rs` — `bind_self` rebase_mode (#578)
- `src/mcp/handlers/force_release.rs` — shared `rebase_clean_self` helper (#578 + #571)
- `src/daemon/mcp_registry_watcher.rs` — MCPRegistryWatcher 5th tracker (#579)
- `src/mcp/handlers/restart.rs` — restart_daemon MCP tool (#580)
- `src/skills.rs` — Skills System core (#581)
- `docs/PROCESS-LOC-ESTIMATION-METHODOLOGY.md` — 5-category framework + ceiling protocol (#582)
- `docs/RCA-PARALLEL-FILLER-SAFETY-REVALUATION.md` — Option B recommendation (#583)

### 5-tracker supervisor coexistence active
AntiStallTracker + IdleWatchdogTracker + DecisionTimeoutTracker + HelperStalenessWatchdog + MCPRegistryWatcher — all init + scan independently every TICK。

### Tool count: 32 (post #580 restart_daemon addition)

**Sprint 61 base**: HEAD 17011c0、CI green、no in-flight PRs、all worktrees released。

---

## §3 Inventory — P-ranking

### P0 — Sprint 60 deferral wiring (4 candidates、ship-priority)

**Total P0 estimate**：~190-380 LOC + tests = roughly 25-40% of Sprint 61 capacity envelope

#### P0-1. Skills System auto-install at agent launch

- **Source**: Sprint 60 W2 PR-1 #581 deferral (per general (A) approval m-20260509215944336230-389)
- **Goal**: daemon agent-launch flow auto-invokes `install_for_agent(home, working_dir)` on configured agents、eliminating manual `agend skills install` requirement
- **Fix shape**: Pure-wiring (no API redesign)、daemon launch → call install_for_agent
- **LOC est**: ~50-100 prod + ~30-50 test = ~80-150 total
- **Tier**: Tier-1 single primary
- **Path**: Path A IMPL with smoke (verifies new agent launch installs skills)
- **Files**: `src/daemon/lifecycle.rs` (~30-50 LOC、call install_for_agent post-bind) + tests
- **Dependencies**: none hard、PR-1 from #581 already shipped install_for_agent API
- **Operator gate**: NO — general self-decide
- **ETA**: ~1-1.5hr

#### P0-2. Skills System fleet.yaml per-instance override

- **Source**: Sprint 60 W2 PR-1 #581 deferral
- **Goal**: fleet.yaml schema field `instance.<name>.skills: [skill-1, skill-2]` for per-instance scope control
- **Fix shape**: Pure-wiring (no API redesign)、fleet.yaml schema field + dispatch to install_for_agent on configured agents
- **LOC est**: ~30-80 prod + ~20-50 test = ~50-130 total
- **Tier**: Tier-1 single primary
- **Path**: Path A IMPL with smoke
- **Files**: `src/fleet.rs` (~10-20 LOC schema field) + `src/daemon/lifecycle.rs` (~10-30 LOC dispatch logic) + tests
- **Dependencies**: synergy with P0-1 (both touch lifecycle.rs、sequential dispatch mandatory)
- **Operator gate**: NO — general self-decide
- **ETA**: ~1-1.5hr

#### P0-3. LOC estimation Component 2 helper script + GHA + label automation

- **Source**: Sprint 60 W3 PR-1 #582 deferral
- **Goal**: Automate LOC overrun detection + cohesion-accept override workflow
- **Fix shape**:
  - `scripts/check_loc_overrun.sh` (~30-50 LOC) parses `<!-- LOC-EST: X-Y -->` from PR description vs `gh pr diff` actual
  - GitHub Actions workflow integration (`.github/workflows/loc-overrun-check.yml` ~30-50 LOC)
  - PR label automation (`loc-overrun-accepted` label triggers cohesion-accept override)
- **LOC est**: ~60-100 prod + ~30-50 test = ~90-150 total
- **Tier**: Tier-1 single primary
- **Path**: Path A IMPL with smoke (verifies CI fires on overrun + label override works)
- **Files**: `scripts/check_loc_overrun.sh` (NEW) + `.github/workflows/loc-overrun-check.yml` (NEW) + tests
- **Dependencies**: none hard、#582 methodology doc already shipped
- **Operator gate**: NO — general self-decide
- **ETA**: ~1.5-2hr

#### P0-4. Parallel filler Option B formal schema

- **Source**: Sprint 60 W3 PR-2 #583 §5 deferral
- **Goal**: Formal dispatch schema for parallel-filler opt-in (replaces MVP "parallel-feasible vs PR-X" dispatch-text semantic)
- **Fix shape**:
  - Update memory `feedback_filler_pr_file_overlap_audit.md` with formal schema
  - Optional: dispatch tool schema field `parallel_with: <pr_id>` if MCP dispatch evolves
  - Smoke test verifying parallel filler scenario with bind_self rebase mode safety net
- **LOC est**: ~30-50 doc + ~20-50 test = ~50-100 total
- **Tier**: Tier-1 single primary
- **Path**: Path B doc + Path A optional smoke test
- **Files**: memory update + optional tests
- **Dependencies**: requires P0-1 to P0-3 stable (gives confidence in lifecycle.rs touches not introducing race)
- **Operator gate**: NO — general self-decide
- **ETA**: ~30min-1hr

**P0 dispatch shape recommendation**: Wave 1 sequential (P0-1 → P0-2 same lifecycle.rs file overlap) → P0-3 (independent surface) → P0-4 (smoke + doc)。Total Wave 1 ETA ~4-6hr。

### P1 — Operator-gate cross-cycle (1 candidate)

#### P1-1. Bopomofo IME Shape A — ⚠ operator-gate pending

- **Source**: Sprint 59 W2 PR-2 RCA #575 + operator-reported regression issue #532
- **Symptom**: Claude Code agent pane cursor not focused on command line during Bopomofo IME composition
- **Fix shape**: Shape A — drop scroll-offset half of cursor-emit gate at `src/render/core_render.rs:400` (~5 LOC change、unwinds old over-restriction)
- **LOC est**: ~5 prod + ~10-20 test = ~15-25 total
- **Tier**: Tier-1 single primary
- **Path**: Path A IMPL with manual smoke (operator runs IME composition test post-merge)
- **Files**: `src/render/core_render.rs` (-1 line + minor adjustment at line 400) + tests
- **Dependencies**: ⚠ **OPERATOR-GATE PENDING**：cross-backend reproduction confirmation (Claude Code / Codex / Kiro / Gemini) — same as Sprint 60 W2 punt
  - If only Claude Code reproduces → backend-specific contamination → operator triage required
  - If all four reproduce → Shape A IMPL ships under general self-decide
- **Operator gate**: YES — cross-backend reproduction confirmation required pre-IMPL dispatch
- **ETA**: ~30min once operator confirms

**P1 dispatch shape**: ASAP post operator confirmation、tiny scope。Could fit any wave or as cherry-pick filler。

### P2 — Deferred + Carried-over (12+ candidates)

#### P2-A. Sprint 58 P2 deferred items (8 from Sprint 58 closeout deferred-items roster)

[Reference roster from Sprint 58 closeout synth — specific items need restoration during PLAN dispatch construction]
- **A1-A8**: per Sprint 58 P2 deferred catalog

**Suggested handling**: cherry-pick 1-2 items per Sprint 61 W3 capacity if remaining。Else carryover to Sprint 62。

#### P2-B. Sprint 58 protocol-audit candidates remaining (4 post-#566)

[Reference roster from Sprint 58 closeout synth — specific items need restoration during PLAN dispatch construction]
- **B1-B4**: per Sprint 58 protocol-audit catalog

**Suggested handling**: bundle into single doc/RCA PR if scope per-item is small。Or cherry-pick 1-2 if value clear。

#### P2-C. teloxide 0.13.0 upgrade evaluation (Wave 2 PR-IMPL F2 deferral)

- **Source**: Sprint 59 Wave 2 PR-IMPL #574 F2 deferral
- **Goal**: evaluate teloxide 0.13.0 (or successor) for chat-side enumeration support; if available、unlock 5-class (γ) taxonomy + post-hoc duplicate detection
- **LOC est**: Phase 1 ~100-200 LOC dep upgrade + Phase 2 ~150-250 LOC IMPL conditional
- **Tier**: Tier-1 single primary
- **Path**: Path B RCA-first → Path A IMPL conditional
- **Operator gate**: NO — general self-decide

**Suggested handling**: Sprint 61 W3 OR Sprint 62 candidate depending on capacity。

#### P2-D. Multi-agent coordination test harness (Sprint 60 W3 PR-2 #583 §5 deferral)

- **Source**: Sprint 60 W3 PR-2 #583 §5 (referenced as Sprint 61+ candidate)
- **Goal**: Test harness simulating multi-agent dispatch scenarios for parallel-filler safety verification
- **LOC est**: ~150-300 LOC test infrastructure + tests
- **Tier**: Tier-1 single primary
- **Path**: Path A test infrastructure
- **Operator gate**: NO — general self-decide

**Suggested handling**: defer Sprint 62+ — current single-agent #578 unit + e2e tests sufficient for Sprint 61 P0 wiring。

#### P2-E. Verdict-delivery confirmation protocol (Sprint 60 W1 PR-1 #578 incident pattern)

- **Source**: Sprint 60 W1 PR-1 #578 verdict-delivery-miss observation
- **Goal**: If pattern recurs (≥2 incidents)、formalize confirmation protocol + tooling
- **LOC est**: TBD pending pattern recurrence
- **Tier**: TBD
- **Path**: Path B observation memory first

**Suggested handling**: keep as observation candidate、dispatch IF pattern recurs in Sprint 61。Save feedback memory if recurs。

**P2 dispatch shape recommendation**: cherry-pick 1-2 items from P2 list based on Sprint 61 capacity envelope。Aim for 0-2 P2 items in Sprint 61、defer rest to Sprint 62。

---

## §4 Dependencies graph

```
P0-1 (Skills auto-install) ──[lifecycle.rs file-overlap]── P0-2 (Skills fleet.yaml override)
                              [SEQUENTIAL MANDATORY]

P0-3 (LOC est helper) ──[independent]── async with P0-1/P0-2

P0-4 (Parallel filler formal) ──[stability gate]── post P0-1+P0-2+P0-3

P1-1 (Bopomofo IME) ──[operator-gate]── cross-backend confirmation

P2-A/B/C/D/E ──[independent]── Sprint 62 candidates if Sprint 61 capacity full
```

---

## §5 Dispatch shape

### Recommended Wave structure (capacity ~6-10 PRs)

**Wave 1 — P0 deferral wiring (3-4 PRs、~4-6hr ETA)**
- W1 PR-1：P0-1 Skills auto-install at agent launch (~80-150 LOC)
- W1 PR-2：P0-2 Skills fleet.yaml per-instance override (~50-130 LOC、sequential post PR-1 lifecycle.rs file-overlap)
- W1 PR-3：P0-3 LOC estimation Component 2 helper + GHA + label (~90-150 LOC、independent surface、parallel-feasible per #583 Option B if disjoint files but sequential default)
- W1 PR-4 (optional)：P1-1 Bopomofo IME if operator confirms cross-backend pre-Wave-1 (~15-25 LOC、tiny scope)

**Wave 2 — P0 closeout + select P2 (1-3 PRs、~1-3hr ETA)**
- W2 PR-1：P0-4 parallel filler formal schema (~50-100 LOC、stability gate post W1)
- W2 PR-2 (optional)：1 P2-A Sprint 58 deferred OR P2-B protocol-audit bundled
- W2 PR-3 (optional)：P2-C teloxide upgrade evaluation Phase 1 (Path B RCA)

**Wave 3 — Sprint 61 closeout + carryover (0-2 PRs)**
- W3 PR-1 (optional)：P1-1 Bopomofo IME if operator confirms mid-Sprint-61
- W3 PR-2 (optional)：1-2 cherry-picked P2 items

### Sequential vs parallel

Sequential default per #583 Option B recommendation。Parallel opt-in via dispatch text "parallel-feasible vs PR-X" semantic for explicit disjoint-surface cases。P0-1 → P0-2 sequential mandatory (lifecycle.rs file overlap)。P0-3 vs P0-1/P0-2 could be parallel-feasible (different files) but sequential default applies until P0-4 formalizes opt-in。

### File-overlap audit pre-dispatch

For each Wave 1 PR、verify in-flight PR file touches do not overlap:
- P0-1 + P0-2 both touch `src/daemon/lifecycle.rs` — sequential mandatory
- P0-3 touches `scripts/` + `.github/workflows/` — independent
- P0-4 touches docs only — independent

---

## §6 Operator gates

### Pre-IMPL operator gate
- **P1-1 Bopomofo IME**：cross-backend reproduction confirmation required (Claude Code / Codex / Kiro / Gemini)
- **P2-A Sprint 58 deferred selection**：if Sprint 61 capacity allows >2 deferred items、operator review pick

### Post-IMPL operator confirmation
- **P0-1 Skills auto-install**：operator post-merge smoke (verifies new agent launches install skills correctly)
- **P0-2 Skills fleet.yaml override**：operator post-merge smoke (verifies per-instance scope works)
- **P1-1 Bopomofo IME**：operator post-merge IME composition test

### Auto-proceed without operator
- All other P0 items (general self-decide per current authority)
- All other P2 items (general self-decide)

---

## §7 Total estimate aggregate

### Capacity envelope: ~6-10 PRs / ~6-12hr ETA total

| Bucket | PRs (typical) | LOC range | ETA range |
|---|---|---|---|
| P0 deferral wiring | 3-4 | 270-530 LOC + tests | 4-6hr |
| P1 cross-cycle | 0-1 | 0-25 LOC | 0-30min |
| P2 select | 0-3 | 0-450 LOC + tests | 0-4hr |
| **Total** | **3-8 PRs** | **270-1005 LOC** | **4-10hr core** |

### Strict-capacity scenario (3 PRs only)
- W1: P0-1 / P0-2 / P0-3 (3 PRs P0 wiring complete)
- W2/W3: skip
- All P1/P2 → Sprint 62

### Aspirational scenario (7-8 PRs)
- W1: P0-1 / P0-2 / P0-3 / P1-1 (4 PRs)
- W2: P0-4 / P2-A / P2-B-bundled (3 PRs)
- W3: P2-C-Phase-1 RCA (1 PR)

---

## §8 Scope appetite recommendation

**Lead recommendation: balanced scenario** (5-6 PRs、Wave 1 P0-1+P0-2+P0-3 + Wave 2 P0-4 + Wave 3 selective P2)

Rationale:
1. **P0 must-ship**: 4 deferral wiring items address Sprint 60 deferred work、bounded scope each
2. **P1-1 Bopomofo IME**: ship if operator confirms cross-backend、tiny scope opportunistic
3. **P2 selective**: 1-2 items if Wave 3 capacity remains、else carryover
4. **No infrastructure-only Sprint risk**: P1-1 + selective P2 provide variety alongside infrastructure wiring
5. **Sprint 60 baseline preserved**: Sprint 60 was 6 PRs balanced scenario、Sprint 61 follows same envelope

**Operator scope-appetite question**：do you prefer (i) infrastructure-heavy (P0 only + P2-C teloxide eval、defer P1-1 to Sprint 62)、OR (ii) balanced (P0 + P1-1 + 1-2 P2)、OR (iii) feature-heavy (P0-3 only + P1-1 + P2-A/B/C selective)?

Default: balanced scenario per general auto-proceed mode if no operator override within 24hr post-PLAN-merge。

---

## §9 Risks + mitigations

### Risk 1: P0-1 + P0-2 lifecycle.rs touches introduce launcher regression
- **Mitigation**: smoke tests verify new agent launches work correctly post-each-PR、existing fleet tests preserve regression coverage

### Risk 2: P0-3 GHA workflow false-positives on legitimate cohesion-accept cases
- **Mitigation**: PR label `loc-overrun-accepted` documented + tested as override path、threshold tunable via env vars

### Risk 3: Bopomofo IME cross-backend confirmation takes longer than Sprint 61 window
- **Mitigation**: punt to Sprint 62 W1 if no operator confirmation by Sprint 61 W2 closeout (already Sprint 60 precedent)

### Risk 4: P0-4 parallel filler formal schema introduces churn vs MVP semantic
- **Mitigation**: schema is additive (preserves "parallel-feasible vs PR-X" dispatch-text MVP)、explicit opt-in via formal field

### Risk 5: P0-3 helper script CI integration fails on existing PR backlog without `<!-- LOC-EST: X-Y -->` markers
- **Mitigation**: helper script gracefully skips PRs without marker、only triggers on PRs that opt-in

---

## §10 IMPL gate / next steps

**Auto-trigger**: Sprint 61 PLAN VERIFIED → general dispatches Wave 1 PR-1 (P0-1 Skills auto-install) per auto-proceed mode

**Operator pre-Wave-1 review** (optional): operator may flag scope-appetite preference + Bopomofo IME cross-backend status

**Default next step**: post-PLAN-merge、lead dispatches Wave 1 PR-1 sequential per Q2=(C) protocol + #583 Option B dispatch sequencing

---

## §11 Cross-references

- **Sprint 60 closeout synth**: lead m-20260509231900...
- **Sprint 60 PLAN reference**: `docs/PLAN-sprint60-multi-candidate-2026-05-10.md` (PR #577 merged 9124a95)
- **Sprint 60 deferral PRs**: #581 (Skills System) / #582 (LOC estimation methodology) / #583 (parallel filler safety RCA)
- **Bopomofo IME RCA**: `docs/RCA-BOPOMOFO-IME-CURSOR-REGRESSION.md` (Sprint 59 W2 PR-2 #575)
- **Operator Q1+Q2 decisions**: feedback memory `feedback_q2_bypass_free_rebase_protocol.md`
- **Filler-overlap audit rule**: feedback memory `feedback_filler_pr_file_overlap_audit.md` (updated by Sprint 60 #583)
- **Reviewer stale-content diagnostic**: feedback memory `feedback_reviewer_stale_content_diagnostic.md`
- **STRICT triple-banner v2**: feedback memory `feedback_strict_triple_banner_v2.md`
- **AGEND_GIT_BYPASS prohibition**: feedback memory `feedback_no_agend_git_bypass.md`
- **Sprint 60 candidate pool snapshot**: project memory `project_sprint60_candidate_pool.md`
- **Skills System Plan reference**: general memory `project_skills_system_plan_reference.md` (verbatim)
- **LOC estimation methodology**: `docs/PROCESS-LOC-ESTIMATION-METHODOLOGY.md` (Sprint 60 #582)

---

## §12 Closeout — review request

Lead requests general + (optional) operator review of:
- **§3 P-ranking**：is P0/P1/P2 ordering aligned with operational priority?
- **§5 dispatch shape**：is Wave 1/2/3 structure realistic for ~6-10-PR capacity?
- **§7 capacity envelope**：is balanced scenario (5-6 PRs) right OR should we go strict-capacity / aspirational?
- **§8 scope appetite**：operator pre-IMPL preference question
- **§9 risk register**：any missed risks?

Verdict expectations：VERIFIED + scope appetite ruling → IMPL dispatch begins。REJECTED + adjustment guidance → r1 PLAN revision。
