# Sprint 62 PLAN — Multi-Candidate Inventory + Dispatch Shape

**Date**: 2026-05-10
**Author**: lead
**Status**: PLAN draft v0 (awaiting general review + operator scope-appetite ruling)
**Source-of-truth**: `origin/main` HEAD `d216375` (Sprint 61 W2 PR-1 #588 just merged、Sprint 61 closeout achieved)
**Auto-trigger**: per operator-approved (A) auto-dispatch sprint PLAN on prior closeout (general dispatch m-20260510005340465676-481)

<!-- LOC-EST: 350-450 -->

---

## §0 Context

**Sprint 61 closeout** (2026-05-10) shipped 4 PRs (W1: 3 + W2: 1):
- All Sprint 60 deferrals closed (4-of-4: #581 deferral 1+2 + #582 Component 2 + #583 §5)
- Skills System feature complete (auto-install + fleet.yaml override)
- LOC overrun automation deployed (Component 2 helper + GHA + label override)
- Parallel filler opt-in formal schema documented

**Sprint 61 highlights**：
- 4/4 PRs first-attempt VERIFIED (cleanest Sprint outcome since Sprint 56)
- ~626 LOC total Sprint 61 / ETA ~70%+ faster than 4-7hr core estimate
- 0 BYPASS across all 4 PRs (Q2=(C) bypass-free protocol maintained)
- 3 self-applying meta-recursive validation cases established (#582 + #583 + #587)
- 100% LOC-EST marker compliance post-#582 (5/5 PRs)
- Cumulative Sprint 56→61: 54 PRs merged (52 ledger-clean post-policy + 1 ⚠ scoped-bypass)

**Sprint 62 candidate pool** — 17+ items accumulated through Sprint 60-61 surfaced + Sprint 58 carried-over + Sprint 59 W2 deferrals。

**Capacity baseline**：~5-7 PRs per Sprint per Sprint 60 (6 PRs) + Sprint 61 (4 PRs) recent baseline。

**Sprint 62 theme** (proposed): **P2 cleanup + Sprint 58 carryover + cross-cycle deferral closures**。Most candidates are small-scope infrastructure cleanup or doc work. Sprint 60-61 closed P0 infrastructure + P1 Skills System + Sprint 60 deferrals — Sprint 62 is the cleanup-sprint pattern。

---

## §1 Goal

Sprint 62 ships the cleanup + carryover items deferred from Sprint 58/59/60/61:
1. Sprint 60-61 surfaced cleanups (P0/P2 small): FNV non-crypto digest + skills-stage GC + verdict-delivery confirmation protocol
2. teloxide upgrade evaluation (P0/P1: Phase 1 RCA、conditional Phase 2 IMPL)
3. Bopomofo IME Shape A (P1、3rd Sprint deferral if operator no-response)
4. Sprint 58 P2 carryover (selective、12 items remaining)
5. Multi-agent coordination test harness (P2、infrastructure)

**Non-goals**:
- Major new feature work (no Skills System-level surface)
- Sprint 60-61 mature pattern revisits (LOC-EST automation + cohesion-accept proceed as-is)

---

## §2 Verified state (origin/main d216375)

### Sprint 60-61 landings active
- Skills System: src/skills.rs + src/cli.rs + src/main.rs Doctor variant + auto-install hook + fleet.yaml override
- LOC overrun automation: scripts/check_loc_overrun.sh + .github/workflows/loc-overrun-check.yml + label override
- Parallel filler protocol: docs/PROTOCOL-PARALLEL-FILLER-OPT-IN-SCHEMA.md
- LOC estimation methodology: docs/PROCESS-LOC-ESTIMATION-METHODOLOGY.md
- Engineering anti-stall arc: 10 PRs / 5 layers (Sprint 58→60)
- 5-tracker supervisor coexistence: AntiStall + IdleWatchdog + DecisionTimeout + HelperStaleness + MCPRegistryWatcher
- bind_self rebase mode + restart_daemon + force_release_worktree (SPOF coverage trifecta)

### Tool count: 32 (post Sprint 60 #580 restart_daemon)

**Sprint 62 base**: HEAD d216375、CI green、all worktrees released、no in-flight PRs。

---

## §3 Inventory — P-ranking

### P0 — Cleanup of Sprint 60-61 mature observations (3 candidates、ship-priority)

#### P0-1. FNV non-crypto digest replacement (Sprint 61 #586 caveat)

- **Source**: Sprint 61 #586 reviewer minor caveat: FNV-1a digest in `stage_filtered_source` is non-cryptographic
- **Goal**: Replace FNV-1a with a stronger digest (BLAKE3 / SHA-256 / similar) for `<home>/.skills-stage/<digest>/` directory naming
- **Fix shape**: Switch digest implementation in skills.rs `stage_filtered_source` helper
- **LOC est**: ~10-30 prod + ~10-20 test = ~20-50 total
- **Tier**: Tier-1 single primary
- **Path**: Path A IMPL with smoke
- **Files**: `src/skills.rs` (~10-20 LOC digest replacement)
- **Dependencies**: none、Sprint 61 #586 already shipped FNV stub
- **Operator gate**: NO
- **ETA**: ~30min

#### P0-2. skills-stage `<digest>/` GC cleanup (Sprint 61 #586 caveat)

- **Source**: Sprint 61 #586 reviewer minor caveat: stage dirs rebuilt per call + retained without GC
- **Goal**: Garbage-collect stale stage directories (older than retention threshold)
- **Fix shape**: Add `cleanup_stale_stages` helper invoked on daemon start OR periodic via supervisor
- **LOC est**: ~30-50 prod + ~20-40 test = ~50-90 total
- **Tier**: Tier-1 single primary
- **Path**: Path A IMPL with smoke
- **Files**: `src/skills.rs` (cleanup helper) + `src/daemon/mod.rs` (invocation site)
- **Dependencies**: synergy with P0-1 (both touch skills.rs)、sequential dispatch recommended
- **Operator gate**: NO
- **ETA**: ~1hr

#### P0-3. Verdict-delivery confirmation protocol (Sprint 60 W1 PR-1 #578 incident pattern)

- **Source**: Sprint 60 W1 PR-1 #578 verdict-delivery-miss incident (resolved via lead-status-query at ~60min stall threshold) + Sprint 61 W2 PR-1 task_id mismatch incident (similar dispatch-tracking issue)
- **Goal**: If pattern recurs OR formal protocol desired、document confirmation procedure + optional tooling
- **Fix shape**:
  - Update memory `feedback_reviewer_stale_content_diagnostic.md` OR new memory with confirmation pattern
  - Optional: standardized `[verdict-delivered] <verdict> at <head>` ack message convention from reviewer
- **LOC est**: ~30-60 doc + ~10-30 optional test = ~40-90 total
- **Tier**: Tier-1 single primary
- **Path**: Path B doc + Path A optional smoke
- **Files**: memory + optional reviewer-side ack convention
- **Dependencies**: none
- **Operator gate**: NO
- **ETA**: ~30min-1hr

**P0 dispatch shape recommendation**: Wave 1 sequential (P0-1 → P0-2 same skills.rs file overlap) → P0-3 (independent surface)。Total Wave 1 ETA ~2-3hr。

### P1 — Operator-gate cross-cycle (1 candidate、3rd Sprint deferral if continues)

#### P1-1. Bopomofo IME Shape A — ⚠ operator-gate pending (Sprint 60 → 61 → 62 deferral)

- **Source**: Sprint 59 W2 PR-2 RCA #575 + operator-reported regression issue #532
- **Symptom**: Claude Code agent pane cursor not focused on command line during Bopomofo IME composition
- **Fix shape**: Shape A — drop scroll-offset half of cursor-emit gate at `src/render/core_render.rs:400` (~5 LOC)
- **LOC est**: ~5 prod + ~10-20 test = ~15-25 total
- **Tier**: Tier-1 single primary
- **Path**: Path A IMPL with manual smoke
- **Files**: `src/render/core_render.rs`
- **Dependencies**: ⚠ **OPERATOR-GATE PENDING (3rd Sprint)**：cross-backend reproduction confirmation
- **Operator gate**: YES — same as Sprint 60 + 61
- **ETA**: ~30min once operator confirms

**P1 dispatch shape**: ASAP if operator confirms。If 3-Sprint deferral pattern continues、surface to operator at Sprint 62 closeout for direction (continue defer / cancel / alternative fix shape)。

### P2 — Carried-over + cross-cycle deferred (13+ candidates)

#### P2-A. Sprint 58 P2 deferred items (8 items)

[Specific roster restoration during dispatch construction needed]
- **A1-A8**: per Sprint 58 closeout deferred-items roster

**Suggested handling**: cherry-pick 1-2 items per Sprint 62 W3 capacity if remaining。Else carryover Sprint 63。

#### P2-B. Sprint 58 protocol-audit candidates remaining (4 items)

[Specific roster restoration during dispatch construction needed]
- **B1-B4**: per Sprint 58 protocol-audit catalog post-#566

**Suggested handling**: bundle into single doc/RCA PR if scope per-item small。Or cherry-pick 1-2 if value clear。

#### P2-C. teloxide 0.13.0 upgrade evaluation (Sprint 59 W2 PR-IMPL F2 deferral)

- **Source**: Sprint 59 Wave 2 PR-IMPL #574 F2 deferral
- **Goal**: Evaluate teloxide 0.13.0+ for chat-side enumeration support; if available、unlock 5-class (γ) taxonomy + post-hoc duplicate detection
- **Fix shape**:
  - Phase 1 RCA: dependency-bump evaluation (~100-200 LOC PR、Path B)
  - Phase 2 IMPL: conditional on enumeration availability (~150-250 LOC、Path A)
- **LOC est**: Phase 1 ~100-200 / Phase 2 ~150-250
- **Tier**: Tier-1 single primary each phase
- **Path**: Path B RCA → Path A IMPL conditional
- **Operator gate**: NO

**Suggested handling**: ship Phase 1 RCA in Sprint 62 W2、conditional Phase 2 in Sprint 62 W3 OR Sprint 63 depending on RCA outcome。

#### P2-D. Multi-agent coordination test harness (Sprint 60 #583 §5 + Sprint 61 #588 §6 deferrals)

- **Source**: Sprint 60 #583 §5 + Sprint 61 #588 §6 (2 deferrals referencing same item)
- **Goal**: Test harness simulating multi-agent dispatch scenarios for parallel-filler safety verification
- **LOC est**: ~150-300 LOC test infrastructure + tests
- **Tier**: Tier-1 single primary
- **Path**: Path A test infrastructure
- **Operator gate**: NO

**Suggested handling**: defer Sprint 63+ — current single-agent #578 unit + e2e tests sufficient for P2 carryover work。

**P2 dispatch shape recommendation**: cherry-pick 1-3 items based on Sprint 62 capacity envelope。Aim for 1-2 P2 items (selective)、defer rest to Sprint 63。

---

## §4 Dependencies graph

```
P0-1 (FNV digest replacement) ──[skills.rs file-overlap]── P0-2 (skills-stage GC)
                                  [SEQUENTIAL RECOMMENDED]

P0-3 (verdict-delivery protocol) ──[independent]── async with P0-1/P0-2

P1-1 (Bopomofo IME) ──[operator-gate]── cross-backend confirmation (3rd Sprint pending)

P2-C teloxide upgrade ──[Phase 1 → Phase 2]── Phase 2 conditional on Phase 1 RCA outcome

P2-A/B/D ──[independent]── cherry-pick by capacity
```

---

## §5 Dispatch shape

### Recommended Wave structure (capacity ~5-7 PRs)

**Wave 1 — P0 cleanup (3 PRs、~2-3hr ETA)**
- W1 PR-1：P0-1 FNV digest replacement (~20-50 LOC)
- W1 PR-2：P0-2 skills-stage GC (~50-90 LOC、sequential post PR-1)
- W1 PR-3：P0-3 verdict-delivery protocol (~40-90 LOC、independent)
- W1 PR-4 (optional)：P1-1 Bopomofo IME if operator confirms cross-backend pre-Wave-1

**Wave 2 — P2 selective + teloxide RCA (1-3 PRs、~1-3hr ETA)**
- W2 PR-1：P2-C teloxide upgrade evaluation Phase 1 RCA (Path B、~100-200 LOC)
- W2 PR-2 (optional)：1-2 P2-A Sprint 58 deferred items
- W2 PR-3 (optional)：P2-B protocol-audit bundled doc/RCA

**Wave 3 — Sprint 62 closeout + carryover (0-2 PRs)**
- W3 PR-1 (optional)：P2-C teloxide Phase 2 IMPL (conditional on Phase 1 RCA verdict + ≤250 LOC)
- W3 PR-2 (optional)：P1-1 Bopomofo IME if operator confirms mid-Sprint

### Sequential vs parallel

Sequential default per Sprint 61 #588 Option B protocol。Parallel opt-in via formal `parallel-filler: opt-in (vs PR-X; file surface: <paths>)` marker for explicit disjoint-surface cases。

P0-1 + P0-2 sequential mandatory (skills.rs file overlap)。P0-3 independent surface allows parallel-feasibility per Option B but sequential default。

---

## §6 Operator gates

### Pre-IMPL operator gate
- **P1-1 Bopomofo IME**：cross-backend reproduction confirmation (3rd Sprint deferral pending)
- **P2-A Sprint 58 deferred selection**：if Sprint 62 capacity allows >2 deferred items、operator review pick

### Post-IMPL operator confirmation
- **P0-1 FNV digest replacement**：operator post-merge smoke (verifies stage directory naming consistency)
- **P0-3 verdict-delivery protocol**：operator post-merge ack of pattern formalization

### Auto-proceed without operator
- All other P0/P2 items (general self-decide per current authority)

---

## §7 Total estimate aggregate

### Capacity envelope: ~5-7 PRs / ~5-10hr ETA total

| Bucket | PRs (typical) | LOC range | ETA range |
|---|---|---|---|
| P0 cleanup | 3 | 110-230 LOC | 2-3hr |
| P1 cross-cycle | 0-1 | 0-25 LOC | 0-30min |
| P2 select | 1-3 | 100-450 LOC | 1-4hr |
| **Total** | **4-7 PRs** | **210-705 LOC** | **3-7hr core** |

### Strict-capacity scenario (3 PRs only)
- W1: P0-1 / P0-2 / P0-3 (3 PRs)
- W2/W3: skip
- All P1/P2 → Sprint 63

### Aspirational scenario (6-7 PRs)
- W1: P0-1 / P0-2 / P0-3 / P1-1 (4 PRs)
- W2: P2-C Phase 1 RCA / 1 P2-A (2 PRs)
- W3: P2-C Phase 2 IMPL OR Sprint 58 protocol-audit bundle (1 PR)

---

## §8 Scope appetite recommendation

**Lead recommendation: balanced scenario** (5-6 PRs、Wave 1 P0 + Wave 2 teloxide RCA + 1 P2 selective)

Rationale:
1. **P0 cleanup must-ship**: 3 small infrastructure cleanups address Sprint 60-61 mature observations
2. **teloxide RCA value**: Phase 1 RCA unlocks Sprint 63+ Phase 2 IMPL pipeline、preserves momentum on long-deferred chat-side enumeration limitation
3. **P2 selective**: 1 item ship if W2 capacity remains、cherry-pick from Sprint 58 P2 list per scope appetite ruling
4. **P1-1 Bopomofo IME**: ship opportunistically if operator confirms (3rd Sprint deferral pattern emerging — surface to operator if continues)

**Operator scope-appetite question**：do you prefer (i) infrastructure-cleanup-only (P0 only、defer all P1/P2 to Sprint 63)、OR (ii) balanced per lead recommendation、OR (iii) feature-heavy (P0 + P1 + P2-C Phase 1+2)?

Default: balanced scenario per general auto-proceed mode if no operator override within 24hr post-PLAN-merge。

---

## §9 Risks + mitigations

### Risk 1: P0-1 FNV→BLAKE3 digest change breaks existing stage directory naming
- **Mitigation**: cleanup_stale_stages (P0-2) GC handles transition、existing FNV-named stages cleaned on next daemon start

### Risk 2: P0-2 GC threshold tuning (too aggressive deletes active stages)
- **Mitigation**: conservative threshold (e.g. 7-day mtime)、smoke test verifies active-stage preservation

### Risk 3: P2-C teloxide upgrade introduces breaking changes elsewhere
- **Mitigation**: Phase 1 RCA evaluates breaking-change surface upfront、Phase 2 IMPL conditional on RCA-acceptable scope

### Risk 4: Bopomofo IME 3rd Sprint deferral signals operator unavailability
- **Mitigation**: Sprint 62 closeout surfaces to operator for direction (continue defer / cancel / alternative shape)、avoid 4th Sprint perpetual-deferral

### Risk 5: P0-3 verdict-delivery protocol requires reviewer-side cooperation that may not standardize cleanly
- **Mitigation**: Path B doc-only initially、tooling enforcement Sprint 63+ if pattern adopted

---

## §10 IMPL gate / next steps

**Auto-trigger**: Sprint 62 PLAN VERIFIED → general dispatches Wave 1 PR-1 (P0-1 FNV digest replacement) per auto-proceed mode

**Operator pre-Wave-1 review** (optional): operator may flag scope-appetite preference + Bopomofo IME cross-backend status

**Default next step**: post-PLAN-merge、lead dispatches Wave 1 PR-1 sequential per Q2=(C) protocol + #588 Option B dispatch sequencing

---

## §11 Cross-references

- **Sprint 61 closeout synth**: lead m-20260510005521...
- **Sprint 60 PLAN reference**: `docs/PLAN-sprint60-multi-candidate-2026-05-10.md` (PR #577 merged 9124a95)
- **Sprint 61 PLAN reference**: `docs/PLAN-sprint61-multi-candidate-2026-05-10.md` (PR #584 merged d7643e3)
- **Sprint 60 deferral closures**: #585/#586/#587/#588 (Sprint 61 W1+W2)
- **Bopomofo IME RCA**: `docs/RCA-BOPOMOFO-IME-CURSOR-REGRESSION.md` (Sprint 59 W2 PR-2 #575)
- **LOC estimation methodology**: `docs/PROCESS-LOC-ESTIMATION-METHODOLOGY.md` (Sprint 60 #582)
- **Parallel filler protocol**: `docs/PROTOCOL-PARALLEL-FILLER-OPT-IN-SCHEMA.md` (Sprint 61 #588)
- **LOC overrun automation**: `scripts/check_loc_overrun.sh` + `.github/workflows/loc-overrun-check.yml` (Sprint 61 #587)

---

## §12 Closeout — review request

Lead requests general + (optional) operator review of:
- **§3 P-ranking**：is P0/P1/P2 ordering aligned with operational priority? Sprint 60-61 cleanup mature observations correctly P0?
- **§5 dispatch shape**：is Wave 1/2/3 structure realistic for ~5-7-PR capacity?
- **§7 capacity envelope**：is balanced scenario (5-6 PRs) right OR strict-capacity / aspirational?
- **§8 scope appetite**：operator pre-IMPL preference question + Bopomofo IME 3rd Sprint deferral pattern
- **§9 risk register**：any missed risks?

Verdict expectations：VERIFIED + scope appetite ruling → IMPL dispatch begins。REJECTED + adjustment guidance → r1 PLAN revision。
