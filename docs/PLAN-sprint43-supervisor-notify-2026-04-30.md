# PLAN: Sprint 43 — Supervisor member-state-change notify

**Date:** 2026-04-30
**Status:** plan-first; awaiting operator GO before any IMPL dispatch
**Branch:** `docs/sprint43-supervisor-notify-plan`
**Origin:** general m-20260430060731311284-342 (Sprint order 41→43→42); source backlog `docs/BACKLOG-supervisor-notify-policy-2026-04-30.md` (general-authored, operator-approved Option B + 8-state decision tree)
**Process:** 4-perspective challenge round (Sprint 32~42 model)
**Scope decision:** project decision `d-20260430061547546788-7`

---

## 0. KISS gate (§0)

- **What real problem does this solve?** Concrete operator incident: codex hit usage_limit lockout at HEAD `d8b1c50` post Sprint 39 PR-1 r5 verdict (~9h inactive); lead (orchestrator) had no programmatic signal to react. Operator manually intervened. Without notify infrastructure, every error-state stall requires human surveillance of `list_instances` output.
- **Would deletion break anyone?** No external user blocked; orchestrators continue to run blind on member state transitions. But operator availability becomes the bottleneck for fleet recovery — non-scalable.
- **Operator philosophy alignment**: 一勞永逸 favors infrastructure that lets orchestrators react autonomously, with operator escalation only when LLM decision tree exhausts.

---

## 1. Verified current state (lead minimal-delta)

### Backlog inventory (138 LOC at `docs/BACKLOG-supervisor-notify-policy-2026-04-30.md`)
- §1 problem statement (codex-lockout incident)
- §2 Option B daemon supervisor notify orchestrator (trigger / payload / routing)
- §3 8-state orchestrator response decision tree
- §4 helper functions (optional PR-3)
- §5 PR breakdown proposal (PR-1 daemon hook ~70 LOC + PR-2 protocol amendment ~60 LOC + PR-3 helpers optional ~50 LOC)
- §7 §13 candidate questions

### Existing daemon infrastructure relevant to hook
- `src/daemon/supervisor.rs:47` `tick()` 10s scan loop; per-agent `core.state.tick()` + `check_awaiting_operator`
- `src/state.rs:27` `AgentState` enum with **16 variants** including (per dev S1):
  - 8 backlog-listed error states: `Hang` / `AwaitingOperator` / `PermissionPrompt` / `ContextFull` / `RateLimit` / `UsageLimit` / `AuthError` / `Crashed`
  - **3 backlog-missing error states (per reviewer-kiro P2)**: `ApiError` / `Restarting` / `InteractivePrompt`
- `src/teams.rs:62` `find_team_for(home, &name)` for orchestrator lookup
- `src/inbox.rs::enqueue` for single-receiver delivery (existing `send` infrastructure)

---

## 2. Three perspectives (challenge round summary)

### 2.1 lead — minimal-delta synthesis
Both perspectives confirm the implementation is straightforward (existing 8-state classifier + existing inbox infrastructure). The cross-vantage value is **architectural discipline** — reviewer-kiro's P1 "anti-broadcast safeguard" + P3 "defer helpers" + P5 "debounce + self-notify" findings prevent infrastructure bloat that would echo Sprint 35 fleet_broadcast over-engineering. Sprint 43 ships notify infrastructure as Phase 1; helpers and broader observability deferred until concrete pain materializes.

### 2.2 dev (kiro) STRUCTURAL (m-20260430061853168148-354)

**S1 detection hook surface**: single hook in `supervisor.rs:47 tick()` loop after `core.state.tick()`. Save prev_state, read new_state, match if error-class. ~15 LOC + ~20 LOC for `is_error_class()` method on `AgentState`.

**S2 payload schema**: 6 of 8 fields already tracked (member name + team + from/to_state + timestamp + last_pane_excerpt via existing `vterm.tail_lines`). 2 NEW fields: `unlock_at` parse from pane (~20 LOC regex; usage_limit-specific) + `consecutive_count` (~5 LOC HealthTracker field).

**S3 routing**: `teams::find_team_for` + `inbox::enqueue` — single-receiver via existing infrastructure; NOT broadcast plumbing (Sprint 35 lesson preserved). 4 edge cases handled (no team / no orchestrator / orchestrator-deleted / team-changed) ~25 LOC.

**S4 8-state decision tree**: all 8 backlog states directly matchable on `AgentState` enum variants (zero new classifier needed). ~10 LOC match block. **Note**: dev's claim "all 8 states are directly matchable" is technically correct for the backlog's 8 but enum has 11 error-class variants total (per reviewer-kiro P2 — see §3.3).

**S5 helpers**: free functions in new `src/orchestrator_helpers.rs` OR `src/instructions.rs` protocol text; 0 trait sig changes; ~50 LOC for 3 functions (handle_member_state_change / spawn_or_wait_or_escalate / escalate_operator).

**Total**: PR-1 ~70 LOC + PR-2 ~60 LOC docs + PR-3 ~50 LOC optional helpers. Backlog §5 estimates accurate.

### 2.3 reviewer-kiro PRIOR-ART / CROSS-VANTAGE (m-20260430061857352723-355)

**P1 Sprint 35 KISS-removal cross-check (PRIORITY)**:
- Single-receiver vs broadcast structurally different ✓ (1:1 not 1:N)
- **HOWEVER**: backlog §2.3 "general fleet observer" clause re-introduces broadcast risk. **Drop the clause** — Sprint 35 fleet_broadcast had 0 programmatic consumers; adding general as second receiver echoes the killed pattern.
- **GO-NARROW recommendation**: ship 6 actionable states (usage_limit + rate_limit + hang + auth_error + permission_prompt + awaiting_operator); defer crash + context_full (existing mitigations: auto-respawn, kiro self-healing compaction).

**P2 8-state completeness gap**:
- Backlog missing 3 enum states: **ApiError / Restarting / InteractivePrompt**
- Naming inconsistency: backlog "crash" vs enum `Crashed`
- Subsumable: crash + restarting → "daemon-handled monitor only"; context_full → "self-healing"
- Edge case: orchestrator-self state-change → skip self-notify
- Edge case: operator-direct managed instances → no team = no notify (correct by construction)

**P3 §4 helpers — DEFER**:
- Same "theoretical value" smell that killed fleet_broadcast Sprint 35
- Orchestrators are LLMs; can implement decision tree from protocol doc text
- Existing MCP tools (interrupt / replace_instance / reply) suffice
- PR-3 already marked optional in backlog — take the hint

**P4 single-receiver invariant fixture (anti-broadcast safeguard)**:
- Sprint 37 PR #340 team-isolation gate pattern applied to notify routing
- 2 teams × 2 agents fixture: worker-a → usage_limit, assert orch-a inbox=1, all others=0
- Inject hook: extend existing `NoticeAction` enum (don't create new delivery channel)

**P5 5 adversarial scenarios**:
- Scenario 1 BLOCKING: **notify storm (state-flapping)** — debounce ~10 LOC (track last-notify-time per agent, 60s cooldown)
- Scenario 2 NIT: orchestrator absent → log warning ~5 LOC
- Scenario 3 LOW: classifier already mature
- Scenario 4 BLOCKING: **recursive notify** — skip when `member == orchestrator` ~3 LOC
- Scenario 5 LOW: 10s tick latency acceptable

---

## 3. BLOCKING items for IMPL plan (per reviewer-kiro)

These four items must land in PR-1 (or backlog revision) before Sprint 43 wave can claim feature-complete:

### 3.1 BLOCKING — Drop "general fleet observer" clause (per P1)
**Rationale**: backlog §2.3 ambiguous "optionally forward to general" re-introduces broadcast risk. Sprint 35 fleet_broadcast killed because 0 programmatic consumers; adding general as second receiver echoes the killed pattern.

**Required action**: backlog revision (edit §2.3) OR PLAN explicit statement that "general forwarding" is OUT OF SCOPE for Sprint 43. If general genuinely needs fleet-wide observability, that's a separate sprint with its own KISS justification.

### 3.2 BLOCKING — Debounce notify (per P5 Scenario 1)
**Rationale**: state-flapping agent (rate_limit ↔ ready every 10s) floods orchestrator inbox; orchestrator LLM wastes tokens on noise.

**Required impl**: track last-notify-time per agent in supervisor; suppress duplicate state-change events within 60s cooldown window. ~10 LOC in supervisor.rs.

### 3.3 BLOCKING — Skip self-notify (per P5 Scenario 4)
**Rationale**: orchestrator hits error state → supervisor tries to notify orchestrator about itself → no-op or infinite loop.

**Required impl**: skip notify when `member == team.orchestrator`. ~3 LOC in supervisor.rs hook. Operator escalation via existing Telegram `gated_notify` path naturally handles orchestrator-itself failures.

### 3.4 BLOCKING — Single-receiver invariant test fixture (per P4)
**Rationale**: Sprint 37 PR #340 team-isolation gate enforced via §3.5.10 fixture. Sprint 43 notify routing needs equivalent enforcement.

**Required test fixture**: 2 teams × 2 agents production-path-coupled fixture; worker-a→usage_limit transition; assert orch-a inbox=1 event, orch-b/worker-a/worker-b/general inboxes=0 events. ~30 LOC.

---

## 4. NIT / RECOMMEND for §13 operator awareness

### 4.1 GO-NARROW 6 states (per reviewer-kiro P1)
Ship 6 actionable: usage_limit, rate_limit, hang, auth_error, permission_prompt, awaiting_operator. Defer crash + context_full (existing mitigations make orchestrator response value-additive only for loop-detection / self-healing-failure cases).

**Operator decision**: 8 (full) OR 6 (GO-NARROW)?

### 4.2 3 missing enum error states (per reviewer-kiro P2)
Backlog incomplete on `AgentState` enum coverage:
- `ApiError` (priority 13) — generic API failures, similar response to rate_limit
- `Restarting` (priority 15) — transient daemon respawn state, monitor-only
- `InteractivePrompt` (priority 7) — already supervisor-handled via `take_interactive_prompt_notice`; similar to permission_prompt

**Operator decision**: include in Sprint 43 scope, defer, OR explicitly out-of-scope?

Naming inconsistency `crash` (backlog) vs `Crashed` (enum) — minor, non-blocking.

### 4.3 Defer §4 helper functions (per reviewer-kiro P3)
Helpers have same "theoretical value" smell as Sprint 35 fleet_broadcast (no concrete consumer). Orchestrators are LLMs; existing MCP tools (interrupt / replace_instance / reply) suffice. Backlog §5 already marks PR-3 optional.

**Operator decision**: defer (drop PR-3) OR keep optional?

### 4.4 Orchestrator-absent log warning (per reviewer-kiro P5 Scenario 2)
~5 LOC: when notify is suppressed due to missing orchestrator, log warn at supervisor for operator visibility.

---

## 5. PR sequencing (refined per backlog §5 + perspectives)

| PR | Scope | LOC | Tier | Cross-vantage required? |
|---|---|---|---|---|
| **PR-1** | Daemon hook + routing + 4 BLOCKING items (§3.1 if scope-narrow / §3.2 debounce / §3.3 skip-self / §3.4 fixture) | ~95-100 (~70 baseline + ~13 BLOCKING + ~30 fixture LOC) | **Tier-2 dual-reviewer** (daemon-side + single-receiver invariant verification) | YES per backlog §5 |
| **PR-2** | Protocol amendment docs (decision tree + cross-references) | ~60 | Tier-1 | No (docs-only §3.5.5) |
| **PR-3** | Helper functions (per §4.3 — recommend DEFER) | ~50 | Tier-1 | No |

**Total ~155-210 LOC (PR-3 deferred = ~155).** Strict serial — PR-1 → PR-2 (PR-3 if approved).

---

## 6. §3.5.10 / §3.5.11 application per PR

### PR-1
- §3.5.10 wire-format: notify event payload spec-quoted; production-path-coupled via real supervisor tick → real inbox.enqueue
- §3.5.10 single-receiver invariant fixture (per §3.4) — 2-team × 2-agent production-path coupled
- §3.5.11 test-first per detection hook + routing rule + debounce + self-skip
- §3.5.13 form claim-accuracy mandate per §11 (Sprint 39 lessons)

### PR-2
- Standard docs-only §3.5.5 LOW exception

### PR-3 (if approved)
- Standard test-first per helper function

---

## 7. Risks (Sprint 43-specific, beyond per-perspective)

| Risk | Mitigation |
|---|---|
| §3.1 "general fleet observer" creep at IMPL time | PLAN explicit OUT OF SCOPE statement; PR-1 review checks no general-forwarding code |
| §3.2 debounce window misjudgment (60s too long → real escalation delayed; 60s too short → storm) | Make configurable via fleet.yaml or const that's tunable in PR follow-up |
| §3.3 self-notify edge case forgotten | Test in §3.4 fixture (orch-a hits state → asserts no self-notify) |
| Sprint 41 closure delays Sprint 43 | Sprint 43 daemon-side; Sprint 41 TUI/App-side; minimal file overlap, can run parallel |
| Sprint 39 retrospective false-claim pattern recurs | TIGHTENED §3.5.13 grep-evidence per claim mandated for Sprint 43 PR-1 dispatches |

---

## 8. Out of scope

- General fleet observer / broadcast revival (per §3.1)
- §4 helper functions if §4.3 recommendation accepted
- TUI test harness (Sprint 42 IMPL DEFERRED until Sprint 43 closeout)
- async-trait removal (Sprint 38 deferred-permanent until Rust dyn-async stable)
- CiHttpClient extraction (Sprint 39 follow-up `t-20260430024226176283-9`; bundle decision deferred to Sprint 43 IMPL dispatch time per general m-20260430060731311284-342)

---

## 9. Open questions for operator (§13)

1. **GO-NARROW (6 states) OR full 8 states** per backlog §3 (per reviewer-kiro P1)
2. **3 missing enum states** (ApiError / Restarting / InteractivePrompt) — include in Sprint 43, defer, OR out-of-scope?
3. **§4 helper functions PR-3** — DEFER (recommend) OR keep optional?
4. **§3.1 "general fleet observer" clause** — explicit OUT OF SCOPE confirm (recommend) OR keep ambiguous for future sprint?
5. **Sprint 39 CiHttpClient follow-up** bundling decision — bundle with Sprint 43 IMPL OR independent nit-PR?
6. **Tier classification per PR** — PR-1 Tier-2 dual confirmed (per backlog §5 + reviewer-kiro P1)?
7. **Cross-vantage reviewer for PR-1** — reviewer (codex, expected unlock ~3:14 PM) OR reviewer-kiro continues fill-in OR cross-team reviewer2 borrow per Sprint 33/35/37/40 pattern?
8. **Sprint number 43** confirmed (per general)?
9. **Debounce cooldown** value (60s recommended; configurable?)
10. **IMPL dispatch ownership** — dev (kiro) OR rotate dev2 in for any phase?

---

## 10. Cross-references

- general m-20260430060731311284-342 (operator scope + Sprint order 41→43→42)
- general m-20260430055323598776-331 (reviewer-kiro spawn for codex usage-limit lockout)
- decision `d-20260430061547546788-7` (Sprint 43 plan-first)
- master task `t-20260430061551332234-17`
- dev S1-S5 perspective: m-20260430061853168148-354
- reviewer-kiro P1-P5 perspective: m-20260430061857352723-355
- source backlog: `docs/BACKLOG-supervisor-notify-policy-2026-04-30.md`
- Sprint 35 fleet_broadcast removal: PR #333 5ece359 (KISS-removal precedent)
- Sprint 37 PR #340 team-isolation gate (single-receiver invariant precedent)
- Sprint 38 PLAN-first 4-perspective (closed-world dissent precedent)
- Sprint 42 review-extend doc: `docs/PLAN-sprint42-tui-test-harness-review-2026-04-30.md`
- `src/state.rs:27` AgentState enum (16 variants)
- `src/daemon/supervisor.rs:47` tick() loop
- `src/teams.rs:62` find_team_for
- `src/inbox.rs::enqueue` single-receiver delivery
- `docs/FLEET-DEV-PROTOCOL-v1.md` §0 / §3.5.10 / §3.5.11 / §3.5.13 / §10.1
