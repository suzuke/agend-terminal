# Sprint 55 P0-C — Auto-Bind Dispatch Trigger Scope Filter (FINAL)

**Status**: Phase 3 lead-synthesized design — pending operator review per m-3637 design-first directive
**Date**: 2026-05-08
**Origin**: SPLIT from P0-B EC10 per Phase 2 reviewer challenge
**Phase 1 evidence**: dev RCA P0-B EC10 + Phase 0 dogfood (this very design phase task)
**Phase 2 verdict**: reviewer recommended split (distinct semantic from P0-B's HOW-to-bind contract)
**Authors**: dev (RCA + dogfood evidence) + reviewer (split verdict + scope advice) + lead (synthesis)

## §1 Executive summary

**Problem (dogfood-derived)**: `dispatch_auto_bind_lease` (`src/mcp/handlers/dispatch_hook/mod.rs:21-83`) fires on EVERY task dispatch, creating a daemon-managed worktree at `<source_repo>/.worktrees/<agent>` regardless of task class. Read-only design/audit/RCA tasks (like this Sprint 55 P0 design phase task itself) get unnecessary worktrees auto-created — wasted resource + STRICT compliance friction (worktree nested under operator clone post Sprint 54 PR #519 + parallel-worktree convention requires manual `release_worktree` cleanup).

**Live evidence**: Sprint 55 P0 design RCA dispatch m-11 to dev (THIS task) is read-only + DOC-only writes. Yet `dispatch_auto_bind_lease` fired on receipt, created daemon worktree at `agend-terminal/.worktrees/dev`. Required Phase 0 `release_worktree` cleanup before dev could work in parallel sibling worktree. Wasted ~5 min of worktree-create + cleanup churn for zero IMPL benefit.

**Final design — Two variants for operator choice**:

### Variant A: Minimal opt-out (~30-50 LOC, recommended for Sprint 55)
- Add `bind: bool = true` field to `task` board entries + `delegate_task` send args
- `dispatch_auto_bind_lease` checks field; skips bind on `bind: false`
- Backward-compat: absence of field defaults to `true` (current behavior)
- Lead/general explicitly mark RCA/audit/design tasks `bind: false`

### Variant B: Task-class taxonomy (~80-150 LOC, deferred Sprint 56+)
- Add `class: "rca" | "audit" | "design" | "doc-only" | "impl" | "fix" | "feat"` field
- Dispatch hook gates auto-bind by class (impl/fix/feat → bind; others → skip)
- Backward-compat: absence defaults to `impl` class (current behavior)

**Lead recommendation**: Variant A for Sprint 55 (smaller blast radius + faster ship + dogfood-evidence already justifies it). Variant B reserved for Sprint 56+ if cross-cutting class taxonomy proves valuable for other features (test-only mode, observability, telemetry).

## §2 Premise + dogfood evidence

This Sprint 55 P0 design phase task is the dogfood evidence. Phase 0 friction:

```
1. Lead dispatches task to dev (m-11) → general "delegate_task" send → dispatch_auto_bind_lease fires
2. Daemon creates worktree at /Users/suzuke/Documents/Hack/agend-terminal/.worktrees/dev
3. Daemon writes binding.json + .agend-managed marker
4. Dev arrives in dispatched task; sees nested-under-operator-clone worktree
5. Dev recognizes STRICT physical-separation intent (operator m-3617) requires parallel sibling worktree
6. Dev calls `release_worktree(target_instance=dev)` — clears auto-bind (5 min round-trip)
7. Dev creates parallel worktree at ~/Documents/Hack/agend-terminal-worktrees/sprint54-p1b-bug2-fix-option-c-provisioning
8. Dev resumes Phase 0 worktree provisioning + Phase 1 RCA
```

**Wasted cycles**: Phase 0 `release_worktree` cleanup + new branch creation = ~5 min friction for ZERO IMPL benefit. Multiplied across N future RCA/design dispatches = N×5min cumulative loss.

**Why this isn't a P0-B core concern**: P0-B refactors arg-shape and derivation pipeline (HOW the binding happens once triggered). P0-C is about WHEN the binding triggers (task-class policy). Distinct architectural layer; reviewer Phase 2 verdict correctly identified the split.

## §3 Chosen design: Variant A (minimal `bind: bool` opt-out)

### Surface change

```rust
// task::create
task(action: "create", title: "...", branch: "...", bind: false)

// send (delegate_task)
send(target_instance: "dev", request_kind: "task", task_id: "...", bind: false)

// Dispatch hook
fn dispatch_auto_bind_lease(...) -> Option<...> {
    if !task_should_bind(task_record) {
        return None;  // skip auto-bind, return early
    }
    // existing bind logic
}

fn task_should_bind(task_record: &TaskRecord) -> bool {
    // Default true if field absent (backward-compat)
    task_record.bind.unwrap_or(true)
}
```

### Backward-compat

- Existing dispatches without `bind` field → defaults to `true` → current auto-bind behavior preserved
- New RCA/audit/design dispatches explicitly pass `bind: false` → skip auto-bind
- No breaking change for any existing call site

### Caller-side discipline

Lead/general explicitly tag these dispatch classes as `bind: false`:
- Phase 1 RCA tasks (read-only investigation)
- Phase 2 reviewer challenge tasks
- Audit-style tasks (e.g. P1-B Bug 2 audit doc PR #518)
- Documentation-only PRs (e.g. P2-9 Sprint 48 fwdref cleanup #510, P2-7 flaky tests triage #520)
- Smoke verification tasks (e.g. Phase 5 smoke batch m-0)

Continue passing `bind: true` (or omitting) for:
- IMPL tasks
- Fix tasks
- Feature tasks
- Any task expected to commit/push to a feature branch

## §4 Rejected alternatives

### Variant B: Task-class taxonomy
- **Rejected for Sprint 55** scope reasons (~80-150 LOC + cross-cutting changes to task board + send + dispatch_hook + serde + CLI flags)
- **Reserved for Sprint 56+** if cross-cutting taxonomy proves valuable for other features (test-only filtering, observability tagging, telemetry buckets)
- Argument FOR (deferred): named classes are more expressive than bool; future extension easier
- Argument AGAINST (current): blast radius too large for single-bug fix; bool covers 95% of dogfood need

### Make auto-bind opt-IN via explicit `bind: true`
- **Rejected**: breaks 50+ existing dispatch sites (every IMPL dispatch would need flag added)
- Migration burden disproportionate to bug surface

### Always auto-bind + provide easy `release_worktree(on_completion=true)` hook
- **Rejected**: doesn't avoid initial worktree-create cost (5 min × N tasks still wasted)
- Treats symptom not cause

### Agent-side env var to disable auto-bind for current dispatch
- **Rejected**: agents shouldn't have to opt-out post-hoc (already received daemon-state by env-var-set time); should be caller-side directive

## §5 Implementation surface

### Code sites (approximate, dev refines during IMPL)

1. **`src/tasks.rs` (TaskRecord struct)** — add `bind: Option<bool>` field with `#[serde(default, skip_serializing_if = "Option::is_none")]`
2. **`src/mcp/handlers/comms.rs` (send handler)** — accept `bind` arg; propagate to task record OR direct to dispatch_hook
3. **`src/mcp/handlers/task.rs` (task create handler)** — accept `bind` arg in `task(action: "create", ...)`
4. **`src/mcp/handlers/dispatch_hook/mod.rs:21-83`** — read `task_record.bind` (or default true); branch on false → skip bind
5. **Tests** — unit tests for default-true (no field), explicit-false, explicit-true; mock dispatch hook + verify bind skip behavior

### Telemetry

INFO log when bind is skipped:
```
INFO dispatch_auto_bind_lease skipping auto-bind for task <task_id> (bind: false)
```

No log on default-true path (current behavior, no noise).

## §6 Edge case adjudication

### EC1 — Caller passes `bind: false` but later needs binding
- **Recommendation**: Caller can explicitly call `bind_self(...)` MCP tool from agent side. Dispatch-time skip doesn't preclude on-demand binding.
- **Rationale**: Auto-bind and explicit bind are independent code paths; skipping the former preserves the latter

### EC2 — Task pre-existing in board with `bind` field absent
- **Recommendation**: Default true (backward-compat); current behavior preserved
- Verified by serde `#[serde(default)]` + `Option::unwrap_or(true)` chain

### EC3 — Mixed bind + class semantics in future Variant B
- **Recommendation**: If Variant B added Sprint 56+, derive `bind` from `class` if both present (class wins as more specific)
- **Migration path**: Sprint 56 reads `class` field; falls back to `bind` for backward-compat with Variant A callers

### EC4 — Multiple agents sharing source_repo, mixed bind values
- **Recommendation**: Each agent's task record has independent `bind` field; no cross-agent coupling
- **Sprint 53 P0-1.5 lease conflict** still applies (only one agent can hold same-branch lease at a time)

### EC5 — Reviewer challenge on dispatch (Phase 2 task) needs read-only access to dev's parallel worktree
- **Recommendation**: Reviewer dispatches also `bind: false` (RCA/challenge class)
- Reviewer reads dev's worktree path via filesystem Read tool; no separate worktree binding needed

## §7 LOC + Tier estimate

| Component | LOC est | Notes |
|---|---|---|
| `TaskRecord.bind: Option<bool>` field | ~5 | Additive serde field |
| `task` create handler accept `bind` | ~5-10 | Plumbing arg → record |
| `send` handler propagate `bind` | ~5-10 | Plumbing arg → task creation |
| `dispatch_auto_bind_lease` early-return on `bind: false` | ~10-15 | `task_should_bind` helper + conditional |
| Tests | ~15-25 | Default + explicit-false + explicit-true paths |
| **Total** | **~40-65** | Within general's "small" expectation for this kind of feature flag |

**Tier**: Tier-1 single primary review (codex). No Tier-2 expected.

**LOC ceiling**: 80 nominal, 100 hard escalate.

## §8 Out of scope (deferred to Sprint 56+ or beyond)

- **Variant B task-class taxonomy** — full class enumeration + per-class dispatch behavior
- **Auto-`release_worktree` on task completion** — separate concern (binding lifecycle, not dispatch trigger)
- **CLI flag for `bind: false` in `agend-terminal task create`** — possible Sprint 56 ergonomics improvement
- **Heuristic auto-detect of doc-only tasks** — too magical; explicit `bind: false` preferred

## §9 Risks

| Risk | Severity | Mitigation |
|---|---|---|
| Caller forgets `bind: false` on RCA dispatch → wasted worktree | LOW | Lead orchestrator habit + dispatch templates (e.g. `delegate_rca_task` helper that defaults `bind: false`) |
| Variant A locks design before cross-cutting taxonomy needs surface | LOW | Sprint 56+ migration path documented (Variant B compatible with Variant A via class-wins-over-bind) |
| Existing 50+ dispatch sites accidentally depend on auto-bind triggering | LOW-MED | Default-true preserves current behavior; explicit opt-out only |
| Reviewer Phase 2 task can't access dev's parallel worktree if reviewer also `bind: false` | LOW | Reviewer reads via filesystem path (Read tool); doesn't need own binding |
| Telemetry INFO log on every `bind: false` dispatch creates log noise | LOW | INFO-level (not WARN); operator can filter at log-aggregation layer |

## §10 Implementation order

P0-C can land **independently of P0-A and P0-B** since it touches:
- Different code (task board + dispatch hook)
- No shared state with channel discipline (P0-A) or binding refactor (P0-B core)
- No conflict with proposed test matrices

**Recommended order**: 
1. P0-A first (smaller user-facing correctness, locks routing invariants)
2. P0-B core (binding refactor)
3. P0-C (auto-bind trigger scope filter)

OR P0-C parallel with P0-B (independent scopes).

## §11 Status / next steps

- **Phase 1 dev RCA**: Complete (P0-B EC10 section in dev's P0-B doc)
- **Phase 2 reviewer challenge**: Complete (split-verdict + Variant A vs B option)
- **Phase 3 lead synthesis**: This document (extracted as separate P0-C dispatch)
- **Operator review**: Pending m-3637 directive (Variant A vs B + Sprint 55 OR Sprint 56+ timing)
- **Phase 4 IMPL** (post-approval): dev primary, reviewer Tier-1, ~40-65 LOC, ~1hr cycle (Variant A)

**Decision required from operator**:
1. **Variant A vs B**: Variant A recommended for Sprint 55 ship cadence; Variant B deferred Sprint 56+
2. **Sprint 55 inclusion vs Sprint 56+ defer**: dogfood evidence justifies Sprint 55, but Sprint 55 already has P0-A + P0-B; capacity check
3. **Caller-side discipline policy**: which dispatch classes ALWAYS pass `bind: false`? (Suggested list in §3 above)

Sprint 55 capacity feasibility:
- P0-A ~70-110 LOC, ~1.5-2hr
- P0-B core ~210-335 LOC, ~3-4hr
- P0-C Variant A ~40-65 LOC, ~1hr
- **Total Sprint 55 P0**: ~320-510 LOC, ~5.5-7hr engineering (excluding review + CI)

Realistic for single-Sprint ship if dispatched sequentially per-PR with reviewer Tier-1 cycle.
