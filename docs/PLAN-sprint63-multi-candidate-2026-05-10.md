# Sprint 63 PLAN — Multi-Candidate Inventory + Dispatch Shape

**Date**: 2026-05-10
**Author**: lead
**Status**: PLAN draft v0 (awaiting general review + operator scope-appetite ruling)
**Source-of-truth**: `origin/main` HEAD `b4744e9` (Sprint 62 W2 PR-1 #593 just merged、Sprint 62 closeout achieved)
**Auto-trigger**: per operator-approved (A) auto-dispatch sprint PLAN on prior closeout (general dispatch m-20260510025525058971-543)

<!-- LOC-EST: 300-400 -->

---

## §0 Context

**Sprint 62 closeout** (2026-05-10) shipped 4 PRs (W1: 3 + W2: 1):
- W1 cleanup: FNV→SHA-256 + skills-stage GC + verdict-delivery confirmation protocol
- W2 closure-by-determination: teloxide upgrade evaluation revealed Bot API upstream limitation + Sprint 59 W2 F2 deferral CLOSED BY DETERMINATION
- All Sprint 60-61 deferrals closed cumulatively (Sprint 62 W1)
- ★ #587 automation full 3-path validation matrix complete in single Wave ★

**Sprint 62 highlights**：
- 4/4 PRs first-attempt VERIFIED (cleanest pattern preserved from Sprint 61)
- ~482 LOC total / ETA ~85%+ faster than 3-7hr core estimate
- 0 BYPASS / 100% LOC-EST marker compliance / 1 HARD-FAIL cohesion-accept Option (a) successful invocation
- New process pattern emerged: closure-by-determination
- Cumulative Sprint 56→62: 59 PRs merged (57 ledger-clean post-policy + 1 ⚠ scoped-bypass)

**Sprint 63 candidate pool** — 13+ items: Sprint 58 carried-over + multi-Sprint deferrals + cross-cycle cleanup。

**Capacity baseline**：~4-7 PRs per Sprint 60 (6) / 61 (4) / 62 (4) recent baseline。

**Sprint 63 theme** (proposed): **Sprint 58 P2 carry-over + multi-Sprint deferral cleanup**。Sprint 60-61 P0 infrastructure complete、Sprint 62 cleanup theme delivered。Sprint 63 focuses on long-deferred P2 items + remaining cross-cycle observations。

---

## §1 Goal

Sprint 63 ships:
1. Sprint 58 P2 carryover selective (1-3 items per scope appetite)
2. Sprint 58 protocol-audit candidates (4 items、bundle if scope per-item small)
3. Multi-agent test harness (P2、Sprint 60-62 deferrals reference)
4. Bopomofo IME direction execution (pending operator response: continue defer / cancel / closure-by-determination)

**Non-goals**:
- New feature work (Sprint 63 is cleanup-themed continuation)
- Major refactors (Sprint 60-62 mature patterns proceed as-is)

---

## §2 Verified state (origin/main b4744e9)

### Sprint 60-62 landings active
- Skills System (Sprint 60-61): full feature including auto-install + fleet.yaml override + SHA-256 prefix digest + skills-stage GC
- LOC overrun automation: helper script + GHA workflow + label override + 3-path validation matrix
- Parallel filler protocol + verdict-delivery confirmation protocol
- LOC estimation methodology + 5-category framework
- Engineering anti-stall arc: 10 PRs / 5 layers (Sprint 58→60)
- 5-tracker supervisor coexistence
- bind_self rebase mode + restart_daemon + force_release_worktree
- teloxide-driven Phase 2 IMPL CLOSED BY DETERMINATION (Sprint 62 W2)

### Tool count: 32 (post Sprint 60 #580 restart_daemon)

**Sprint 63 base**: HEAD b4744e9、CI green、no in-flight PRs、all worktrees released。

---

## §3 Inventory — P-ranking

### P0 — Sprint 58 carryover priority items (3-4 candidates、TBD specific roster)

[Specific roster restoration during dispatch construction needed — Sprint 58 closeout deferred-items roster reference]

#### P0-1 to P0-4. Sprint 58 P2 deferred items selective (placeholder)

- **Source**: Sprint 58 closeout deferred-items roster (8 items total、Sprint 63 selective top-priority)
- **Goal**: cherry-pick top-priority items for Sprint 63 W1 dispatch
- **LOC est per item**: ~20-100 LOC each (typical Sprint 58 P2 small-cleanup scope)
- **Tier**: Tier-1 single primary
- **Path**: Path A IMPL with smoke OR Path B doc/RCA depending on item type
- **Operator gate**: NO — general self-decide

**Action item for general**: provide Sprint 58 P2 deferred-items roster for accurate ranking。

### P1 — Operator-gate cross-cycle (1 candidate、4-Sprint deferral)

#### P1-1. Bopomofo IME — operator direction execution post-4-Sprint observation

- **Source**: Sprint 59 W2 PR-2 RCA #575 + 4-Sprint deferral observation (Sprint 60→61→62→63)
- **Operator decision pending** (per general telegram message_id 3961):
  - **(a) Continue defer**: Sprint 63 PLAN keeps as P1 + ⚠ operator-gate、await cross-backend confirmation any Sprint mid-cycle
  - **(b) Cancel**: Sprint 63 PLAN removes entry + archive as "operator-accepted current state"
  - **(c) Closure-by-determination**: Sprint 63 W1 cross-backend reproduction confirms backend-specific contamination → close as backend triage
- **Default if no operator response by Sprint 63 W2**: continue defer to Sprint 64 (5-Sprint observation point)
- **LOC est** (if (a) ships): ~5-25 LOC、Tier-1
- **LOC est** (if (b) cancel): 0 LOC (Sprint 63 PLAN edit)
- **LOC est** (if (c) closure-by-determination): ~50-100 LOC RCA Path B

### P2 — Carried-over + deferred (12+ candidates)

#### P2-A. Sprint 58 protocol-audit candidates remaining (4 items)

- **Source**: Sprint 58 closeout protocol-audit catalog post-#566
- **Goal**: bundle into single doc/RCA PR if scope per-item small
- **LOC est**: ~100-200 LOC bundled
- **Tier**: Tier-1 single primary
- **Path**: Path B doc/RCA bundle
- **Operator gate**: NO

#### P2-B. Multi-agent coordination test harness (3-Sprint deferral references)

- **Source**: Sprint 60 #583 §5 + Sprint 61 #588 §6 + Sprint 62 PLAN §3 P2-D
- **Goal**: Test harness simulating multi-agent dispatch scenarios for parallel-filler safety verification
- **LOC est**: ~150-300 LOC test infrastructure
- **Tier**: Tier-1 single primary
- **Path**: Path A test infrastructure
- **Operator gate**: NO

**Suggested handling**: defer Sprint 64+ unless operator/general specifically prioritizes — current single-agent #578 unit + e2e tests sufficient for current operations。

#### P2-C. Sprint 60-62 surfaced cleanup observations (if any remain unclosed)

- **Source**: Sprint 60-62 closeout observations
- **Status**: most addressed via Sprint 61 W1 + Sprint 62 W1 cleanup
- **Remaining items**: TBD pending Sprint 60-62 cleanup retrospective

---

## §4 Dependencies graph

```
P0 Sprint 58 carryover (selective) ──[independent each]── parallel-feasible if disjoint files

P1-1 Bopomofo IME ──[operator-gate]── 4-Sprint deferral resolution

P2-A protocol-audit bundle ──[independent]── async with P0/P1
P2-B multi-agent harness ──[independent]── Sprint 64+ candidate
```

---

## §5 Dispatch shape

### Recommended Wave structure (capacity ~4-6 PRs)

**Wave 1 — P0 Sprint 58 carryover (3-4 PRs、~3-5hr ETA)**
- W1 PR-1 to PR-3/4: Sprint 58 P2 deferred items selective per ranking
- W1 PR-N (optional)：P1-1 Bopomofo IME if operator confirms direction (continue defer / cancel / closure-by-determination) pre-Wave-1

**Wave 2 — P2 selective (0-2 PRs、~1-3hr ETA)**
- W2 PR-1 (optional)：P2-A Sprint 58 protocol-audit bundled doc/RCA
- W2 PR-2 (optional)：P1-1 if operator confirms mid-Sprint

**Wave 3 — Sprint 63 closeout + carryover (0-1 PR)**
- W3 PR-1 (optional)：1 cherry-picked P2 item if Sprint 63 W2 capacity remains

### Sequential vs parallel

Sequential default per Sprint 61 #588 Option B protocol。Parallel opt-in via formal `parallel-filler: opt-in (vs PR-X; file surface: <paths>)` marker for explicit disjoint-surface cases。

---

## §6 Operator gates

### Pre-IMPL operator gate
- **P1-1 Bopomofo IME**: operator direction (a)/(b)/(c) pending per 4-Sprint deferral observation
- **P0 Sprint 58 carryover selection**: if Sprint 63 capacity allows >3 items、operator review pick

### Auto-proceed without operator
- All other P0/P2 items (general self-decide per current authority)

---

## §7 Total estimate aggregate

### Capacity envelope: ~4-6 PRs / ~4-9hr ETA total

| Bucket | PRs (typical) | LOC range | ETA range |
|---|---|---|---|
| P0 Sprint 58 carryover | 3-4 | 60-400 LOC | 3-5hr |
| P1 cross-cycle | 0-1 | 0-100 LOC | 0-1hr |
| P2 select | 0-2 | 0-300 LOC | 0-3hr |
| **Total** | **3-7 PRs** | **60-800 LOC** | **3-9hr core** |

### Strict-capacity scenario (3 PRs)
- W1: 3 P0 Sprint 58 carryover items
- W2/W3: skip
- All P1/P2 → Sprint 64

### Aspirational scenario (6-7 PRs)
- W1: 4 P0 Sprint 58 + 1 P1-1 (5 PRs)
- W2: 1 P2-A + 1 P2-B (2 PRs、Sprint 64+ unlikely)

---

## §8 Scope appetite recommendation

**Lead recommendation: balanced scenario** (4-5 PRs、Wave 1 P0 Sprint 58 + Wave 2 P2 selective)

Rationale:
1. **P0 Sprint 58 carryover**: 8 items remaining、selective 3-4 maintains Sprint 60-62 cadence
2. **P1-1 Bopomofo**: opportunistic ship if operator confirms direction (any of 3 options)
3. **P2 selective**: 1 P2-A protocol-audit bundle if W2 capacity remains
4. **Sprint 64 carry-over preserved**: P2-B multi-agent harness + remaining Sprint 58 P2 + any new Sprint 63 surfaced items

**Operator scope-appetite question** (§8): (i) infrastructure-cleanup-only (P0 only) / (ii) balanced per lead recommendation / (iii) feature-heavy (P0 + P1 + P2-A bundle)
Default: balanced per general auto-proceed mode if no operator override within 24hr post-PLAN-merge。

---

## §9 Risks + mitigations

### Risk 1: Sprint 58 P2 deferred-items roster restoration accuracy
- **Mitigation**: lead reviews Sprint 58 closeout synth pre-W1-PR-1 dispatch、verifies items still relevant + LOC estimates current

### Risk 2: Bopomofo IME 4-Sprint deferral pattern continues into Sprint 64
- **Mitigation**: Sprint 64 closeout surface 5-Sprint deferral observation if pattern persists、escalate operator decision urgency

### Risk 3: Multi-agent harness P2-B accumulates 4-Sprint deferral signal
- **Mitigation**: Sprint 64+ consider closure-by-determination if multi-agent use cases remain hypothetical

### Risk 4: Faster-than-estimate pattern (85%+ Sprint 62) tempting to over-pack
- **Mitigation**: respect balanced scope appetite floor (~4-6 PRs)、avoid aspirational over-commit

### Risk 5: Sprint 60-62 mature patterns regression (cohesion-accept / closure-by-determination)
- **Mitigation**: Sprint 63 PRs continue marker compliance + adjudication patterns、recur feedback memory if drift

---

## §10 IMPL gate / next steps

**Auto-trigger**: Sprint 63 PLAN VERIFIED → general dispatches Wave 1 PR-1 (P0 Sprint 58 first carryover item) per auto-proceed mode

**Operator pre-Wave-1 review** (optional): operator may flag scope-appetite preference + Bopomofo IME direction (a/b/c)

**Default next step**: post-PLAN-merge、lead requests Sprint 58 P2 deferred-items roster from general OR Sprint 58 closeout synth references for accurate W1 PR-1 dispatch construction

---

## §11 Cross-references

- **Sprint 62 closeout synth**: lead m-20260510025220...
- **Sprint 60 PLAN reference**: `docs/PLAN-sprint60-multi-candidate-2026-05-10.md`
- **Sprint 61 PLAN reference**: `docs/PLAN-sprint61-multi-candidate-2026-05-10.md`
- **Sprint 62 PLAN reference**: `docs/PLAN-sprint62-multi-candidate-2026-05-10.md`
- **Bopomofo IME RCA**: `docs/RCA-BOPOMOFO-IME-CURSOR-REGRESSION.md` (Sprint 59 W2 PR-2 #575)
- **Closure-by-determination pattern**: `feedback_closure_by_determination_pattern.md` (Sprint 62 W2 PR-1 #593)
- **LOC estimation methodology**: `docs/PROCESS-LOC-ESTIMATION-METHODOLOGY.md` (Sprint 60 #582)
- **LOC overrun automation**: `scripts/check_loc_overrun.sh` + `.github/workflows/loc-overrun-check.yml` (Sprint 61 #587)

---

## §12 Closeout — review request

Lead requests general + (optional) operator review of:
- **§3 P-ranking**: Sprint 58 P2 carryover prioritization需要 specific roster restoration、Bopomofo IME direction pending operator
- **§5 dispatch shape**: Wave 1 capacity for 3-4 P0 items realistic?
- **§7 capacity envelope**: 4-6 PRs / 4-9hr core ETA aligned with Sprint 60-62 baseline?
- **§8 scope appetite**: balanced default appropriate?
- **§9 risk register**: Risk 2 (Bopomofo 4-Sprint persistence) + Risk 3 (multi-agent harness 4-Sprint deferral pattern) noted?

Verdict expectations：VERIFIED + scope appetite ruling → IMPL dispatch begins。REJECTED + adjustment guidance → r1 PLAN revision。
