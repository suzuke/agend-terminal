# PLAN: Sprint 40 — Group 1 MCP Layer optimisation + coverage uplift

**Date**: 2026-04-30
**Basis**: main HEAD `1a04c26` (post Sprint 38 PLAN merge)
**Operator brief**: general m-122 at 2026-04-30T02:18Z — first per-group impl following architecture audit (PR #346 `docs/ARCHITECTURE-GROUPS.md`).
**Team**: lead2 (orchestrator, MINIMAL-DELTA + cost-benefit) · dev2 / kiro-cli (STRUCTURAL) · reviewer2 / codex (PRIOR-ART, two-pass)
**Status**: ready for operator §13 answers

---

## 0. Scope (frozen by operator)

Per general m-122:
- **In-scope**: cleanup of Group 1 MCP Layer files. Two angles **AND** required:
  1. **Optimisation** — dead code / over-abstraction / duplication / paranoia leftovers (per Sprint 28 RBAC + Sprint 35 fleet-update broadcast deletion philosophy)
  2. **Coverage uplift** — low-cov functions get tests OR redesign decision per §3.5.10 fixture pattern
- **Operator philosophy**: "一次做到好不要半吊子" — every fix must answer "PERMANENT vs TRADE-OFF"; bias toward PERMANENT. TRADE-OFF needs explicit "what would PERMANENT look like" follow-up.
- **Out-of-scope**:
  - Other groups (Group 2 TUI/App etc. — separate sprints)
  - New MCP tool development (Sprint 40 is cleanup-only)
  - Core abstraction redesign (unless prior-art proves "一次做到好" demands it)

The 4-perspective challenge round answers **how minimal · prior art · cost-benefit boundaries**, NOT whether-to.

---

## 1. Coverage baseline (real measurement, dev2-reported)

`cargo llvm-cov` at HEAD `1a04c26`:

| File | LOC | Cov % | Inline tests |
|---|---|---|---|
| `src/mcp/handlers/channel.rs` | 39 | **10.3 %** | 0 |
| `src/mcp/handlers/schedule.rs` | 21 | **28.6 %** | 0 |
| `src/mcp/handlers/ci.rs` | 105 | **32.4 %** | 0 |
| `src/mcp/handlers/instance.rs` | 564 | **47.5 %** | 0 (in `tests.rs`: 67) |
| `src/mcp/handlers/task.rs` | 58 | 62.1 % | 0 |
| `src/bin/agend-mcp-bridge.rs` | 228 | 73.7 % | 0 |
| `src/mcp/handlers/comms.rs` | 586 | 86.9 % | 5 |
| `src/mcp/mod.rs` | 235 | 90.2 % | 7 |
| `src/mcp_config.rs` | 688 | 94.9 % | 34 |
| `src/inbox.rs` | 1691 | 95.0 % | 65 |
| `src/mcp/tools.rs` | 326 | 98.2 % | 1 (count invariant) |
| `src/mcp/handlers/mod.rs` | 90 | 98.9 % | 0 |

**Group 1 weighted average**: ~75 % (dev2's per-file numbers aggregate; close to overall main cov of 73.1 % from PR #346).

**Real low-cov hotspots** (worth coverage uplift OR redesign decision): `channel.rs` (10.3%), `schedule.rs` (28.6%), `ci.rs` (32.4%), `instance.rs` (47.5%).

---

## 2. STRUCTURAL findings (dev2)

dev2 produced per-file analysis with **PERMANENT / TRADE-OFF** labelling. Summary:

### 2.1 Per-file proposals (dev2)

| File | Proposal | LOC | Risk | Type |
|---|---|---|---|---|
| `channel.rs` | +3-4 error-path tests (no active channel, missing file_id) | +20 | LOW | PERMANENT |
| `ci.rs` | +3-4 tests for `handle_checkout_repo` path resolution + JSON shape | +30 | LOW | PERMANENT |
| `schedule.rs` | **No change** — thin pass-throughs to `schedules::*` / `deployments::*`; already covered upstream | 0 | N/A | PERMANENT |
| `task.rs` | +2 tests for create/update_team API-fallback path | +15 | LOW | PERMANENT |
| `instance.rs` | +4-5 tests targeting team-mode create / start-resume / describe shape | +40 | MEDIUM | PERMANENT (error paths) / **TRADE-OFF** (team-mode internals) |
| `comms.rs` | +1-2 tests for broadcast tag-filtering | +15 | LOW | PERMANENT |
| `mod.rs` / `tools.rs` / `mcp_config.rs` / `inbox.rs` / `mcp/mod.rs` | No change | 0 | N/A | PERMANENT |
| `agend-mcp-bridge.rs` | +1-2 tests for proxy error paths | +15 | LOW | PERMANENT |

**Cross-file proposals** (dev2):
- **PR-A**: handler test coverage batch (channel + ci + task + broadcast-tags) — +80 LOC
- **PR-B**: `instance.rs` test coverage — +40 LOC
- **PR-C**: bridge proxy error tests — +15 LOC
- **PR-D (optional)**: merge `schedule.rs` into `task.rs` — net −5 LOC (reduce file count)

### 2.2 Critical gap in dev2 STRUCTURAL

dev2's analysis is overwhelmingly **coverage-uplift focused** (4 PRs all about adding tests except PR-D). **dev2 found 0 paranoia leftovers, 0 boundary leaks, 0 over-abstraction beyond "thin shim".** That mismatches operator's BOTH-AND brief (optimisation AND coverage). reviewer2 PRIOR-ART filled the gap (§3 below).

---

## 3. PRIOR-ART findings (reviewer2 — pass 1 + pass 2)

### 3.1 Recent MCP fix-shape templates (apply as Sprint 40 IMPL anchors)

| # | Template | Source | Apply to |
|---|---|---|---|
| T1 | Single normalisation chokepoint pattern (centralise field-lifting in one adapter, regression test all variants) | PR #323 `lift_message` | T1-style invariants for any new test additions |
| T2 | Vertical-slice deletion (full chain: tool def + dispatch + handler + API const + tests + dead config field) | PR #324 tool_kill removal | If we delete anything in Sprint 40, follow this shape |
| T3 | Adversarial shape tests first (terminal/grid/buffer state needs adversarial cases not just happy path) | PR #334 `pane_snapshot` trim-before-window | PR-A test additions for handlers with terminal/buffer state |
| T4 | Source-precedence + fallback warning (multi-store reads need explicit precedence) | PR #325 topic routing | Apply to T7 fallback policy centralisation |
| T5 | Boundary extraction before behaviour change (move side-effects out of transport handlers) | reviewer2's own findings B1+B2 | Direct template for T4-T5 below |

### 3.2 Paranoia / defensive-code leftover candidates

reviewer2 ran §3.5.12 (d) counter-example analysis on candidates:

| ID | Location | Verdict | Action |
|---|---|---|---|
| **P1** | `src/mcp/mod.rs:14-79` MCP tool ACL env policy (allow/deny lists) | **escalate** — counter-example exists for shared-shell / per-instance subsets, but redundant in strict single-user model | **§13 Q2 — operator decides** |
| **P2** | `src/channel/auth.rs:33-57` warn-once dedup `Mutex<HashSet>` | **KEEP** — strong counter-example for deletion (observability protection, lightweight) | no change |
| **P3** | `src/mcp/handlers/instance.rs:504-519` auto-dedup spawn names | **KEEP with optional `strict_name=true` flag** — policy ambiguity not paranoia | follow-up Sprint, not 40 |
| **P4** | `src/mcp/handlers/comms.rs:380-401` delegate_task branch checkout side-effect | **strong move candidate** — comms transport mutating worktree is surprising | resolved by **T5** below |

### 3.3 Boundary leaks (MCP ↔ channel ↔ api)

reviewer2's primary "permanent" findings missed by dev2:

| ID | Location | Leak | Suggested action | Risk in cleanup sprint |
|---|---|---|---|---|
| **B1** | `comms.rs:348-367` MCP calls `channel::active_channel().send_from_agent(InjectProvenance)` directly | channel-specific operation in MCP layer | push provenance to API SEND service boundary | **MEDIUM** (touches dispatch semantics) |
| **B2** | `comms.rs:380-401` worktree checkout side-effect in transport handler | infrastructure side-effect in comms transport | move to instance/worktree orchestration layer | **MEDIUM-HIGH** (behavioural coupling) |
| **B3** | `api/handlers/mcp_proxy.rs:46-75` API directly calls `crate::mcp::handlers::handle_tool` | layer crossing | introduce `ToolExecutor` service boundary; API owns timeout, MCP pure execution | **LOW-MEDIUM** (pure extraction if no behaviour change) |
| **B4** | `comms.rs:88-121` daemon-unavailable fallback policy duplicated per tool path | resilience policy in MCP tool layer | centralise fallback in API/service | **MEDIUM** (preserve current offline behaviour) |

### 3.4 KISS dissent on dev2 proposals (reviewer2 pass 2)

| dev2 proposal | reviewer2 KISS verdict | Rationale |
|---|---|---|
| **PR-A** | KEEP as-is, PERMANENT | wire/dispatch behaviour pins, protects against PR #323 regression class |
| **PR-B** | **SPLIT** | Keep error-path + black-box invariants (PERMANENT); defer team-mode internal-choreography pinning (TRADE-OFF). Black-box invariants survive future redesign. |
| **PR-C** | KEEP as-is, PERMANENT | bridge reliability/error semantics durable contract surface |
| **PR-D** | **DEFER** to Sprint 41 | low immediate payoff; only do if readability drag remains after A/C/B |

reviewer2 also added: `schedule.rs` "no test" recommendation should include **1-2 route-level tests** that dispatch reaches the intended downstream — minimal integration pin, NOT broad unit shim coverage.

---

## 4. MINIMAL-DELTA synthesis (lead2)

### 4.1 Reconcile dev2 + reviewer2

dev2 proposes a **coverage-uplift sprint**; reviewer2 surfaces **structural cleanup opportunities** dev2 missed. Operator's BOTH-AND directive means **Sprint 40 must include both lanes**, NOT just dev2's narrower view.

The PR backlog therefore expands from dev2's 4 proposals to a merged set:

### 4.2 Proposed Sprint 40 PR backlog (merged)

| # | PR | Lane | LOC | Risk | Tier | Type | Sources |
|---|---|---|---|---|---|---|---|
| **T-1** | Handler test coverage batch (channel + ci + task-fallback + comms-tags + 1-2 schedule route-level pins) | coverage | +85 | LOW | Tier-1 | PERMANENT | dev2 PR-A + reviewer2 schedule pin |
| **T-2** | Bridge proxy error path tests | coverage | +15 | LOW | Tier-1 | PERMANENT | dev2 PR-C |
| **T-3** | `instance.rs` error-path + shape invariants ONLY (no team-mode choreography pinning) | coverage | +20 | LOW | Tier-1 | PERMANENT | dev2 PR-B narrowed by reviewer2 KISS |
| **T-4** | **B3 ToolExecutor service boundary** — API owns timeout, MCP pure execution | structural | +30 / -20 | LOW-MEDIUM | Tier-1 | PERMANENT | reviewer2 B3 |
| **T-5** | **B1 InjectProvenance push to API service** — comms.rs no longer calls `channel::active_channel()` directly | structural | +25 / -15 | MEDIUM | Tier-2 dual | PERMANENT | reviewer2 B1 |
| **T-6** | **B2+P4 worktree checkout move out of comms transport** — relocate `delegate_task` branch-checkout side-effect to instance/worktree orchestration layer | structural | +40 / -25 | MEDIUM-HIGH | Tier-2 dual | PERMANENT | reviewer2 B2 + P4 |
| **T-7** | **B4 fallback policy centralisation** — daemon-unavailable fallback in API/service, not per-tool MCP path | structural | +30 / -40 | MEDIUM | Tier-2 dual | PERMANENT | reviewer2 B4 + T4 prior-art |

**Estimated total**: +245 / -100 ≈ **net +145 LOC** (mostly tests + extractions; net positive but boundary-extraction PRs have negative impl LOC offset by positive test LOC).

### 4.3 Explicitly NOT in Sprint 40 backlog

- **dev2 PR-D** (schedule→task merge) — defer to Sprint 41 per reviewer2 KISS verdict; low immediate payoff
- **dev2 PR-B team-mode choreography pinning** — TRADE-OFF subset; defer until team-mode redesign sprint (per reviewer2 "permanent alternative" advice — black-box invariants in T-3 survive any future redesign)
- **reviewer2 P3** (auto-dedup spawn `strict_name` flag) — separate follow-up sprint; policy ambiguity, not paranoia
- **reviewer2 P1** (MCP tool ACL env policy) — **§13 Q2 awaits operator decision**

### 4.4 PERMANENT vs TRADE-OFF labelling (per operator philosophy)

All 7 backlog items are **PERMANENT**:
- T-1/T-2/T-3 are coverage pins on stable wire-format / dispatch behaviour — survive any subsequent refactor
- T-4/T-5/T-6/T-7 are boundary extractions — once layer responsibilities are corrected, they don't drift back

**No TRADE-OFF items in Sprint 40 backlog.** Items dev2 originally labelled TRADE-OFF (team-mode internals) are explicitly excluded; their PERMANENT alternative (T-3 black-box invariants) replaces them.

---

## 5. §13 decisions surfaced for operator

### Q1 — Sprint 40 scope expansion

**Recommendation**: **Accept the expanded backlog (T-1 .. T-7)** rather than dev2's narrower coverage-only proposals. Rationale: operator's brief explicitly lists optimisation AND coverage as required outcomes. dev2's STRUCTURAL focused exclusively on coverage; reviewer2's PRIOR-ART surfaced the optimisation lane (boundary leaks B1-B4 + paranoia P4). Skipping T-4/T-5/T-6/T-7 leaves the optimisation directive unaddressed.

**Alternative if rejected**: only T-1/T-2/T-3 (coverage-only) — defer all boundary extractions to Sprint 41+. Cleaner per-sprint scope but pushes "一次做到好" to next sprint.

### Q2 — P1 MCP tool ACL env policy (`src/mcp/mod.rs:14-79`)

reviewer2's counter-example analysis: **inconclusive**. Real value if operator runs shared shell or wants per-instance tool subsets; redundant under strict single-user localhost. **Operator decision needed** — does the ACL env policy stay (active operational requirement), get demoted to a feature flag, or get removed (KISS)?

### Q3 — PR-B split per reviewer2 KISS verdict

**Recommendation**: confirm split. T-3 includes only error-path + black-box invariants (PERMANENT); team-mode internal choreography testing deferred until team-mode redesign sprint.

### Q4 — PR-D (schedule→task merge) deferral

**Recommendation**: confirm deferral to Sprint 41 per reviewer2.

### Q5 — Per-PR tier classification

Confirm:
- T-1, T-2, T-3, T-4 → **Tier-1** single primary reviewer
- T-5, T-6, T-7 → **Tier-2 dual reviewer** (touch daemon-routing-equivalent boundaries — channel adapter coupling, transport-layer side-effects, fallback policy)

T-5/T-6/T-7 dual-reviewer dispatch carries the same Sprint 35 PR #333 / Sprint 37 PR #340 lineup pattern (reviewer2 PRIMARY + dev-team `reviewer` cross-vantage with operator-authorisation citation embedded in dispatch text).

### Q6 — Sprint number

**Recommendation**: Sprint 40 confirmed (general m-122 pre-confirmed; no clash with Sprint 38/39 dev team work).

### Q7 — PR ordering

**Recommendation** (reviewer2's KISS-tight order, extended for boundary PRs):
1. T-1 (handler test batch) — fastest signal, lowest risk
2. T-2 (bridge proxy tests) — low LOC / high leverage
3. T-3 (instance.rs error-path subset) — bounded by reviewer2 dissent
4. T-4 (B3 ToolExecutor) — pure extraction, no behaviour change
5. T-5 (B1 InjectProvenance) — Tier-2; extract before adding tests around it
6. T-6 (B2+P4 worktree checkout move) — Tier-2; highest behavioural coupling, do once T-3 invariants exist
7. T-7 (B4 fallback policy) — Tier-2; do last so prior extractions can simplify the consolidation

### Q8 — Coverage target (realistic, not vanity)

Current Group 1 weighted: ~75 %. After T-1+T-2+T-3 coverage PRs: estimated **~82-85 %** weighted (channel.rs ~50 %, ci.rs ~70 %, instance.rs ~60 %, others unchanged). Realistic — channel.rs cannot exceed ~60 % without a real bot, schedule.rs is structural shim. Don't target 90 %+ vanity number.

### Q9 — Cross-sprint coordination

- Sprint 38 (async-trait removal, dev team PLAN-first per PR #347) — does NOT touch `src/mcp/handlers/*` per scope; **no overlap**.
- Sprint 39 (GitLab+Bitbucket provider, dev team) — touches `ci_watch.rs` per general m-122 confirmation; **no MCP layer overlap**.
- All three sprints can run in parallel safely.

---

## 6. Acceptance criteria

For each PR in T-1 .. T-7:

- §3.5.10 fixture present (test-first or fixture-pinning per scope)
- §3.5.11 RED→GREEN if feature/fix; pure deletion / refactor exemption with byte-equivalent guarantee where applicable
- §3.5.13 verdict mirrored to GH PR before self-merge
- Tier-2 PRs (T-5/T-6/T-7) require dual-reviewer VERIFIED + CI green
- §3.6.9 cleanup pair on merge
- Each PR's `kind=update` push notification carries scope-conformance statement per §3.6.1 amendment #1 (PR #343)

Cumulative criteria after all 7 PRs land:
- Group 1 weighted coverage ≥ 82 % (realistic target per Q8)
- B1 / B2+P4 / B3 / B4 boundary leaks closed; comms.rs no longer holds channel-specific calls / worktree side-effects / fallback-policy duplication / direct API-shim references
- Net LOC ≈ +145 (mostly tests + extractions)

---

## 7. Process notes

- **Worktree**: `/Users/suzuke/.agend-terminal/workspace/lead2/repo` on `plan/sprint40-mcp-layer-optimisation` off `1a04c26`
- **Decision (PLAN scope freeze)**: `d-20260430021844853836-2`
- **Dispatches**:
  - dev2 STRUCTURAL — dispatched 2026-04-30T02:19Z, reported 02:22Z (~3 min wall)
  - reviewer2 PRIOR-ART pass 1 — dispatched 02:19Z, reported 02:21Z (~2 min)
  - reviewer2 PRIOR-ART pass 2 (KISS dissent on dev2) — reported 02:28Z (~6 min after pass 1)
- **PR path** (this PLAN PR): §3.5.5-extended LOW docs-only single-reviewer self-merge (operator-authorised; same Sprint 33 / 34 / 37 pattern).
- **Amendment #1 dogfood**: orchestrator pre-dispatch verification will apply on every IMPL push (T-1 ... T-7) per PR #343 amendment.
- **Cross-team auth pre-authorised** by operator for Sprint 40 Tier-2 dual reviewer borrow per general m-122 ("帶 operator authorization line 進 dispatch text 給 reviewer 看 — per Sprint 33 PR-3 / Sprint 37 PR-340 既有 pattern").

## 8. Self-awareness

dev2's STRUCTURAL coverage-only focus is itself an instance of the trap operator's "一次做到好" philosophy guards against — covering low-cov modules while leaving structural debt is "半吊子". reviewer2's PRIOR-ART catching the optimisation gap is exactly the value-add of the 4-perspective protocol; the synthesis here folds both back into a unified backlog so we don't ship a coverage-only sprint and then need a separate "actually optimise MCP layer" sprint right after.

If operator answers Q1 "narrow only" (coverage-only, defer T-4..T-7), this plan flags that subsequent Sprint 41+ should pick up T-4..T-7 directly to avoid re-discovering the same boundary leaks via incident.
