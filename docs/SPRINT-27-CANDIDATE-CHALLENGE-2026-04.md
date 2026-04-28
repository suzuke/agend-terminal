# Sprint 27 candidate challenge — 2026-04

Operator m-123 + m-198 directive: Sprint 27 主軸 = `state.rs` regex matching → behavioral
inference (PTY silence + cursor pos + echo probe + fg pgid) 跨 5 backends。Operator
m-123 explicit: 「sprint27 專心做這一塊，不要排其他事情」 — full sprint capacity 投入。

Per operator m-198 directive: dev team 4-perspective challenge round, dev-lead synthesizes.
This doc captures all 4 perspectives + recommended Sprint 27 scope for operator approval.

## 4 perspectives

- **dev-impl-1 (minimal/skeptical)** — *authored by dev-lead due to impl-1 kiro-cli context
  exhaustion, per operator m-198 transparency directive*
- **dev-impl-2 (structural)** — m-202 (own collected verdict)
- **dev-reviewer (prior-art)** — m-208 (own collected verdict)
- **dev-reviewer-2 (cost/benefit)** — m-205 (own collected verdict)

## Convergent baseline (4/4 agree)

- **GO Sprint 27 (NOT NO-GO, NOT DEFER)** — operator commitment + whack-a-mole evidence
  empirically grounds need
- **NOT full single-sprint XL epic** — phased rollout with shadow-mode/tiebreaker first
- **Repurpose 13 fixtures (NOT retire)** — ground truth + regex backstop + calibration anchor
- **§3.5.10 wire-format external-fixture MANDATORY** — mock-pair self-validation trap risk
  identical to PR #267 r3 lesson; real backend spawn or fixture-replay-with-original-timing only
- **§3.5.11 strict commit-order or §3.5.11 r3 empirical-revert** — per Sprint 25-26 dogfood

## Per-perspective synthesis

### #1 dev-impl-1 minimal/skeptical (dev-lead self-author)

**Stance**: GO Sprint 27 with **single-backend Phase 1** (claude only), even smaller than
impl-2/reviewer-2 Phase 1 design.

**Reasoning** (skeptical of consensus Phase 1 scope):
- (a) **Per-backend calibration risk under-estimated** — even claude alone (biggest fleet share)
  could surface unexpected DSR timing / silence threshold edge cases that consume planned
  Sprint 27 capacity. Adding kiro-cli (Phase 1 consensus) compounds calibration debugging.
- (b) **Engine + 1-backend** = ship one full vertical slice; engine + 2-backends = horizontal
  spread without depth. Operator m-123 「不排他事」commitment supports going **deep** not wide.
- (c) **YAGNI on multi-backend prematurely** — claude-code + kiro-cli signals may diverge
  enough that single trait abstraction breaks; learn from claude first, generalize from
  empirical Phase 2.
- (d) **Cheaper alternative if Phase 1 trips** — keep regex-only for 4 non-claude backends,
  ship behavioral-only for claude with 5 backends total still functional.

**Counter-argument self-audit**:
- structural (impl-2) will argue: single-backend engine is over-fit; trait surface needs ≥2
  impls to validate generalization (PR #230 channel trait pattern lesson).
- prior-art (dev-reviewer) confirms greenfield + existing primitives ready — engine cost is
  higher than per-backend calibration cost; skipping kiro-cli saves marginal LOC, doesn't
  significantly de-risk.
- cost-benefit (reviewer-2) argues claude+kiro-cli ~80% fleet traffic at ~50% LOC = ROI sweet
  spot; single-backend Phase 1 cuts to ~50% fleet, doesn't halve LOC (engine dominates).

**When minimal beats consensus**:
- (1) **Operator UX stability priority** — single-backend phased rollout reduces "operator sees
  inference disagree across backends mid-sprint" risk; one-at-a-time controlled introduction.
- (2) **De-risking calibration emergence** — if claude-only behavioral test surfaces new DSR
  timing complexity, Sprint 27 has slack to address it; if 2-backend Phase 1 surfaces, harder
  to bound scope.

**Verdict**: GO Sprint 27 Phase 1 **claude-only** (~500-700 LOC engine + claude calibration);
Sprint 28 adds kiro-cli; Sprint 29 batches remaining 3 backends.

### #2 dev-impl-2 structural (m-202)

**Stance**: GO Phase 1 Sprint 27 ~200 LOC tiebreaker mode.

**Trait design**: Single `BehavioralProbe` trait with default impls + per-backend override
via `impl for Backend`. Methods: `silence_threshold` / `is_thinking_by_silence` /
`cursor_query_supported` / `fg_pgid_inference`. Calibration as `BackendPreset` fields
(compile-time, not YAML — backend-version-specific).

**3-phase opt-in**:
- **Phase 1** Sprint 27 (~200 LOC): behavioral as **tiebreaker only** (fills None gaps,
  zero change to regex-detected states)
- **Phase 2** Sprint 28+ (~300 LOC): behavioral primary with env var opt-in (cursor query
  + fg pgid added)
- **Phase 3** Sprint 28+ (~100 LOC): swap priority (behavioral default, regex fallback)

**Hybrid model**: `behavioral_detect()` runs first → if `None`, regex `detect()` fallback.
13 fixtures KEEP (test regex path). New behavioral fixtures added.

**§3.5.10**: Option 4 not-Send escape — behavioral signals collected in supervisor tick
thread, passed as plain values to `StateTracker::feed`. Cursor query is same-thread PTY
write/read.

**LOC estimate**: Phase 1 ~200 (state.rs +100, supervisor.rs +50, backend.rs +50).
Phase 2 ~300, Phase 3 ~100.

### #3 dev-reviewer prior-art (m-208)

**Stance**: GO conditional with **6 commit-hash-anchored conditions**.

**Whack-a-mole empirically confirmed**: 8 per-version regex patches in `src/state.rs` over 18
months (`5c95982` Claude / `24ff0e0` Codex / `a638311` Gemini / `a034900` Codex / `bd452cd`
Kiro / `039f74d` OpenCode / `407f31d` Claude / `e6c3be2` Claude). Sprint 27 thesis grounded.

**Behavioral attempts past**: ZERO matches in `src/state.rs` history — greenfield for
behavioral inference.

**Existing primitives ALREADY EXIST in `src/backend_harness.rs`**:
- `verify_tcgetpgrp()` (commit `a49cb1e`) — fg pgid probe Unix
- `probe_esc_stops_generation()` — behavioral pattern PROTOTYPED (spawn → wait ready → send
  prompt → inject ESC → observe). **This IS the pattern Sprint 27 wants, scaled to detection.**
- `src/vterm.rs:31-56 PtyWriteListener` — DSR/CPR plumbing IN PLACE; cursor-pos echo-probe
  infrastructure ready.

**Leverage not rebuild**.

**13 fixtures REPURPOSE**:
1. Ground truth for shadow-mode divergence measurement
2. Regex backstop validation (PROMPT_REGEX named fallback retained permanently)
3. Per-backend calibration anchor when behavioral signal ambiguous

**Anti-pattern recurrence HIGH risk without shadow-mode gate**: behavioral signals (PTY
silence threshold, cursor-pos timing) NOT grep-able post-hoc — worse than regex for
whack-a-mole detection unless shadow-mode logs divergence per-backend continuously.

**§3.5.10 mock-pair lesson identical risk class**: synthesized PTY emulator with hand-crafted
DSR responses → behavioral test PASSES. Real backend binaries diverge. **wire-format
external-fixture MUST apply** — real backend spawn or fixture-replay-with-original-timing,
NOT impl-synthesized PTY emulator.

**6 GO conditions**:
1. Shadow-mode rollout ≥1 sprint, gate threshold divergence ≤5% per-backend OR revert
2. Real PTY external-fixture (no synthesized emulator) per §3.5.10 r3
3. §3.5.11 test-first split commits OR r3 empirical-revert
4. Repurpose 13 fixtures as ground truth; assert replay regex+behavioral match rate ≥95%
5. Leverage existing primitives (`verify_tcgetpgrp` / `probe_esc_stops_generation` /
   `PtyWriteListener`) — promote not rebuild
6. Default flip deferred Sprint 28+ — Sprint 27 ships shadow-mode + divergence dashboard
   only; regex stays default

**NO-GO trigger**: shadow-mode shows divergence >5% per-backend OR cross-backend signals
require per-backend calibration tables (whack-a-mole resurface) → patch-surface-shift not
root-fix; revert.

### #4 dev-reviewer-2 cost/benefit (m-205)

**Stance**: GO Phase 1 Sprint 27 (engine + claude + kiro-cli + cross-platform skeleton);
Phase 2 Sprint 28 remaining 3 backends.

**Cost**: XL (~2800-4200 LOC total).
- state.rs rewrite: ~500-800
- Per-backend calibration (5 × 4-6 modes × ~50 LOC harness): ~1500
- Cross-platform PTY introspection (Unix `tcgetpgrp` + Windows `ConsoleProcessList`): ~200-400
- DSR cursor query (`\x1b[6n` send + parse + timeout): ~100-200
- Test infrastructure (real-backend integration + behavioral assertion): ~500-800

**Phase 1 Sprint 27**: engine + claude + kiro-cli + cross-platform skeleton + integration
test harness ~1500-2000 LOC = ~80% fleet traffic at ~50% total LOC. **ROI sweet spot.**

**Phase 2 Sprint 28**: codex + gemini + opencode + DSR cursor query + production validation
harness ~1300-2200 LOC.

**Per-backend impact**:
- claude + kiro-cli: HIGH complexity + frequent mode changes → Phase 1 priority
- codex + gemini + opencode: lower complexity → Phase 2

**state_pattern_coverage fixtures (PR #269)**: KEEP as regression backstop. Storage trivial
(~13kB). Behavioral inference v1 won't be 100% accurate → fixtures = fallback baseline.
Sprint 28+ housekeeping retire IFF inference reaches >99% production accuracy.

**Benefit**: operator-visible 5/5 (state column most-visible TUI element); maintenance debt
4/5 (regex-per-version → OS signals stable); failure-rate 3/5 (mis-classify reduces alert
fatigue).

## Convergence + recommended Sprint 27 scope

### 4-perspective convergence on critical conditions

| Condition | impl-1 (minimal) | impl-2 (structural) | dev-reviewer (prior-art) | reviewer-2 (cost/benefit) |
|---|---|---|---|---|
| GO Sprint 27 | ✓ (single-backend) | ✓ (Phase 1) | ✓ (conditional) | ✓ (Phase 1 2-backend) |
| Shadow-mode/tiebreaker first | ✓ | ✓ tiebreaker | ✓ shadow ≥1 sprint | ✓ |
| Real PTY external-fixture (NOT mock-pair) | ✓ | ✓ Option 4 | ✓ §3.5.10 r3 mandatory | ✓ |
| Leverage existing primitives | (implicit) | (compatible) | ✓ explicit (m-208 §3) | (compatible) |
| 13 fixtures REPURPOSE | (compatible) | ✓ keep | ✓ explicit | ✓ keep |
| Default flip Sprint 28+ | ✓ | ✓ Phase 2/3 later | ✓ explicit | ✓ |

### Recommended Sprint 27 scope (single-PR wave or multi-PR per phase)

**Sprint 27 scope** (~1500-2000 LOC, fits 1 sprint with operator m-123 full capacity):

**PR-A engine + claude+kiro-cli shadow-mode** (~1200-1500 LOC):
- `BehavioralProbe` trait per impl-2 m-202 design (single trait + per-backend `BackendPreset`
  override fields)
- Engine wires probe outputs into `StateTracker` as **shadow-mode metric only** (regex stays
  primary; behavioral output logged to telemetry per backend per state, no state change)
- claude + kiro-cli `BackendPreset` calibration (silence_thinking_ms / silence_idle_ms /
  cursor_query_supported / fg_pgid_inference)
- Leverage existing primitives: promote `verify_tcgetpgrp` (`a49cb1e`) + `PtyWriteListener`
  DSR plumbing (`src/vterm.rs:31-56`) + `probe_esc_stops_generation` (`src/backend_harness.rs:160`)
- Cross-platform skeleton: Unix `tcgetpgrp` + Windows `ConsoleProcessList` stub (full Windows
  Phase 2)

**PR-B real-backend external-fixture test** (~300-500 LOC):
- §3.5.10 r3 wire-format real PTY: real claude / real kiro-cli binary spawn OR replay
  13 captured `.raw` fixtures at original timing (NOT synthesized PTY emulator)
- §3.5.11 strict split: RED test-only commit (assert behavioral ≠ regex divergence ≤5%) →
  GREEN impl
- Divergence dashboard: log behavioral_state vs regex_state per-backend per-tick to telemetry;
  operator-runnable summary command (e.g. `agend-terminal state-divergence-report --since 1h`)

**Sprint 27 NOT included (deferred Sprint 28+)**:
- codex + gemini + opencode calibration → Phase 2 sprint
- Default flip (behavioral primary) → Sprint 28+ if shadow ≥1 sprint divergence ≤5%
- Full Windows ConsoleProcessList impl → Phase 2 sprint
- DSR cursor query full impl → Phase 2 sprint (Phase 1 ships skeleton + opt-in)
- state_pattern_coverage retirement → Sprint 28+ housekeeping IFF inference >99% prod accuracy

### Operator decisions required

1. **Sprint 27 scope: 2-backend (claude+kiro-cli) per consensus OR 1-backend (claude only)
   per dev-lead minimal-delta?**
   - Recommend **2-backend per consensus** (dev-lead minimal stance over-conservative;
     prior-art confirms greenfield + existing primitives ready — engine dominates LOC, kiro-cli
     marginal addition; reviewer-2 ROI 80% fleet at 50% LOC).
2. **Shadow-mode default duration**: ≥1 sprint per dev-reviewer m-208 condition #1, or
   shorter/longer?
   - Recommend **≥1 sprint (Sprint 27 deploy → Sprint 28 evaluate divergence dashboard)**.
3. **Default flip threshold**: divergence ≤5% per-backend (dev-reviewer m-208) OR more
   stringent ≤1% / ≤10%?
   - Recommend **≤5%** (strict enough to avoid silent UX shift; loose enough to allow
     legitimate edge-case divergence operator can investigate).
4. **PR-B test wire-format choice**: real backend binary spawn (high CI cost, accurate) OR
   fixture replay at original timing (lower CI cost, accuracy depends on capture quality)?
   - Recommend **fixture replay** for CI fast-path + real backend spawn for nightly matrix
     (per PR #270 dev-reviewer m-194 hybrid pattern).
5. **operator UX shift opt-in mechanism for Sprint 28+ flip**: env var (per impl-2 m-202
   Phase 2) OR config field (`fleet.yaml`) OR auto-flip post-shadow-mode?
   - Recommend **env var** for Sprint 28 opt-in (consistent with §3.5.13 verdict externalization
     mechanism for similar progressive rollout); Sprint 29+ auto-flip if shadow-mode green ≥1
     sprint.

## Cross-amendment dependencies

Sprint 27 PRs ship under §3.5.10 + §3.5.11 + §3.5.12 + §3.5.13 + §3.6 enforcement (all LIVE
post-PR #271):
- §3.5.10 wire-format external-fixture (3-class) — real backend spawn / fixture replay; NO
  synthesized PTY emulator (PR #267 r3 mock-pair lesson explicit per dev-reviewer m-208)
- §3.5.11 strict commit-order OR §3.5.11 r3 empirical-revert — operator m-69 strict +
  reviewer-2 m-165 consistent enforcement; r3 escape only if architectural impossibility
- §3.5.12 deferred-defense — don't add new defers without dual + operator post_decision
- §3.5.13 verdict externalization — mirror to GH PR comments per operator m-84 永久 rule
- §3.6 async pipeline — impl/reviewer 不等 CI/merge, dev-lead persist + watch + self-merge

## Sprint 27 amendment-batch implications

Per operator m-177 amendment batching: amendment ideas surfacing during Sprint 27 implementation
accumulate to Sprint 27-end batch PR. Likely candidates:
- §3.5.10 r4 fixture-replay-original-timing exemption refinement (if real backend spawn 過
  expensive emerges as common case)
- §3.6 polling cadence / divergence-threshold tuning if Phase 1 dogfood reveals process gaps
- Behavioral inference shadow-mode → primary flip protocol formalization

## Source verdicts (4 perspectives)

- impl-1 minimal/skeptical: dev-lead self-author (current doc §1)
- impl-2 m-202 structural (2026-04-28 05:20)
- dev-reviewer m-208 prior-art (2026-04-28 05:47)
- dev-reviewer-2 m-205 cost/benefit (2026-04-28 05:23)
