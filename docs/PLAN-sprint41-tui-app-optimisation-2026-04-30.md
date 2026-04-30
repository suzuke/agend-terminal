# PLAN: Sprint 41 — Group 2 TUI/App optimisation + coverage uplift

**Date**: 2026-04-30
**Basis**: main HEAD `0498a13` (post Sprint 39 merges)
**Operator brief**: general m-297 at 2026-04-30T05:19Z — second per-group impl following Sprint 40 closeout.
**Team**: lead2 (orchestrator, MINIMAL-DELTA + KISS dissent) · dev2 / kiro-cli (STRUCTURAL with logic-vs-glue) · dev2-1 / kiro-cli (PRIOR-ART, reviewer2 substitute due to codex usage-limit)
**Status**: ready for operator §13 answers

---

## 0. Scope (frozen by operator)

Per general m-297:
- **In-scope**: cleanup of Group 2 TUI/App files (`docs/ARCHITECTURE-GROUPS.md` Group 2/7). Two angles **AND** required:
  1. **Optimisation** — boundary leaks / paranoia leftovers / over-abstraction (Sprint 28 RBAC + Sprint 35 fleet-broadcast + Sprint 40 boundary-extraction lineage)
  2. **Coverage uplift** — file-by-file with **logic-vs-glue gate**: real logic gets tests, pure ratatui glue accepted-as-glue rather than pretend-tested
- **Operator philosophy**: "ROI debatable per ratatui glue" — every file must explicitly answer "logic worth covering?" rather than chasing % vanity. Bias toward PERMANENT logic pins; document NO_CHANGE for glue.
- **Out-of-scope**:
  - Snapshot tooling introduction (`cargo insta`, `ratatui-test`) — Sprint 42 TUI test harness PLAN (claude-a1f200 `plan/tui-test-harness` 4a0abaf) explicitly defers to Phase 4+
  - Other groups
  - TUI behaviour change / new features
  - Test harness redesign (Sprint 42 dedicated PLAN)

---

## 1. Coverage baseline (real measurement, dev2 m-304)

`cargo llvm-cov` at HEAD `b98d505`, 14 files / ~10,437 LOC:

| File | LOC | Cov % | Inline tests | Logic% | Glue% |
|---|---|---|---|---|---|
| `src/layout.rs` | 2106 | 75.5 % | 37 | 90 | 10 |
| `src/keybinds.rs` | 374 | 68.8 % | 15 | 95 | 5 |
| `src/app/mouse.rs` | 625 | 47.6 % | 5 | 60 | 40 |
| `src/app/overlay.rs` | 1196 | 41.5 % | 8 | 50 | 50 |
| `src/render.rs` | 2385 | 40.0 % | 19 | 20 | 80 |
| `src/app/tui_events.rs` | 697 | 29.3 % | 6 | 40 | 60 |
| `src/app/pane_factory.rs` | 527 | 23.8 % | 5 | 30 | 70 |
| `src/tui.rs` | 213 | 20.0 % | 3 | 10 | 90 |
| `src/app/mod.rs` | 1039 | 15.3 % | 4 | 20 | 80 |
| `src/app/commands.rs` | 325 | **0.0 %** | 0 | 70 | 30 |
| `src/app/dispatch.rs` | 330 | **0.0 %** | 0 | 50 | 50 |
| `src/app/session.rs` | 395 | **0.0 %** | 0 | 80 | 20 |
| `src/app/telegram_hooks.rs` | 88 | **0.0 %** | 0 | 0 | 100 |
| `src/app/api_server.rs` | 137 | **0.0 %** | 0 | 0 | 100 |

**Logic-vs-glue gate** (per operator's "ROI debatable per ratatui glue"):
- 8 files have ≥30 % logic content worth covering
- 4 files are pure glue (`api_server.rs`, `telegram_hooks.rs`, `tui.rs`, `app/mod.rs`) — accept-as-glue per gate
- 2 files already adequate (`layout.rs` 75.5%, `keybinds.rs` 68.8%) — NO_CHANGE

**Real low-cov logic hotspots**: `session.rs` (0% / 80% logic), `commands.rs` (0% / 70% logic), `dispatch.rs` (0% / 50% logic), `pane_factory.rs` (23.8% / 30% logic).

---

## 2. STRUCTURAL findings (dev2 m-304)

dev2's per-file analysis applied logic-vs-glue gate explicitly. Coverage-uplift focus, 4-PR proposal:

| # | PR | Files | LOC | Risk | Type |
|---|---|---|---|---|---|
| dev2 T-1 | session.rs save/restore round-trip (§3.5.10 persistence-replay) | 1 | +35 | MEDIUM | PERMANENT |
| dev2 T-2 | commands.rs + dispatch.rs parsing/routing | 2 | +50 | LOW | PERMANENT |
| dev2 T-3 | render.rs hit-test logic + mouse.rs drag state | 2 | +55 | LOW | PERMANENT |
| dev2 T-4 | overlay.rs search + tui_events.rs key mapping + pane_factory.rs config | 3 | +50 | LOW | PERMANENT |

**Total dev2 proposal**: +225 LOC tests across 8 files; 6 files NO_CHANGE.

### 2.1 Critical gap in dev2 STRUCTURAL

dev2 found **0 paranoia leftovers, 0 boundary leaks, 0 over-abstraction** — same coverage-only narrowing as Sprint 40. That mismatches operator's BOTH-AND brief (optimisation **AND** coverage). dev2-1 PRIOR-ART filled the gap (§3 below). This recurring pattern is itself worth flagging: dev2's STRUCTURAL audits systematically miss the optimisation lane.

---

## 3. PRIOR-ART findings (dev2-1, reviewer2 substitute)

### 3.1 Recent TUI fix-shape templates

| # | Template | Source | Apply to |
|---|---|---|---|
| T1 | Test-first 2-commit cosmetic cleanup (red asserts absence → green removes) | PR #322 `[state]` suffix removal | render.rs cosmetic refactors |
| T2 | Test-first 3-commit cross-layer feature with §3.5.10 fixture | PR #326 `pane_snapshot` MCP tool | Cross-layer additions |
| T3 | Adversarial buffer-shape cases (gemini-banner: content-then-blanks) | PR #334 trim-before-windowing | vterm/render bug fixes |
| T4 | Centralised overlay sizing helper (`centered_overlay_rect` / `clamp_overlay_dim`) | Sprint 21 Phase 4 a2acf75 | Any new overlay or overlay refactor |

**Pattern summary**: All recent TUI PRs follow §3.5.11 test-first 2-3 commit shape. No exceptions.

### 3.2 Paranoia / defensive-code scan

dev2-1 ran §3.5.12(d) counter-example analysis:

| ID | Location | Verdict |
|---|---|---|
| `pane.backend.is_some()` guards (render.rs:134, 491; layout.rs:82) | `Backend::from_command()` returns `Option<Backend>` for unrecognised commands → `None` reachable in production | **KEEP** — legitimate, not paranoia |
| RBAC residue scan in TUI scope | Sprint 28 PR #285 cleanup complete, only `PermissionPrompt` AgentState variant remains | CLEAN |
| Fleet-update broadcast residue | Sprint 35 PR #333 cleanup complete; `:broadcast` is user-facing MCP, not deleted fleet-update | CLEAN |
| TODO/FIXME/HACK | Zero hits in TUI scope | CLEAN |
| MovePaneTarget defensive `source_pane_id` / `source_tab_idx` (overlay.rs:91-93) | Self-documented defensive intent, ~0 LOC cost, modal enforcement guard | **KEEP** |

**No paranoia removal candidates worth shipping.** TUI scope already cleaned across Sprint 28/29/35.

### 3.3 Boundary findings (TUI ↔ App ↔ Render)

dev2-1's primary "permanent" findings missed by dev2:

| ID | Location | Leak | Suggested action | Risk |
|---|---|---|---|---|
| **F1** | `render.rs:485` `pane.drain_output()` inside `render_pane()` | State mutation during render walk | Move drain to event loop pre-`terminal.draw()` | MEDIUM |
| **F2** | `render.rs:1920-1927` `render_scratch_shell` drain + vterm resize + PTY resize | I/O side-effect inside render fn (scratch shell outside layout tree) | Special-case overlay drain in event loop | MEDIUM-HIGH |
| **F3** | `render.rs:16-23` `TelegramStatus` enum defined in render layer | Channel-specific type in render module | Move to `channel/` or shared types module | **LOW** (mechanical) |
| **F4** | `render.rs:277-307` `resize_panes()` mutates layout + dispatches PTY resize | Layout/sizing logic + I/O in render.rs | Extract to layout sizing module | **LOW-MEDIUM** (already called from app/mod.rs, not from terminal.draw) |
| **F5** | `app/telegram_hooks.rs` thin pane-lifecycle → channel-API hooks | App layer orchestrates channel-specific lifecycle | Event-driven via PaneCreated/PaneDeleted events | MEDIUM (requires event infra not yet present) |

**Cheap mechanical optimisation candidates**: F3 (TelegramStatus move) + F4 (resize_panes extraction). F1/F2 are pragmatic trade-offs (drain-what-you-render avoids backlog management); F5 needs event infrastructure that doesn't exist.

### 3.4 Snapshot / ratatui-test tooling — DO NOT INTRODUCE

- `cargo insta` / `ratatui-test` not in Cargo.toml; zero codebase references outside PLAN docs
- Sprint 42 TUI test harness PLAN (`plan/tui-test-harness` 4a0abaf) explicitly defers snapshot tooling to Phase 4+
- Existing TUI test precedent: 2 hand-rolled `TestBackend` usages in render.rs (lines 2163, 2186) — sufficient for assertion-based tests
- **Recommendation**: Sprint 41 uses existing TestBackend pattern. Defer insta/ratatui-test evaluation to Sprint 42's dedicated tooling sprint.

---

## 4. MINIMAL-DELTA synthesis (lead2 KISS dissent)

### 4.1 Reconcile dev2 + dev2-1

dev2 proposes **coverage-uplift only** (4 PRs); dev2-1 surfaces **2 cheap structural cleanups** dev2 missed (F3, F4) plus 1 deferred (F5 event infra). Operator's BOTH-AND directive applied:

- **KEEP** dev2 T-1 (session.rs) — highest logic%, real persistence path, MEDIUM-risk worth pinning
- **KEEP** dev2 T-3 (render hit-test + mouse drag) — pure coordinate math + state-machine transitions, regression-prone
- **SCRUTINISE** dev2 T-2 (commands.rs parsing + dispatch.rs routing) — keep PERMANENT subset (logic outcomes), avoid pinning exact string formatting / table layout
- **SCRUTINISE** dev2 T-4 (overlay search + key mapping + pane config) — overlay search is real logic; key mapping has churn risk if mappings change deliberately. KEEP overlay + pane_factory; **defer tui_events key mapping** to "as bug occurs" or fold into Sprint 42 harness sprint.
- **ADD** F3 (TelegramStatus move) — cheap mechanical, ~3 import changes
- **ADD** F4 (resize_panes extraction) — already called from app/mod.rs, mechanical extraction
- **DEFER** F1/F2 (drain_output / scratch_shell) — pragmatic trade-offs, deferring loses nothing
- **DEFER** F5 (telegram_hooks event infra) — requires infrastructure not yet present; better as Sprint 43+ event-layer sprint

### 4.2 Proposed Sprint 41 PR backlog (merged)

| # | PR | Lane | LOC | Risk | Tier | Type | Sources |
|---|---|---|---|---|---|---|---|
| **T-1** | `session.rs` save/restore round-trip (§3.5.10 persistence-replay) | coverage | +35 | MEDIUM | Tier-1 | PERMANENT | dev2 T-1 |
| **T-2** | `commands.rs` parsing + `dispatch.rs` action routing — **logic-outcome assertions only** | coverage | +50 | LOW | Tier-1 | PERMANENT | dev2 T-2 narrowed |
| **T-3** | `render.rs` hit-test logic (`tab_bar_hit_test`, `overlay_hit_test`) + `mouse.rs` drag state machine | coverage | +55 | LOW | Tier-1 | PERMANENT | dev2 T-3 |
| **T-4** | `overlay.rs` search filter + `pane_factory.rs` fleet.yaml config resolution (tui_events excluded) | coverage | +35 | LOW | Tier-1 | PERMANENT | dev2 T-4 narrowed |
| **T-5** | **F3 TelegramStatus move out of render.rs** — mechanical relocation to channel module | structural | +0 / -8 (move) | LOW | Tier-1 | PERMANENT | dev2-1 F3 |
| **T-6** | **F4 resize_panes extraction** — pure layout/sizing function moves out of render.rs | structural | +0 / -30 (move) | LOW-MEDIUM | Tier-1 | PERMANENT | dev2-1 F4 |

**Estimated total**: +175 / -38 ≈ **net +137 LOC** (mostly tests + mechanical moves). All Tier-1 (no daemon-routing boundary touched).

### 4.3 Explicitly NOT in Sprint 41 backlog

- **dev2 T-4 tui_events key mapping** — mapping changes are intentional UX shifts; pinning incurs churn cost > catch rate. Defer to Sprint 42 harness sprint where ratatui-test or similar handles it idiomatically.
- **dev2-1 F1 drain_output relocation** — pragmatic trade-off, drain-what-you-render avoids backlog management. KEEP.
- **dev2-1 F2 scratch_shell drain+resize** — MEDIUM-HIGH risk; scratch shell is special-cased outside layout tree. Defer.
- **dev2-1 F5 telegram_hooks event-driven refactor** — requires event infrastructure not yet present. Sprint 43+ event-layer sprint.
- **insta / ratatui-test introduction** — Sprint 42 dedicated tooling sprint per claude-a1f200 PLAN.
- **render.rs glue cov uplift** — accept at 40 % per logic-vs-glue gate; not worth pretend-testing widget assembly.
- **app/mod.rs / tui.rs / api_server.rs / telegram_hooks.rs** — pure glue per gate; NO_CHANGE.

### 4.4 PERMANENT vs TRADE-OFF labelling (per operator philosophy)

All 6 backlog items are **PERMANENT**:
- T-1/T-2/T-3/T-4 are coverage pins on stable input/output contracts (session JSON, command strings, hit-test math, search filters) — survive any subsequent refactor
- T-5/T-6 are mechanical module-level moves — once channel type lives in channel module and resize lives in layout sizing module, they don't drift back

**No TRADE-OFF items in Sprint 41 backlog.** Items dev2 suggested with churn risk (key mapping pinning) are explicitly excluded; T-2/T-4 narrowing to logic outcomes is the PERMANENT alternative.

---

## 5. §13 decisions surfaced for operator

### Q1 — Sprint 41 scope expansion (BOTH-AND vs coverage-only)

**Recommendation**: **Accept the expanded backlog (T-1 .. T-6)** rather than dev2's 4-PR coverage-only narrower view. Rationale: operator's brief explicitly lists optimisation AND coverage. dev2 STRUCTURAL found 0 paranoia + 0 boundaries (recurring pattern from Sprint 40). dev2-1 surfaced F3 + F4 as cheap structural wins (~38 LOC removed, mechanical moves). Skipping them leaves the optimisation directive unaddressed and they'll resurface in Sprint 42 harness work anyway.

**Alternative if rejected**: T-1/T-2/T-3/T-4 only (coverage-only) — defer F3/F4 structural moves. Cleaner per-sprint scope but pushes "一次做到好" to next sprint.

### Q2 — dev2 T-4 narrowing (drop tui_events key mapping)

**Recommendation**: confirm narrowing. Key mapping pinning has churn risk if mappings change deliberately (every key remap forces test edit). Sprint 42 TUI test harness sprint is the correct place for table-driven key-mapping coverage with ratatui-test or similar.

**Alternative if rejected**: include +15 LOC tui_events key-mapping tests now — accept churn risk.

### Q3 — F1/F2/F5 deferral

**Recommendation**: confirm deferral.
- F1 (drain in render) — pragmatic trade-off, KEEP indefinitely
- F2 (scratch_shell drain+resize) — MEDIUM-HIGH risk, only revisit if causes incident
- F5 (telegram_hooks event-driven) — Sprint 43+ event-layer sprint candidate

### Q4 — Sprint 42 boundary

**Recommendation**: Sprint 41 explicitly stays at coverage uplift on existing `TestBackend` pattern + 2 mechanical moves. **No insta / ratatui-test / harness redesign in Sprint 41.** Sprint 42 (claude-a1f200 `plan/tui-test-harness`) handles harness design with its own 4-perspective challenge (per `t-20260430055147257001-16`).

### Q5 — PR ordering

**Recommendation** (KISS-tight):
1. T-5 (F3 TelegramStatus move) — pure mechanical, fastest signal, smallest blast radius
2. T-6 (F4 resize_panes extraction) — mechanical extraction, low risk
3. T-3 (render hit-test + mouse drag) — bounded coordinate math
4. T-4 (overlay search + pane_factory config) — bounded logic
5. T-2 (commands + dispatch) — string-parsing assertions, slightly more churn-sensitive
6. T-1 (session.rs round-trip) — MEDIUM risk, last so prior tests stabilise the surface

T-1..T-6 are **all parallel-safe** (independent files), but ordering affects review confidence — small mechanical first, MEDIUM risk last.

### Q6 — Coverage target (realistic, not vanity)

Current Group 2 weighted (across 14 files): ~36 % (10,437 LOC, ~3,750 covered). After T-1..T-4: estimated **~45-50 % weighted**. render.rs glue holds at 40 % per logic-vs-glue gate; pure-glue files stay at 0 %. **Don't target 70 %+ vanity number** — operator brief explicitly accepts low cov of UI glue.

### Q7 — Per-PR tier classification

**Recommendation**: All T-1..T-6 are **Tier-1 single primary reviewer**. None touch daemon-routing equivalent boundaries — TUI/App layer is downstream of `dispatch.rs` boundary. Sprint 41 Tier-2 borrow not anticipated.

### Q8 — Sprint number

**Recommendation**: Sprint 41 confirmed (general m-297 pre-confirmed; no clash with Sprint 38/39/42).

### Q9 — Cross-sprint coordination

- Sprint 38 (async-trait removal, dev team) — no TUI scope overlap; **no conflict**
- Sprint 39 (CI providers GitLab/Bitbucket, dev team) — touches `ci_watch.rs`; **no TUI scope overlap**
- Sprint 42 (TUI test harness, claude-a1f200 PLAN-first) — adjacent scope; **boundary explicit**: Sprint 41 = logic-pin coverage with existing TestBackend + 2 mechanical moves; Sprint 42 = harness redesign / snapshot tooling evaluation. Both can run in parallel.

---

## 6. Acceptance criteria

For each PR in T-1 .. T-6:

- §3.5.10 fixture present where applicable (T-1 explicitly persistence-replay; others assertion-based on existing helpers)
- §3.5.11 RED→GREEN if feature/fix; pure mechanical move (T-5/T-6) takes byte-equivalence exemption per §3.5.11 #2
- §3.5.13 verdict mirrored to GH PR before self-merge
- All Tier-1 single primary; reviewer2 (or substitute) reviews
- §3.6.9 cleanup pair on merge (worktree + branch removal)
- Each PR's `kind=update` push notification carries scope-conformance statement per §3.6.1 amendment #1 (PR #343)
- Logic-vs-glue gate enforced: every test addition must clear the "tests real logic, not ratatui glue" bar; assertions on logic outcomes (return values, state transitions, parsing results), NOT on exact string formatting / table widget layout / colour codes

Cumulative criteria after all 6 PRs land:
- Group 2 weighted coverage ≥ 45 % (realistic target per Q6)
- F3 (TelegramStatus) + F4 (resize_panes) relocated out of render.rs; render.rs becomes pure rendering module
- 4 logic hotspots covered: session save/restore, command parsing, hit-test math, drag state
- Net LOC ≈ +137 (mostly tests + 38 LOC mechanical removal from render.rs)
- Sprint 42 harness work unblocked (no test-pattern conflicts introduced)

---

## 7. Process notes

- **Worktree**: `/Users/suzuke/.agend-terminal/workspace/lead2/repo` on `plan/sprint41-tui-app-optimisation` off `0498a13`
- **Substitute coverage**: reviewer2 (codex) hit usage-limit during Sprint 41 PRIOR-ART phase. Operator authorised kiro-cli substitute. dev2-1 (kiro-cli) delivered PRIOR-ART m-343. reviewer2 expected back online for IMPL review phase; if not, substitute extends.
- **Dispatches**:
  - dev2 STRUCTURAL — dispatched 2026-04-30T05:21Z, reported 05:24Z (m-304)
  - dev2-1 PRIOR-ART — dispatched 2026-04-30T06:00Z (post reviewer2 usage-limit), reported 06:08Z (m-343)
  - lead2 KISS dissent — folded into §4 above
- **PR path** (this PLAN PR): §3.5.5-extended LOW docs-only single-reviewer self-merge (operator-authorised; same Sprint 33/34/37/40 pattern).
- **Amendment #1 dogfood**: orchestrator pre-dispatch verification applies on every IMPL push (T-1 ... T-6) per PR #343 amendment.
- **No cross-team auth needed**: all Sprint 41 PRs Tier-1 single primary.

---

## 8. Self-awareness

dev2's STRUCTURAL recurrence pattern (Sprint 40 + Sprint 41: 0 paranoia + 0 boundaries) is itself a sprint-process finding. The 4-perspective protocol exists precisely to catch this — without dev2-1's PRIOR-ART surfacing F3+F4, Sprint 41 would have shipped as coverage-only and the structural cleanup would have re-emerged in Sprint 42 harness work. The synthesis here folds both lanes back so we don't ship a coverage-only sprint and then need a separate "actually optimise TUI render boundary" sprint right after.

The **logic-vs-glue gate** is dev2's genuine contribution this sprint — it makes the "ROI debatable per ratatui glue" caveat operational rather than aspirational. NO_CHANGE on glue files (`api_server.rs`, `telegram_hooks.rs`, `tui.rs`, `app/mod.rs`) is a documented, defended decision rather than a coverage gap.

If operator answers Q1 "narrow only" (T-1..T-4 coverage-only, defer T-5/T-6), this plan flags that subsequent Sprint 42 harness work or Sprint 43 event-layer work should pick up F3/F4 directly — they are mechanical and the longer they sit, the more downstream code accumulates the misplacement.
